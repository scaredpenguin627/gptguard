//! Command line surface.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::codex::config_tx;
use crate::codex::provider;
use crate::codex::proxy::ProxyRuntime;
use crate::config::{parse_net, AppPaths, Config};
use crate::guard;
use crate::ipcheck;
use crate::service;
use crate::util;

#[derive(Parser)]
#[command(
    name = "gptguard",
    version,
    about = "GPTGuard / GPTGuard: 出口守卫与 Codex turn-state 转发"
)]
pub struct Cli {
    /// Running `gptguard.exe` without arguments starts the tray, which is the
    /// double-click path for non-CLI users.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 创建配置并显示下一步
    Setup,
    /// 启动守护（出口检测 + Codex 转发 + 管理面板）
    Run {
        #[arg(long)]
        open_panel: bool,
    },
    /// 停止守护进程（不退出托盘）
    Stop,
    /// 启动 Windows 托盘（会确保守护在运行）
    Tray {
        #[arg(long)]
        start_guard: bool,
    },
    /// 查看守护状态
    Status,
    /// 立即检测出口
    Check,
    /// 环境与配置诊断
    Doctor,
    /// 打开管理面板
    Web,
    AddIp {
        ip: String,
    },
    RemoveIp {
        ip: String,
    },
    ListIps,
    /// 修改配置：ip-check-mode/interval/model/fallback/account-mode/fixed-model/network-guard/kill-on-block/http-proxy
    Set {
        key: String,
        value: String,
    },
    /// 固定 Codex 模型并写入已接管的配置
    Model {
        model: String,
    },
    /// 模型目录：查看、添加、移除；它只影响校准分类，不是访问白名单
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// state 形状：查看、添加、移除未来模型的块数
    State {
        #[command(subcommand)]
        action: StateAction,
    },
    /// Codex 配置接管
    Codex {
        #[command(subcommand)]
        action: CodexAction,
    },
    /// 自动更新锁定
    UpdateLock {
        #[arg(value_enum)]
        action: OnOff,
    },
    /// 登录自启动
    Autostart {
        #[arg(value_enum)]
        action: OnOff,
    },
    /// 先检测出口，再启动命令；运行中出口失配会终止子进程
    Guard {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ModelsAction {
    List,
    Add { model: String },
    Remove { model: String },
    Reset,
}

#[derive(Subcommand)]
enum StateAction {
    List,
    AddBlock { blocks: usize },
    RemoveBlock { blocks: usize },
}

#[derive(Subcommand)]
enum CodexAction {
    Takeover {
        #[arg(long, default_value = "unknown")]
        auth: String,
        #[arg(long)]
        profile: Option<String>,
    },
    Restore {
        #[arg(long)]
        force: bool,
    },
    Status,
    Probe,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover();
    paths.ensure()?;
    let command = cli.command.unwrap_or(Command::Tray { start_guard: true });
    match command {
        Command::Setup => setup(paths),
        Command::Run { open_panel } => {
            let config = Config::load_or_create(&paths)?;
            block_on(service::run(config, paths, open_panel))
        }
        Command::Stop => stop(&paths),
        Command::Tray { start_guard } => crate::tray::run(start_guard),
        Command::Status => status(&paths),
        Command::Check => block_on(check(&paths)),
        Command::Doctor => doctor(&paths),
        Command::Web => open_panel(&paths),
        Command::AddIp { ip } => add_ip(&paths, &ip, true),
        Command::RemoveIp { ip } => add_ip(&paths, &ip, false),
        Command::ListIps => list_ips(&paths),
        Command::Set { key, value } => set(&paths, &key, &value),
        Command::Model { model } => set_model(&paths, &model),
        Command::Models { action } => models(&paths, action),
        Command::State { action } => state(&paths, action),
        Command::Codex { action } => block_on(codex(&paths, action)),
        Command::UpdateLock { action } => update_lock(&paths, action),
        Command::Autostart { action } => autostart(&paths, action),
        Command::Guard { command } => block_on(guarded(command, paths)),
    }
}

fn block_on<F: std::future::Future<Output = Result<()>>>(future: F) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create tokio runtime")?;
    runtime.block_on(future)
}

fn setup(paths: AppPaths) -> Result<()> {
    let mut config = Config::load_or_create(&paths)?;
    if config.protection.targets.is_empty() {
        config.protection.targets = crate::config::detect_targets();
        config.save(&paths)?;
    }
    println!("配置: {}", paths.config_file.display());
    println!("数据: {}", paths.data_dir.display());
    println!("面板: http://127.0.0.1:{}", config.ui.web_port);
    println!("管理口令: {}", config.ui.admin_token);
    println!();
    println!("下一步:");
    println!("  1. gptguard add-ip <你的代理出口 IP 或 CIDR>");
    println!("  2. gptguard set ip-check-mode both|http|stun");
    println!("  3. gptguard codex takeover --auth chatgpt");
    println!("  4. gptguard run        # 或 gptguard tray");
    println!();
    println!("目标程序:");
    for target in &config.protection.targets {
        println!("  - {}: {}", target.name, target.path);
    }
    Ok(())
}

fn stop(paths: &AppPaths) -> Result<()> {
    let Some(state) = service::read_state(paths) else {
        println!("守护未运行");
        return Ok(());
    };
    if !service::daemon_running(paths) {
        println!("守护未运行");
        return Ok(());
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &state.pid.to_string(), "/T", "/F"])
            .status()
            .context("run taskkill")?;
        if !status.success() {
            bail!("停止守护失败（PID {}）；请用管理员终端重试", state.pid);
        }
    }
    #[cfg(not(windows))]
    {
        let status = std::process::Command::new("kill")
            .args(["-TERM", &state.pid.to_string()])
            .status()
            .context("run kill")?;
        if !status.success() {
            bail!("停止守护失败（PID {}）", state.pid);
        }
    }
    println!("已停止守护（PID {}）", state.pid);
    Ok(())
}

fn status(paths: &AppPaths) -> Result<()> {
    let config = Config::load_or_create(paths)?;
    match service::read_state(paths) {
        Some(state) => {
            println!("{}", service::status_text(&state));
            println!("面板: http://127.0.0.1:{}", config.ui.web_port);
            println!("配置: {}", state.config_path);
            if !state.enforcement.errors.is_empty() {
                println!("执行层: {}", state.enforcement.errors.join("; "));
            }
            Ok(())
        }
        None => {
            println!("守护未运行（没有 state.json）");
            println!("启动: gptguard run 或 gptguard tray");
            Ok(())
        }
    }
}

async fn check(paths: &AppPaths) -> Result<()> {
    let config = Config::load_or_create(paths)?;
    let report = ipcheck::check(&config).await;
    println!("模式: {:?}", report.mode);
    println!(
        "HTTP: {}",
        report
            .http_ip
            .map(|ip| ip.to_string())
            .unwrap_or("-".into())
    );
    println!(
        "STUN: {}",
        report
            .stun_ip
            .map(|ip| ip.to_string())
            .unwrap_or("-".into())
    );
    println!("detected={} allowed={}", report.detected, report.allowed);
    println!("{}", report.detail);
    if !report.allowed {
        bail!("出口检测未通过");
    }
    Ok(())
}

fn doctor(paths: &AppPaths) -> Result<()> {
    let config = Config::load_or_create(paths)?;
    let mut rows = guard::doctor(&config);
    let proxy = service::read_state(paths).map(|state| state.codex);
    rows.push((
        "Codex 转发".into(),
        proxy.as_ref().map(|p| p.config_matches).unwrap_or(false),
        proxy
            .map(|p| format!("{} / {}", p.listen, p.detail))
            .unwrap_or_else(|| "未运行".into()),
    ));
    let mut bad = 0;
    for (name, ok, detail) in rows {
        println!("[{}] {}: {}", if ok { "OK" } else { "--" }, name, detail);
        if !ok {
            bad += 1;
        }
    }
    if bad > 0 {
        println!("\n{bad} 项待处理；管理员权限与目标程序路径是常见原因。");
    }
    Ok(())
}

fn open_panel(paths: &AppPaths) -> Result<()> {
    let config = Config::load_or_create(paths)?;
    let url = format!(
        "http://127.0.0.1:{}/#token={}",
        config.ui.web_port, config.ui.admin_token
    );
    util::open_url(&url)
}

fn add_ip(paths: &AppPaths, value: &str, add: bool) -> Result<()> {
    let mut config = Config::load_or_create(paths)?;
    let normalized = value.trim().to_string();
    if add {
        parse_net(&normalized)?;
        if !config.protection.whitelist.contains(&normalized) {
            config.protection.whitelist.push(normalized.clone());
        }
    } else {
        config
            .protection
            .whitelist
            .retain(|entry| entry != &normalized);
    }
    config.save(paths)?;
    println!(
        "{} {}（当前 {} 条）",
        if add { "已添加" } else { "已移除" },
        normalized,
        config.protection.whitelist.len()
    );
    Ok(())
}

fn list_ips(paths: &AppPaths) -> Result<()> {
    let config = Config::load_or_create(paths)?;
    if config.protection.whitelist.is_empty() {
        println!("白名单为空（按失败处理）");
    }
    for entry in config.protection.whitelist {
        println!("{entry}");
    }
    Ok(())
}

fn set(paths: &AppPaths, key: &str, value: &str) -> Result<()> {
    let mut config = Config::load_or_create(paths)?;
    match key {
        "ip-check-mode" => {
            config.protection.ip_check_mode = match value {
                "both" => crate::config::IpCheckMode::Both,
                "http" => crate::config::IpCheckMode::Http,
                "stun" => crate::config::IpCheckMode::Stun,
                _ => bail!("ip-check-mode must be both, http or stun"),
            };
        }
        "interval" => {
            config.protection.interval_seconds = value.parse().context("interval")?;
        }
        "model" => {
            if !crate::config::valid_model_id(value) {
                bail!("model contains unsupported characters");
            }
            config.codex.model = value.to_string();
            if !config.codex.models.iter().any(|model| model == value) {
                config.codex.models.push(value.to_string());
            }
        }
        "fallback" => {
            config.codex.state_fallback = match value {
                "strict" => crate::config::StateFallback::Strict,
                "passthrough" => crate::config::StateFallback::Passthrough,
                _ => bail!("fallback must be strict or passthrough"),
            };
        }
        "account-mode" => {
            config.codex.account_mode = match value {
                "auto" => crate::config::AccountMode::Auto,
                "personal" => crate::config::AccountMode::Personal,
                "team" => crate::config::AccountMode::Team,
                _ => bail!("account-mode must be auto, personal or team"),
            };
        }
        "fixed-model" => {
            config.codex.fixed_model = value.parse().context("fixed-model")?;
        }
        "network-guard" => {
            config.protection.network_guard = value.parse().context("network-guard")?;
        }
        "kill-on-block" => {
            config.protection.kill_on_block = value.parse().context("kill-on-block")?;
        }
        "http-proxy" => {
            config.protection.http_proxy = if value.trim().is_empty() {
                None
            } else {
                crate::config::validate_proxy_url(value)?;
                Some(value.to_string())
            };
        }
        "codex-upstream" => {
            config.codex.upstream = if value.trim().is_empty() {
                None
            } else {
                crate::config::validate_upstream(value)?;
                Some(value.to_string())
            };
        }
        "injection" => {
            config.codex.injection = value.parse().context("injection")?;
        }
        _ => bail!("unknown key {key}"),
    }
    config.save(paths)?;
    println!("已保存 {key} = {value}");
    Ok(())
}

fn set_model(paths: &AppPaths, model: &str) -> Result<()> {
    if !crate::config::valid_model_id(model) {
        bail!("model contains unsupported characters");
    }
    let mut config = Config::load_or_create(paths)?;
    config.codex.model = model.to_string();
    config.codex.fixed_model = true;
    if !config.codex.models.iter().any(|entry| entry == model) {
        config.codex.models.push(model.to_string());
    }
    config.save(paths)?;
    if config_tx::set_installed_model(&config, paths, model)? {
        println!("已固定模型并写入当前 Codex 接管配置: {model}");
    } else {
        println!("已固定模型: {model}；下次 codex takeover 时写入");
    }
    Ok(())
}

fn models(paths: &AppPaths, action: ModelsAction) -> Result<()> {
    let mut config = Config::load_or_create(paths)?;
    match action {
        ModelsAction::List => {
            for model in &config.codex.models {
                let selected = if model == &config.codex.model {
                    "*"
                } else {
                    " "
                };
                println!("{selected} {model}");
            }
            println!(
                "\n校准前缀: {}",
                config.codex.calibration_prefixes.join(", ")
            );
            println!("辅助模型: {}", config.codex.auxiliary_models.join(", "));
            println!("辅助前缀: {}", config.codex.auxiliary_prefixes.join(", "));
        }
        ModelsAction::Add { model } => {
            if !crate::config::valid_model_id(&model) {
                bail!("model contains unsupported characters");
            }
            if !config.codex.models.iter().any(|entry| entry == &model) {
                config.codex.models.push(model.clone());
                config.save(paths)?;
            }
            println!("已加入模型目录: {model}");
        }
        ModelsAction::Remove { model } => {
            if model == config.codex.model {
                bail!("不能移除当前选中的模型；先切换到其他模型");
            }
            config.codex.models.retain(|entry| entry != &model);
            config.save(paths)?;
            println!("已移除模型目录项: {model}");
        }
        ModelsAction::Reset => {
            config.codex.models = crate::config::DEFAULT_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect();
            config.save(paths)?;
            println!("模型目录已重置为内置列表");
        }
    }
    Ok(())
}

fn state(paths: &AppPaths, action: StateAction) -> Result<()> {
    let mut config = Config::load_or_create(paths)?;
    match action {
        StateAction::List => {
            let (personal, _) = crate::codex::provider::accepted_blocks_with_extra(
                &crate::config::AccountMode::Personal,
                10,
                None,
                &config.codex.extra_state_blocks,
            );
            let (team, _) = crate::codex::provider::accepted_blocks_with_extra(
                &crate::config::AccountMode::Team,
                12,
                None,
                &config.codex.extra_state_blocks,
            );
            println!("个人接受块数: {personal:?}");
            println!("Team 接受块数: {team:?}");
            println!("额外块数: {:?}", config.codex.extra_state_blocks);
            println!("说明: 块数只是封装形状，不代表模型质量。");
        }
        StateAction::AddBlock { blocks } => {
            if !(1..=128).contains(&blocks) {
                bail!("blocks must be 1-128");
            }
            if !config.codex.extra_state_blocks.contains(&blocks) {
                config.codex.extra_state_blocks.push(blocks);
                config.codex.extra_state_blocks.sort_unstable();
                config.save(paths)?;
            }
            println!("已接受新的 state 块数: {blocks}");
        }
        StateAction::RemoveBlock { blocks } => {
            config
                .codex
                .extra_state_blocks
                .retain(|value| *value != blocks);
            config.save(paths)?;
            println!("已移除额外 state 块数: {blocks}");
        }
    }
    Ok(())
}

async fn codex(paths: &AppPaths, action: CodexAction) -> Result<()> {
    let config = Config::load_or_create(paths)?;
    match action {
        CodexAction::Takeover { auth, profile } => {
            let info = config_tx::takeover(&config, paths, profile.as_deref(), &auth)?;
            println!("provider: {}", info.selection.provider_id);
            println!("upstream: {}", provider::upstream_label(&info.selection));
            println!("official: {}", info.selection.official);
            println!("auth: {}", info.selection.auth_kind);
            println!("changed: {}", info.changed);
            println!(
                "Codex 配置: {}",
                config.codex_home().join("config.toml").display()
            );
        }
        CodexAction::Restore { force } => {
            config_tx::restore(&config, paths, force)?;
            println!("已恢复 Codex 原配置");
        }
        CodexAction::Status => {
            let status = config_tx::current_status(&config, paths);
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        CodexAction::Probe => {
            let proxy = ProxyRuntime::new(config, paths.clone())?;
            let report = proxy.probe_now().await;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.ok {
                bail!("没有采集到可用 state");
            }
        }
    }
    Ok(())
}

fn update_lock(paths: &AppPaths, action: OnOff) -> Result<()> {
    let mut config = Config::load_or_create(paths)?;
    let enabled = matches!(action, OnOff::On);
    config.update_lock.enabled = enabled;
    let report = if enabled {
        guard::update_lock(&config, &paths.data_dir)
    } else {
        guard::remove_update_lock(&paths.data_dir)
    };
    if !report.errors.is_empty() {
        config.update_lock.enabled = false;
        config.save(paths)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        bail!("自动更新锁定未生效，配置已回退为关闭");
    }
    config.save(paths)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn autostart(paths: &AppPaths, action: OnOff) -> Result<()> {
    let mut config = Config::load_or_create(paths)?;
    let enabled = matches!(action, OnOff::On);
    guard::set_autostart(enabled)?;
    config.ui.autostart = enabled;
    config.save(paths)?;
    println!("登录自启动: {}", if enabled { "已开启" } else { "已关闭" });
    Ok(())
}

async fn guarded(command: Vec<String>, paths: AppPaths) -> Result<()> {
    let config = Config::load_or_create(&paths)?;
    if command.is_empty() {
        bail!("缺少要启动的命令");
    }
    if !config.protection.enabled {
        let status = std::process::Command::new(&command[0])
            .args(&command[1..])
            .status()
            .with_context(|| format!("spawn {}", command[0]))?;
        std::process::exit(status.code().unwrap_or(1));
    }
    let report = ipcheck::check(&config).await;
    if !report.allowed {
        bail!("出口未通过：{}", report.detail);
    }
    let mut child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .spawn()
        .with_context(|| format!("spawn {}", command[0]))?;
    let mut interval = tokio::time::interval(Duration::from_secs(
        config.protection.interval_seconds.max(1),
    ));
    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                std::process::exit(status.code().unwrap_or(1));
            }
            _ = interval.tick() => {
                let latest = ipcheck::check(&config).await;
                if !latest.allowed {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    bail!("运行中出口失配，已终止子进程：{}", latest.detail);
                }
            }
        }
    }
}
