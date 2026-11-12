//! Daemon orchestration: egress loop, Codex relay, panel and shared state.

use std::fs::{self, OpenOptions};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::codex::proxy::{ProbeReport, ProxyRuntime, ProxyStatus};
use crate::config::{AppPaths, Config};
use crate::guard::{self, EnforcementReport, Verdict};
use crate::util;
use crate::web;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub pid: u32,
    pub started_at: String,
    pub status: Verdict,
    pub detail: String,
    pub checked_at: Option<String>,
    pub http_ip: Option<String>,
    pub stun_ip: Option<String>,
    pub detected: bool,
    pub allowed: bool,
    pub enforcement: EnforcementReport,
    pub codex: ProxyStatus,
    pub update_locked: bool,
    pub config_path: String,
    pub panel_url: String,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
    pub paths: AppPaths,
    pub runtime: Arc<RwLock<RuntimeState>>,
    pub proxy: ProxyRuntime,
    /// Lets an internal configuration transaction restart the daemon after
    /// replacing the generated Mihomo plan.
    pub shutdown: Arc<tokio::sync::Notify>,
    pub started_at: String,
    /// Last applied block/allow decision and its report. Reapplying `netsh`
    /// every three seconds would churn firewall rules for no reason.
    pub last_enforcement: Arc<std::sync::Mutex<Option<(bool, EnforcementReport)>>>,
}

impl AppState {
    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn runtime(&self) -> RuntimeState {
        self.runtime.read().unwrap().clone()
    }

    pub fn save_config(&self, cfg: &Config) -> Result<()> {
        self.proxy.check_reload(cfg)?;
        cfg.save(&self.paths)?;
        *self.config.write().unwrap() = cfg.clone();
        self.proxy.reload(cfg.clone())?;
        Ok(())
    }

    pub async fn check_now(&self) -> RuntimeState {
        evaluate_once(self, true).await
    }

    pub fn write_state(&self) {
        let state = self.runtime();
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            if let Err(err) = crate::config::atomic_write(&self.paths.state_file, text.as_bytes()) {
                util::warn(format!("write state: {err:#}"));
            }
        }
    }

    pub fn doctor(&self) -> Vec<(String, bool, String)> {
        let cfg = self.config();
        let mut rows = guard::doctor(&cfg);
        let proxy = self.proxy.status();
        rows.push((
            "Codex 转发".into(),
            proxy.config_matches,
            format!("{} / {}", proxy.listen, proxy.detail),
        ));
        rows
    }

    pub fn proxy_status(&self) -> ProxyStatus {
        self.proxy.status()
    }

    pub async fn probe_codex(&self) -> ProbeReport {
        self.proxy.probe_now().await
    }

    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }
}

pub async fn run(config: Config, paths: AppPaths, open_panel: bool) -> Result<()> {
    paths.ensure()?;
    util::init_logging(&paths.log_dir)?;
    let _lock = acquire_instance_lock(&paths)?;

    let cfg = config.clone();
    let proxy = ProxyRuntime::new(cfg.clone(), paths.clone())?;
    if !cfg.protection.enabled && cfg.protection.network_guard {
        // A previous daemon may have been force-killed while blocked. Clear
        // GPTGuard's own hosts/firewall entries on a clean disabled start.
        let report = guard::enforce(&cfg, false);
        if !report.errors.is_empty() {
            util::warn(format!(
                "startup cleanup could not remove network block: {}",
                report.errors.join("; ")
            ));
        }
    }
    let started_at = util::now_iso();
    let panel_url = format!("http://127.0.0.1:{}", cfg.ui.web_port);
    let runtime = RuntimeState {
        pid: std::process::id(),
        started_at: started_at.clone(),
        status: if cfg.protection.enabled {
            Verdict::Offline
        } else {
            Verdict::Disabled
        },
        detail: "守护启动中".into(),
        checked_at: None,
        http_ip: None,
        stun_ip: None,
        detected: false,
        allowed: false,
        enforcement: EnforcementReport::default(),
        codex: proxy.status(),
        update_locked: cfg.update_lock.enabled,
        config_path: paths.config_file.display().to_string(),
        panel_url: panel_url.clone(),
    };
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let state = AppState {
        config: Arc::new(RwLock::new(cfg.clone())),
        paths: paths.clone(),
        runtime: Arc::new(RwLock::new(runtime)),
        proxy: proxy.clone(),
        shutdown: shutdown.clone(),
        started_at,
        last_enforcement: Arc::new(std::sync::Mutex::new(None)),
    };
    state.write_state();

    let guard_state = state.clone();
    let guard_task = tokio::spawn(async move {
        guard_loop(guard_state).await;
    });

    let (proxy_tx, proxy_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_state = proxy.clone();
    let proxy_task = tokio::spawn(async move {
        if let Err(err) = proxy_state
            .serve(async {
                let _ = proxy_rx.await;
            })
            .await
        {
            util::error(format!("Codex relay stopped: {err:#}"));
        }
    });

    let (web_tx, web_rx) = tokio::sync::oneshot::channel::<()>();
    let web_state = state.clone();
    let web_task = tokio::spawn(async move {
        if let Err(err) = web::serve(web_state, async {
            let _ = web_rx.await;
        })
        .await
        {
            util::error(format!("management panel stopped: {err:#}"));
        }
    });

    util::info(format!("GPTGuard daemon started; panel {panel_url}"));
    if open_panel || cfg.ui.open_panel_on_start {
        let _ = util::open_url(&panel_url);
    }

    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("wait Ctrl+C")?;
        }
        _ = shutdown.notified() => {
            util::info("shutdown requested by the native configuration flow");
        }
    }
    util::info("shutting down");
    let _ = proxy_tx.send(());
    let _ = web_tx.send(());
    guard_task.abort();
    proxy_task.abort();
    web_task.abort();
    proxy.shutdown();

    // Do not leave a dead daemon's block behind. Update locking is separate and
    // remains until the user turns it off explicitly.
    let latest = state.config();
    if latest.protection.proxy_only {
        util::warn("proxy_only 模式：守护停止后保留防火墙阻断；关闭防护后重启可解除");
    } else if latest.protection.network_guard {
        let _ = guard::enforce(&latest, false);
    }
    let mut state_now = state.runtime();
    state_now.status = Verdict::Stopped;
    state_now.codex = proxy.status();
    state_now.detail = "守护已停止".into();
    *state.runtime.write().unwrap() = state_now.clone();
    state.write_state();
    Ok(())
}

async fn guard_loop(state: AppState) {
    loop {
        let cfg = state.config();
        if cfg.protection.enabled {
            evaluate_once(&state, false).await;
        } else {
            mark_disabled(&state, &cfg);
        }
        tokio::time::sleep(Duration::from_secs(cfg.protection.interval_seconds.max(1))).await;
    }
}

fn apply_enforcement(
    state: &AppState,
    cfg: &Config,
    block: bool,
    force: bool,
) -> EnforcementReport {
    let mut cache = state.last_enforcement.lock().unwrap();
    let unchanged = cache
        .as_ref()
        .map(|(previous, _)| *previous == block)
        .unwrap_or(false);
    if !force && unchanged {
        return cache.as_ref().unwrap().1.clone();
    }
    let report = guard::enforce(cfg, block);
    *cache = Some((block, report.clone()));
    report
}

fn mark_disabled(state: &AppState, cfg: &Config) {
    {
        let mut cache = state.last_enforcement.lock().unwrap();
        if cache.as_ref().is_some_and(|(blocked, _)| *blocked) {
            let _ = guard::enforce(cfg, false);
        }
        *cache = None;
    }
    let mut runtime = state.runtime();
    runtime.status = Verdict::Disabled;
    runtime.detected = false;
    runtime.allowed = false;
    runtime.detail = "防护已关闭".into();
    runtime.checked_at = Some(util::now_iso());
    runtime.enforcement = EnforcementReport {
        elevated: util::is_elevated(),
        ..Default::default()
    };
    runtime.codex = state.proxy.status();
    runtime.update_locked = cfg.update_lock.enabled;
    *state.runtime.write().unwrap() = runtime;
    state.write_state();
}

async fn evaluate_once(state: &AppState, manual: bool) -> RuntimeState {
    let cfg = state.config();
    if !cfg.protection.enabled && !manual {
        mark_disabled(state, &cfg);
        return state.runtime();
    }
    let started = std::time::Instant::now();
    let report = state.proxy.check_egress(&cfg).await;
    // A manual wizard check is allowed to probe an exit before the user turns
    // protection on, but it must never install a block while protection is
    // disabled.  Background checks remain strict and fail closed.
    let block = cfg.protection.enabled && (cfg.protection.proxy_only || !report.allowed);
    let enforcement = apply_enforcement(state, &cfg, block, manual);
    if cfg.protection.proxy_only && !report.allowed && cfg.protection.kill_on_block {
        // Keep the app from continuing with a direct connection while the
        // firewall rule is being refreshed.
        let killed = guard::target_paths(&cfg);
        let _ = util::kill_processes(&killed);
    }
    let mut status = if !cfg.protection.enabled {
        if report.allowed {
            Verdict::Allowed
        } else if report.detected {
            Verdict::Blocked
        } else {
            Verdict::Offline
        }
    } else if report.allowed {
        Verdict::Allowed
    } else if report.detected {
        Verdict::Blocked
    } else {
        Verdict::Offline
    };
    if !enforcement.errors.is_empty() && (status != Verdict::Allowed || cfg.protection.proxy_only) {
        status = Verdict::Error;
    }
    let mut detail = report.detail.clone();
    if !enforcement.errors.is_empty() {
        detail = format!("{detail}；执行层: {}", enforcement.errors.join("; "));
    }
    if manual {
        detail = format!("{detail}（手动检测）");
    }
    let codex = state.proxy.status();
    let mut runtime = state.runtime();
    runtime.status = status;
    runtime.detail = detail;
    runtime.checked_at = Some(util::now_iso());
    runtime.http_ip = report.http_ip.map(|ip| ip.to_string());
    runtime.stun_ip = report.stun_ip.map(|ip| ip.to_string());
    runtime.detected = report.detected;
    runtime.allowed = report.allowed;
    runtime.enforcement = enforcement;
    runtime.codex = codex;
    runtime.update_locked = cfg.update_lock.enabled;
    runtime.pid = std::process::id();
    if manual {
        runtime.detail = format!(
            "{}（检测耗时 {} ms）",
            runtime.detail,
            started.elapsed().as_millis()
        );
    }
    *state.runtime.write().unwrap() = runtime.clone();
    state.write_state();
    runtime
}

pub fn daemon_running(paths: &AppPaths) -> bool {
    let Ok(file) = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&paths.lock_file)
    else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(_) => true,
    }
}

pub fn read_state(paths: &AppPaths) -> Option<RuntimeState> {
    let raw = fs::read_to_string(&paths.state_file).ok()?;
    let mut state: RuntimeState = serde_json::from_str(&raw).ok()?;
    if !daemon_running(paths) {
        state.status = Verdict::Stopped;
        state.codex.mihomo.ready = false;
        state.codex.mihomo.pid = None;
        state.codex.mihomo.detail = "守护未运行".into();
        state.detail = "守护未运行（状态文件来自已退出的进程）".into();
        state.http_ip = None;
        state.stun_ip = None;
        state.detected = false;
        state.allowed = false;
        state.codex.detail = "守护未运行".into();
        state.codex.sessions = 0;
    }
    Some(state)
}

fn acquire_instance_lock(paths: &AppPaths) -> Result<fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&paths.lock_file)
        .with_context(|| format!("open lock {}", paths.lock_file.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(_) => bail!("GPTGuard 守护已经在运行（{}）", paths.lock_file.display()),
    }
}

pub fn status_text(state: &RuntimeState) -> String {
    let label = match state.status {
        Verdict::Allowed => "允许",
        Verdict::Blocked => "已阻断",
        Verdict::Offline => "无法确认出口",
        Verdict::Disabled => "防护关闭",
        Verdict::Error => "异常",
        Verdict::Stopped => "未运行",
    };
    format!(
        "{label} | HTTP {} | STUN {} | Codex {}",
        state.http_ip.as_deref().unwrap_or("-"),
        state.stun_ip.as_deref().unwrap_or("-"),
        state.codex.detail
    )
}
