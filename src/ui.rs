//! Native status and first-run window. The daemon owns the protection state.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde_json::{json, Value};
use slint::ComponentHandle;

use crate::config::{AppPaths, Config};
use crate::{service, util};

slint::include_modules!();

#[cfg(test)]
mod tests;

#[derive(Default)]
struct ActionResult {
    message: String,
    candidate: Option<(String, String)>,
    activate_after_trust: bool,
}

impl ActionResult {
    fn message(text: impl Into<String>) -> Self {
        Self {
            message: text.into(),
            ..Default::default()
        }
    }
}

#[derive(Clone)]
struct Ui {
    paths: Arc<AppPaths>,
    window: slint::Weak<MainWindow>,
    revision: Arc<AtomicU64>,
    polling: Arc<AtomicBool>,
}

impl Ui {
    // Called on the UI thread. Disable conflicting actions synchronously,
    // before a second click can schedule another configuration change.
    fn action(
        &self,
        progress: &str,
        run: impl FnOnce(&AppPaths) -> Result<ActionResult> + Send + 'static,
    ) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if window.get_operation_busy() {
            return;
        }
        self.revision.fetch_add(1, Ordering::Relaxed);
        window.set_operation_busy(true);
        window.set_action_failed(false);
        window.set_action_text(progress.into());
        window.set_trust_candidate_ip("".into());
        let ui = self.clone();
        std::thread::spawn(move || {
            let result = run(&ui.paths);
            let snapshot = fetch_status(&ui.paths);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = ui.window.upgrade() else {
                    return;
                };
                match &result {
                    Ok(outcome) => {
                        window.set_action_text(outcome.message.as_str().into());
                        if let Some((proxy, ip)) = &outcome.candidate {
                            window.set_trust_candidate_proxy(proxy.as_str().into());
                            window.set_trust_candidate_ip(ip.as_str().into());
                            window.set_activate_after_trust(outcome.activate_after_trust);
                        }
                    }
                    Err(err) => {
                        window.set_action_failed(true);
                        window.set_action_text(format!("{err:#}").into());
                    }
                }
                // A status refresh owns the state label, never the action result.
                show_status(&window, snapshot);
                window.set_operation_busy(false);
            });
        });
    }

    fn poll(&self) {
        if self.window.upgrade().is_none_or(|w| w.get_operation_busy())
            || self.polling.swap(true, Ordering::Relaxed)
        {
            return;
        }
        let revision = self.revision.load(Ordering::Relaxed);
        let ui = self.clone();
        std::thread::spawn(move || {
            let snapshot = fetch_status(&ui.paths);
            let _ = slint::invoke_from_event_loop(move || {
                ui.polling.store(false, Ordering::Relaxed);
                if ui.revision.load(Ordering::Relaxed) == revision {
                    if let Some(window) = ui.window.upgrade() {
                        show_status(&window, snapshot);
                    }
                }
            });
        });
    }
}

pub fn run() -> Result<()> {
    let paths = AppPaths::discover();
    paths.ensure()?;
    util::init_logging(&paths.log_dir)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.data_dir.join("ui.lock"))
        .context("open UI lock")?;
    if lock_file.try_lock_exclusive().is_err() {
        // An explicitly elevated second launch can supply the worker to the
        // existing window. An active guard is never restarted for elevation.
        if cfg!(windows) && util::is_elevated() {
            upgrade_idle_daemon(&paths, &fetch_status(&paths)?)?;
        }
        return Ok(());
    }
    let window = MainWindow::new().context("create native window")?;
    window.set_about_text(
        concat!(
            "GPTGuard v",
            env!("CARGO_PKG_VERSION"),
            "  ·  内置 Mihomo v1.19.34（GPL-3.0）"
        )
        .into(),
    );
    let ui = Ui {
        paths: Arc::new(paths),
        window: window.as_weak(),
        revision: Default::default(),
        polling: Default::default(),
    };
    bind_actions(&window, &ui);
    ui.action("正在连接 GPTGuard 后台…", |paths| {
        ensure_daemon(paths)?;
        Ok(ActionResult::default())
    });
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(3),
        move || ui.poll(),
    );
    window.run().context("run native window")
}

fn bind_actions(window: &MainWindow, ui: &Ui) {
    let weak = window.as_weak();
    window.on_cancel_confirmation(move || {
        if let Some(window) = weak.upgrade() {
            window.set_trust_candidate_ip("".into());
            window.set_action_text("已取消出口确认。".into());
        }
    });
    let action = ui.clone();
    window.on_refresh_clicked(move || {
        action.action("正在检测实际出口…", |paths| {
            check_exit(paths, false)
        })
    });
    let action = ui.clone();
    window.on_connect_local_clicked(move || {
        action.action("正在查找 Clash Verge 并检测出口…", |paths| {
            connect_and_check(paths, false)
        })
    });
    let action = ui.clone();
    window.on_protection_clicked(move |enabled| {
        action.action(
            if enabled {
                "正在检查开启条件与出口…"
            } else {
                "正在关闭严格保护…"
            },
            move |paths| match change_protection(paths, enabled) {
                Err(err) if enabled && api_code(&err) == Some("whitelist_required") => {
                    connect_and_check(paths, true)
                }
                result => result,
            },
        );
    });
    let action = ui.clone();
    window.on_trust_local_exit_clicked(move |ip, proxy, enable| {
        let (ip, proxy) = (ip.to_string(), proxy.to_string());
        action.action("正在重新核对出口 IP…", move |paths| {
            post_api(
                paths,
                "/api/codex/trust-local-exit",
                json!({"ip": ip, "proxy": proxy}),
            )?;
            if enable {
                change_protection(paths, true)
                    .with_context(|| format!("{ip} 已加入白名单，开启严格保护失败"))
            } else {
                Ok(ActionResult::message(format!("已信任出口 {ip}。")))
            }
        });
    });
    let action = ui.clone();
    window.on_add_exit_clicked(move |entry| {
        let entry = entry.trim().to_string();
        action.action("正在保存出口白名单…", move |paths| {
            post_api(
                paths,
                "/api/whitelist",
                json!({"entries":[entry], "append":true}),
            )?;
            Ok(ActionResult::message(format!(
                "已将 {entry} 加入出口白名单。"
            )))
        });
    });
    let action = ui.clone();
    window.on_import_clicked(move |subscription| {
        let url = subscription.trim().to_string();
        action.action("正在导入订阅并检测出口…", move |paths| {
            let response = post_api(paths, "/api/codex/subscription", json!({"url": url}))?;
            restart_if_requested(paths, &response)?;
            check_exit(paths, false)
        });
    });
    let action = ui.clone();
    window.on_file_clicked(move || {
        let Some(file) = rfd::FileDialog::new()
            .add_filter("节点文件", &["yaml", "yml", "txt"])
            .pick_file()
        else {
            return;
        };
        action.action("正在导入文件并检测出口…", move |paths| {
            let content = std::fs::read_to_string(&file).context("读取节点文件")?;
            let response = post_api(
                paths,
                "/api/codex/import-file",
                json!({
                    "name": file.file_name().and_then(|name| name.to_str()).unwrap_or("节点文件"),
                    "content": content,
                }),
            )?;
            restart_if_requested(paths, &response)?;
            check_exit(paths, false)
        });
    });
    let action = ui.clone();
    window.on_takeover_clicked(move || {
        action.action("正在接管 Codex 配置…", |paths| {
            post_api(paths, "/api/codex/takeover", json!({"auth": "unknown"}))?;
            Ok(ActionResult::message("Codex 配置已接管。"))
        })
    });
    let action = ui.clone();
    window.on_restore_clicked(move || {
        action.action("正在恢复 Codex 配置…", |paths| {
            post_api(paths, "/api/codex/restore", json!({"force": true}))?;
            Ok(ActionResult::message(
                "Codex 原配置已恢复。如有冲突，已保留副本。",
            ))
        })
    });
}

fn change_protection(paths: &AppPaths, enabled: bool) -> Result<ActionResult> {
    let response = post_api(paths, "/api/protection", json!({"enabled": enabled}))?;
    if response["enabled"].as_bool() != Some(enabled) {
        bail!("后台未确认保护状态。请从通知区退出 GPTGuard 后重新启动。");
    }
    Ok(ActionResult::message(if enabled {
        "已开启严格保护。"
    } else {
        "已关闭严格保护。"
    }))
}

fn connect_and_check(paths: &AppPaths, activate: bool) -> Result<ActionResult> {
    let response = post_api(paths, "/api/codex/connect-local", json!({})).map_err(|err| {
        if activate {
            err.context("开启保护前需要确认出口。请启动 Clash Verge 后重试，或在“其他连接方式”中填写已知的出口 IP")
        } else { err }
    })?;
    restart_if_requested(paths, &response)?;
    check_exit(paths, activate)
}

fn check_exit(paths: &AppPaths, activate: bool) -> Result<ActionResult> {
    let check = post_api(paths, "/api/check", json!({}))?;
    let candidate = &check["trust_candidate"];
    if let (Some(proxy), Some(ip)) = (candidate["proxy"].as_str(), candidate["ip"].as_str()) {
        return Ok(ActionResult {
            message: format!("检测到出口 {ip}。请核对它是否属于你信任的节点。"),
            candidate: Some((proxy.into(), ip.into())),
            activate_after_trust: activate,
        });
    }
    let detail = check["runtime"]["detail"]
        .as_str()
        .unwrap_or("后台没有返回检测结果");
    if !check["runtime"]["detected"].as_bool().unwrap_or(false) {
        bail!("出口检测失败。请检查代理连接并重试。{detail}");
    }
    if activate {
        bail!("尚未确认可信出口。请重新检测，或在“其他连接方式”中填写已知的出口 IP。");
    }
    Ok(ActionResult::message(detail))
}

fn ensure_daemon(paths: &AppPaths) -> Result<()> {
    // A tray may already own its lock even if its worker stopped. Launch the
    // worker with the window's token and wait for its authenticated API first.
    if !service::daemon_running(paths) {
        util::spawn_detached(&util::exe_path(), &["--cli", "run"])?;
    }
    let status = wait_for_api(paths)?;
    util::spawn_detached(&util::exe_path(), &["--cli", "tray", "--start-guard"])?;
    upgrade_idle_daemon(paths, &status)
}

fn upgrade_idle_daemon(paths: &AppPaths, status: &Value) -> Result<()> {
    if cfg!(windows)
        && util::is_elevated()
        && status["daemon_elevated"] == false
        && status["config"]["protection"]["enabled"] == false
    {
        let response = post_api(paths, "/api/restart-idle", json!({}))?;
        restart_if_requested(paths, &response)?;
        anyhow::ensure!(
            fetch_status(paths)?["daemon_elevated"] == true,
            "后台仍以普通权限运行。请从通知区退出 GPTGuard 后，以管理员身份重新启动。"
        );
    }
    Ok(())
}

fn wait_for_api(paths: &AppPaths) -> Result<Value> {
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < until {
        if service::daemon_running(paths) {
            if let Ok(snapshot) = fetch_status(paths) {
                return Ok(snapshot);
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    bail!("连接后台超时。请从通知区退出 GPTGuard 后重新启动。");
}

fn restart_if_requested(paths: &AppPaths, response: &Value) -> Result<()> {
    if response["restart_required"] != true {
        return Ok(());
    }
    for _ in 0..80 {
        if !service::daemon_running(paths) {
            util::spawn_detached(&util::exe_path(), &["--cli", "run"])?;
            wait_for_api(paths)?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    bail!("等待后台重启超时。请从通知区退出 GPTGuard 后重新启动。");
}

fn status_label(enabled: bool, runtime: &Value) -> (&'static str, bool, bool) {
    if !enabled {
        return if runtime["status"] == "error" {
            ("保护已关闭，规则待清理", false, true)
        } else {
            ("尚未开启保护", false, false)
        };
    }
    match runtime["status"].as_str() {
        Some("error") => ("保护执行异常", false, true),
        Some("blocked") => ("已阻断", false, true),
        Some("offline") if !runtime["checked_at"].is_null() => ("无法确认出口", false, true),
        Some("allowed") if runtime["allowed"] == true => ("已保护", true, false),
        _ => ("正在核验出口", false, false),
    }
}

fn changes(value: &Value) -> String {
    value["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("，")
}

fn show_status(window: &MainWindow, snapshot: Result<Value>) {
    let snapshot = snapshot.and_then(|value| {
        anyhow::ensure!(
            value["daemon_elevated"].is_boolean(),
            "后台版本较旧。请关闭主窗口，从通知区退出 GPTGuard，再启动新版。"
        );
        Ok(value)
    });
    let value = match snapshot {
        Ok(value) => value,
        Err(err) => {
            window.set_state_known(false);
            window.set_status_text("后台未就绪".into());
            window.set_detail_text(format!("{err:#}").into());
            window.set_protection_healthy(false);
            window.set_protection_problem(true);
            window.set_trust_candidate_ip("".into());
            return;
        }
    };
    let protection = &value["config"]["protection"];
    let runtime = &value["runtime"];
    let enabled = protection["enabled"] == true;
    let (label, healthy, problem) = status_label(enabled, runtime);
    let matches = value["proxy"]["config_matches"] == true;
    window.set_state_known(true);
    window.set_protection_enabled(enabled);
    window.set_protection_healthy(healthy);
    window.set_protection_problem(problem);
    window.set_status_text(
        if healthy && !matches {
            "出口已通过，Codex 待接管"
        } else {
            label
        }
        .into(),
    );
    let whitelist = protection["whitelist"].as_array().map_or(0, Vec::len);
    let detail = if healthy && !matches {
        "严格保护已开启。要让 Codex 使用这个出口，请在下方确认接管配置。".to_string()
    } else if enabled || problem {
        runtime["detail"].as_str().unwrap_or("正在核验出口").into()
    } else if whitelist == 0 {
        "首次开启会查找 Clash Verge，并请你确认检测到的出口 IP。".to_string()
    } else {
        format!("已保存 {whitelist} 条出口白名单规则。")
    };
    window.set_detail_text(detail.into());
    let local = value["local_proxy"].as_str();
    if whitelist != 0 || local != Some(window.get_trust_candidate_proxy().as_str()) {
        window.set_trust_candidate_ip("".into());
    }
    let route = if let Some(proxy) = local {
        format!("Clash Verge  ·  {}", proxy.trim_end_matches('/'))
    } else if value["proxy"]["mihomo"]["enabled"] == true {
        value["proxy"]["mihomo"]["detail"]
            .as_str()
            .unwrap_or("使用内置 Mihomo")
            .into()
    } else {
        let count = value["config"]["codex"]["route_count"]
            .as_u64()
            .unwrap_or(0);
        if count == 0 {
            "尚未配置代理出口".into()
        } else {
            format!("已配置 {count} 个出口")
        }
    };
    window.set_route_text(route.into());
    let legacy = value["legacy_provider"]["needed"] == true;
    window.set_setup_needed(legacy || !matches);
    window.set_takeover_label(
        if legacy {
            "修复 Codex"
        } else {
            "确认接管配置"
        }
        .into(),
    );
    let codex = if legacy {
        format!("旧版配置需要修复。{}", changes(&value["legacy_provider"]))
    } else if matches {
        "Codex 已连接本地网关。".into()
    } else {
        format!(
            "接管后将应用以下改动。{}",
            changes(&value["takeover_preview"])
        )
    };
    window.set_codex_text(codex.into());
    let doctor = value["doctor"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let name = row["name"].as_str()?;
            let state = if row["ok"] == true {
                "通过"
            } else if name == "登录自启动" {
                "未启用（可选）"
            } else {
                "待处理"
            };
            Some(format!(
                "{name} · {state}    {}",
                row["detail"].as_str().unwrap_or("")
            ))
        })
        .collect::<Vec<_>>()
        .join("\n");
    window.set_doctor_text(doctor.into());
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ApiFailure {
    code: String,
    message: String,
}

fn api_code(err: &anyhow::Error) -> Option<&str> {
    err.downcast_ref::<ApiFailure>()
        .map(|failure| failure.code.as_str())
}

fn fetch_status(paths: &AppPaths) -> Result<Value> {
    request(paths, "/api/status", None)
}

fn post_api(paths: &AppPaths, endpoint: &str, body: Value) -> Result<Value> {
    request(paths, endpoint, Some(body))
}

fn request(paths: &AppPaths, endpoint: &str, body: Option<Value>) -> Result<Value> {
    let config = Config::load_or_create(paths)?;
    let url = format!("http://127.0.0.1:{}{endpoint}", config.ui.web_port);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(if body.is_some() { 90 } else { 5 }))
            .build()?;
        let request = match body {
            None => client.get(url),
            Some(body) => client.post(url).json(&body),
        };
        let response = request
            .header("x-gptguard-token", &config.ui.admin_token)
            .send()
            .await
            .context("无法连接 GPTGuard 后台")?;
        let status = response.status();
        let value: Value = response.json().await.context("读取后台响应失败")?;
        if !status.is_success() {
            return Err(ApiFailure {
                code: value["error"]["code"].as_str().unwrap_or("").into(),
                message: value["error"]["message"]
                    .as_str()
                    .unwrap_or("后台拒绝了请求")
                    .into(),
            }
            .into());
        }
        Ok(value)
    })
}
