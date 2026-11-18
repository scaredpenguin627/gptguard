//! Native first-run and status window.
//!
//! The management HTTP API remains available as a compatibility transport for
//! the daemon and the hidden CLI, but the normal user path is this Rust/Slint
//! window.  The UI never opens a browser and never exposes the internal ports.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fs2::FileExt;
use serde_json::Value;

use crate::config::{AppPaths, Config};
use crate::service;
use crate::util;

slint::include_modules!();

pub fn run() -> Result<()> {
    let paths = AppPaths::discover();
    paths.ensure()?;
    util::init_logging(&paths.log_dir)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(paths.data_dir.join("ui.lock"))
        .context("open UI lock")?;
    if lock_file.try_lock_exclusive().is_err() {
        return Ok(());
    }
    ensure_daemon(&paths)?;

    let window = MainWindow::new().context("create native window")?;
    let shared = Arc::new(paths);
    let operation_lock = Arc::new(std::sync::Mutex::new(()));

    {
        let paths = shared.clone();
        let weak = window.as_weak();
        window.on_refresh_clicked(move || {
            refresh_async(paths.clone(), weak.clone());
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        window.on_diagnostics_clicked(move || {
            diagnostics_async(paths.clone(), weak.clone());
        });
    }
    {
        let weak = window.as_weak();
        window.on_about_clicked(move || {
            set_detail(
                weak.clone(),
                "GPTGuard v0.4.4 · 内置 Mihomo v1.19.34（GPL-3.0）\n源码：https://github.com/MetaCubeX/mihomo\nGPTGuard 只把 Mihomo 作为受监督的出站 sidecar，不开放局域网监听。".into(),
            );
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        let lock = operation_lock.clone();
        window.on_connect_local_clicked(move || {
            set_operation_busy(weak.clone(), true);
            connect_local_async(paths.clone(), weak.clone(), lock.clone());
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        let lock = operation_lock.clone();
        window.on_trust_local_exit_clicked(move |ip| {
            set_operation_busy(weak.clone(), true);
            trust_local_exit_async(paths.clone(), weak.clone(), lock.clone(), ip.to_string());
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        window.on_import_clicked(move |url| {
            let url = url.trim().to_string();
            if url.is_empty() {
                refresh_async(paths.clone(), weak.clone());
                return;
            }
            let weak = weak.clone();
            let paths = paths.clone();
            std::thread::spawn(move || {
                let result = post_api(
                    &paths,
                    "/api/codex/subscription",
                    serde_json::json!({"url": url}),
                )
                .and_then(|value| {
                    restart_if_requested(&paths, &value)?;
                    Ok(())
                });
                let message = match result {
                    Ok(()) => "订阅已导入，正在检测节点".to_string(),
                    Err(err) => format!("订阅导入失败：{err:#}"),
                };
                update_window(weak, message, paths);
            });
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        window.on_file_clicked(move || {
            let paths = paths.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                let Some(file) = rfd::FileDialog::new()
                    .add_filter("节点文件", &["yaml", "yml", "txt"])
                    .pick_file()
                else {
                    return;
                };
                let message = match std::fs::read_to_string(&file)
                    .context("读取节点文件")
                    .and_then(|content| {
                        post_api(
                            &paths,
                            "/api/codex/import-file",
                            serde_json::json!({
                                "name": file.file_name().and_then(|name| name.to_str()).unwrap_or("节点文件"),
                                "content": content,
                            }),
                        )
                        .and_then(|value| {
                            restart_if_requested(&paths, &value)?;
                            Ok(())
                        })
                    }) {
                    Ok(()) => "节点文件已导入，正在检测".to_string(),
                    Err(err) => format!("节点文件导入失败：{err:#}"),
                };
                update_window(weak, message, paths);
            });
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        window.on_takeover_clicked(move || {
            let weak = weak.clone();
            let paths = paths.clone();
            std::thread::spawn(move || {
                let result = post_api(
                    &paths,
                    "/api/codex/takeover",
                    serde_json::json!({"auth": "unknown"}),
                );
                let message = match result {
                    Ok(_) => "Codex 配置已接管，provider 身份保持不变".to_string(),
                    Err(err) => format!("Codex 接管失败：{err:#}"),
                };
                set_detail(weak, message);
            });
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        window.on_restore_clicked(move || {
            let weak = weak.clone();
            let paths = paths.clone();
            std::thread::spawn(move || {
                let result = post_api(
                    &paths,
                    "/api/codex/restore",
                    serde_json::json!({"force": true}),
                );
                match result {
                    Ok(_) => update_window(
                        weak,
                        "Codex 原配置已恢复，当前文件已另存为冲突备份".to_string(),
                        paths,
                    ),
                    Err(err) => set_detail(weak, format!("恢复失败：{err:#}")),
                }
            });
        });
    }
    {
        let paths = shared.clone();
        let weak = window.as_weak();
        window.on_protection_clicked(move |enabled| {
            let paths = paths.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = post_api(
                    &paths,
                    "/api/protection",
                    serde_json::json!({"enabled": enabled}),
                );
                let message = match result {
                    Ok(_) => if enabled {
                        "严格保护已开启"
                    } else {
                        "保护已关闭"
                    }
                    .to_string(),
                    Err(err) => format!("修改保护状态失败：{err:#}"),
                };
                update_window(weak, message, paths);
            });
        });
    }

    {
        let paths = shared.clone();
        let weak = window.as_weak();
        let lock = operation_lock.clone();
        set_operation_busy(weak.clone(), true);
        std::thread::spawn(move || {
            let _guard = lock.lock().ok();
            // This is intentionally idempotent: an existing explicit route or
            // imported subscription always wins over auto-detection.
            if let Ok(value) = post_api(&paths, "/api/codex/auto-proxy", serde_json::json!({})) {
                let _ = restart_if_requested(&paths, &value);
            }
            match fetch_status(&paths) {
                Ok(value) => {
                    apply_snapshot(&weak, &value, false, None);
                }
                Err(err) => set_detail(weak.clone(), format!("后台尚未就绪：{err:#}")),
            }
            set_operation_busy(weak, false);
        });
    }
    window.run().context("run native window")?;
    Ok(())
}

fn ensure_daemon(paths: &AppPaths) -> Result<()> {
    // The tray process owns the notification-area lifetime.  It starts the
    // same executable's Tokio daemon when needed; starting it here keeps the
    // one-file installer path free of a second tray binary.
    util::spawn_detached(&util::exe_path(), &["--cli", "tray", "--start-guard"])?;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(150));
        if service::daemon_running(paths) {
            return Ok(());
        }
    }
    anyhow::bail!("GPTGuard 后台启动超时，请打开诊断查看日志")
}

fn restart_if_requested(paths: &AppPaths, response: &Value) -> Result<()> {
    if response["restart_required"].as_bool().unwrap_or(false) {
        restart_daemon(paths)?;
    }
    Ok(())
}

fn restart_daemon(paths: &AppPaths) -> Result<()> {
    for _ in 0..60 {
        if !service::daemon_running(paths) {
            util::spawn_detached(&util::exe_path(), &["--cli", "run"])?;
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(150));
                if service::daemon_running(paths) {
                    return Ok(());
                }
            }
            anyhow::bail!("GPTGuard 后台启动超时，请打开诊断查看日志")
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("等待后台重启超时，请打开诊断查看日志")
}

fn refresh_async(paths: Arc<AppPaths>, weak: slint::Weak<MainWindow>) {
    refresh_async_mode(paths, weak, false, None);
}

fn refresh_async_mode(
    paths: Arc<AppPaths>,
    weak: slint::Weak<MainWindow>,
    allow_trust_candidate: bool,
    expected_proxy: Option<String>,
) {
    std::thread::spawn(move || {
        let message = match fetch_status(&paths) {
            Ok(value) => apply_snapshot(
                &weak,
                &value,
                allow_trust_candidate,
                expected_proxy.as_deref(),
            ),
            Err(err) => {
                set_detail(weak, format!("后台尚未就绪：{err:#}"));
                return;
            }
        };
        if !message.is_empty() {
            set_detail(weak, message);
        }
    });
}

fn connect_local_async(
    paths: Arc<AppPaths>,
    weak: slint::Weak<MainWindow>,
    lock: Arc<std::sync::Mutex<()>>,
) {
    std::thread::spawn(move || {
        let _guard = lock.lock().ok();
        let result = post_api(&paths, "/api/codex/connect-local", serde_json::json!({})).and_then(
            |response| {
                let proxy = response["proxy"].as_str().unwrap_or_default().to_string();
                if proxy.is_empty() {
                    anyhow::bail!("后台没有返回本机代理地址")
                }
                restart_if_requested(&paths, &response)?;
                let check = post_api(&paths, "/api/check", serde_json::json!({}))?;
                let status = fetch_status(&paths)?;
                Ok((proxy, check, status))
            },
        );
        match result {
            Ok((proxy, check, status)) => {
                apply_snapshot(&weak, &status, true, Some(&proxy));
                let detail = check["runtime"]["detail"]
                    .as_str()
                    .unwrap_or("本机代理检测完成");
                set_detail(weak.clone(), format!("已连接 {proxy}；实际检测：{detail}"));
            }
            Err(err) => set_detail(weak.clone(), format!("一键连接失败：{err:#}")),
        }
        set_operation_busy(weak, false);
    });
}

fn trust_local_exit_async(
    paths: Arc<AppPaths>,
    weak: slint::Weak<MainWindow>,
    lock: Arc<std::sync::Mutex<()>>,
    ip: String,
) {
    std::thread::spawn(move || {
        let _guard = lock.lock().ok();
        let result = post_api(
            &paths,
            "/api/codex/trust-local-exit",
            serde_json::json!({"ip": ip}),
        )
        .and_then(|response| {
            restart_if_requested(&paths, &response)?;
            let check = post_api(&paths, "/api/check", serde_json::json!({}))?;
            let status = fetch_status(&paths)?;
            Ok((check, status))
        });
        match result {
            Ok((check, status)) => {
                apply_snapshot(&weak, &status, false, None);
                let detail = check["runtime"]["detail"]
                    .as_str()
                    .unwrap_or("出口信任已更新");
                set_detail(weak.clone(), format!("已信任出口；实际检测：{detail}"));
            }
            Err(err) => set_detail(weak.clone(), format!("信任出口失败：{err:#}")),
        }
        set_operation_busy(weak, false);
    });
}

fn diagnostics_async(paths: Arc<AppPaths>, weak: slint::Weak<MainWindow>) {
    std::thread::spawn(move || match fetch_status(&paths) {
        Ok(value) => {
            let detail = value["doctor"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| {
                            let name = row["name"].as_str()?;
                            let state = if row["ok"].as_bool().unwrap_or(false) {
                                "通过"
                            } else {
                                "需要修复"
                            };
                            let message = row["detail"].as_str().unwrap_or_default();
                            Some(format!("{name}：{state}（{message}）"))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "没有可用诊断信息".into());
            set_detail(weak, detail);
        }
        Err(err) => set_detail(weak, format!("读取诊断失败：{err:#}")),
    });
}

fn update_window(weak: slint::Weak<MainWindow>, message: String, paths: Arc<AppPaths>) {
    set_detail(weak.clone(), message);
    refresh_async(paths, weak);
}

fn set_operation_busy(weak: slint::Weak<MainWindow>, busy: bool) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_operation_busy(busy);
            if busy {
                window.set_trust_candidate_visible(false);
                window.set_trust_candidate_ip("".into());
            }
        }
    });
}

fn set_detail(weak: slint::Weak<MainWindow>, message: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_detail_text(message.into());
        }
    });
}

fn apply_snapshot(
    weak: &slint::Weak<MainWindow>,
    value: &Value,
    allow_trust_candidate: bool,
    expected_proxy: Option<&str>,
) -> String {
    let runtime = &value["runtime"];
    let status = runtime["status"].as_str().unwrap_or("unknown");
    let detail = runtime["detail"].as_str().unwrap_or("后台已启动");
    let allowed = runtime["allowed"].as_bool().unwrap_or(false);
    let protection = value["config"]["protection"]["enabled"]
        .as_bool()
        .unwrap_or(false);
    let proxy = &value["proxy"];
    let mihomo = proxy["mihomo"]["detail"]
        .as_str()
        .or_else(|| proxy["detail"].as_str())
        .unwrap_or("出口待检测");
    let config_matches = proxy["config_matches"].as_bool().unwrap_or(false);
    let provider = proxy["auth_kind"].as_str().unwrap_or("未接管");
    let legacy_needed = value["legacy_provider"]["needed"]
        .as_bool()
        .unwrap_or(false);
    let setup_needed = !config_matches || legacy_needed;
    let status_text = match (protection, allowed, status) {
        (true, true, _) => "已保护",
        (true, false, "blocked") => "已阻断",
        (true, false, _) => "等待检测",
        (false, _, _) => "尚未开启保护",
    };
    let route_text = format!(
        "{}；出口：{}",
        mihomo,
        if allowed { "检测通过" } else { "待检测" }
    );
    let codex_text = if legacy_needed {
        let changes = value["legacy_provider"]["changes"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("；")
            })
            .unwrap_or_else(|| "恢复原 provider 身份".into());
        format!("检测到旧版 GPTGuard provider。配置差异：{changes}")
    } else if config_matches {
        format!("已接管，provider：{provider}")
    } else {
        let changes = value["takeover_preview"]["changes"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("；")
            })
            .unwrap_or_default();
        if changes.is_empty() {
            "检测到未接管或需要修复的 Codex 配置".to_string()
        } else {
            format!("待确认的 Codex 配置差异：{changes}")
        }
    };
    let trust_candidate_ip = if allow_trust_candidate {
        trust_candidate_ip(value, expected_proxy).unwrap_or_default()
    } else {
        String::new()
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop({
        let status_text = status_text.to_string();
        let detail = detail.to_string();
        let route_text = route_text.clone();
        let codex_text = codex_text.clone();
        let takeover_label = if legacy_needed {
            "修复 Codex"
        } else {
            "确认接管配置"
        };
        let trust_candidate_ip = trust_candidate_ip.clone();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_status_text(status_text.into());
                window.set_detail_text(detail.into());
                window.set_route_text(route_text.into());
                window.set_codex_text(codex_text.into());
                window.set_protection_enabled(protection);
                window.set_setup_needed(setup_needed);
                window.set_takeover_label(takeover_label.into());
                window.set_trust_candidate_visible(!trust_candidate_ip.is_empty());
                window.set_trust_candidate_ip(trust_candidate_ip.into());
            }
        }
    });
    String::new()
}

fn trust_candidate_ip(value: &Value, expected_proxy: Option<&str>) -> Option<String> {
    let expected_proxy = expected_proxy?;
    if !value["config"]["protection"]["whitelist"]
        .as_array()
        .is_some_and(|entries| entries.is_empty())
    {
        return None;
    }
    if !value["runtime"]["detected"].as_bool().unwrap_or(false) {
        return None;
    }
    let ip = value["runtime"]["http_ip"]
        .as_str()?
        .parse::<IpAddr>()
        .ok()?;
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    let routes = value["config"]["codex"]["routes"].as_array()?;
    let enabled = routes
        .iter()
        .filter(|route| route["enabled"].as_bool().unwrap_or(false))
        .collect::<Vec<_>>();
    if enabled.len() != 1 || enabled[0]["url"].as_str()? != expected_proxy {
        return None;
    }
    Some(ip.to_string())
}

fn fetch_status(paths: &AppPaths) -> Result<Value> {
    let config = Config::load_or_create(paths)?;
    let url = format!("http://127.0.0.1:{}/api/status", config.ui.web_port);
    let token = config.ui.admin_token.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create UI request runtime")?;
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        let response = client
            .get(url)
            .header("x-gptguard-token", token)
            .send()
            .await?;
        if !response.status().is_success() {
            anyhow::bail!("后台返回 HTTP {}", response.status());
        }
        Ok(response.json().await?)
    })
}

fn post_api(paths: &AppPaths, endpoint: &str, body: Value) -> Result<Value> {
    let config = Config::load_or_create(paths)?;
    let url = format!("http://127.0.0.1:{}{endpoint}", config.ui.web_port);
    let token = config.ui.admin_token.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create UI request runtime")?;
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()?;
        let response = client
            .post(url)
            .header("x-gptguard-token", token)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        let value: Value = serde_json::from_str(&body).context("解析后台响应")?;
        if !status.is_success() {
            let message = value["error"]["message"]
                .as_str()
                .unwrap_or("后台拒绝了请求");
            anyhow::bail!("后台返回 HTTP {}：{message}", status);
        }
        Ok(value)
    })
}
