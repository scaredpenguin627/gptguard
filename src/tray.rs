//! Windows notification-area controls.
//!
//! The tray is a thin client: it reads the shared state file and calls the
//! loopback panel API. Enforcement stays in the daemon, so closing the tray
//! never disables protection.

#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use winit::application::ApplicationHandler;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};

    use crate::config::{AppPaths, Config};
    use crate::desktop::DesktopSession;
    use crate::service;
    use crate::util;

    struct TrayState {
        _tray: TrayIcon,
        status: MenuItem,
        check: MenuItem,
        open: MenuItem,
        protection: MenuItem,
        update_lock: MenuItem,
        autostart: MenuItem,
        exit: MenuItem,
    }

    pub struct TrayApp {
        paths: AppPaths,
        desktop: DesktopSession,
        token: String,
        port: u16,
        tray: Option<TrayState>,
        last_refresh: Instant,
    }

    impl TrayApp {
        fn new(paths: AppPaths, config: &Config, desktop: DesktopSession) -> Self {
            Self {
                paths,
                desktop,
                token: config.ui.admin_token.clone(),
                port: config.ui.web_port,
                tray: None,
                last_refresh: Instant::now() - Duration::from_secs(10),
            }
        }

        fn init(&mut self) -> Result<()> {
            let menu = Menu::new();
            let status = MenuItem::new("状态：读取中", false, None);
            let check = MenuItem::new("立即检测出口", true, None);
            let open = MenuItem::new("打开 GPTGuard", true, None);
            let protection = MenuItem::new("开启 / 关闭防护", true, None);
            let update_lock = MenuItem::new("开启 / 关闭自动更新锁定", true, None);
            let autostart = MenuItem::new("开启 / 关闭登录自启动", true, None);
            let exit = MenuItem::new("退出 GPTGuard", true, None);
            menu.append_items(&[
                &status,
                &PredefinedMenuItem::separator(),
                &check,
                &open,
                &PredefinedMenuItem::separator(),
                &protection,
                &update_lock,
                &autostart,
                &PredefinedMenuItem::separator(),
                &exit,
            ])
            .context("build tray menu")?;

            let icon = build_icon();
            let tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("GPTGuard")
                .with_icon(icon)
                .build()
                .context("create tray icon")?;
            self.tray = Some(TrayState {
                _tray: tray,
                status,
                check,
                open,
                protection,
                update_lock,
                autostart,
                exit,
            });
            Ok(())
        }

        fn refresh(&mut self) {
            let Some(tray) = &self.tray else {
                return;
            };
            let Some(state) = service::read_state(&self.paths) else {
                tray.status.set_text("状态：守护未运行");
                return;
            };
            let label = match state.status {
                crate::guard::Verdict::Allowed => "允许",
                crate::guard::Verdict::Blocked => "已阻断",
                crate::guard::Verdict::Offline => "无法确认出口",
                crate::guard::Verdict::Disabled => "防护关闭",
                crate::guard::Verdict::Error => "执行层异常",
                crate::guard::Verdict::Stopped => "未运行",
            };
            tray.status.set_text(format!(
                "状态：{label} · HTTP {} · Codex {}",
                state.http_ip.as_deref().unwrap_or("-"),
                if state.codex.config_matches {
                    "已接管"
                } else {
                    "未就绪"
                }
            ));
            tray._tray
                .set_tooltip(Some(format!(
                    "GPTGuard\n{label}\nHTTP: {}\nSTUN: {}",
                    state.http_ip.as_deref().unwrap_or("-"),
                    state.stun_ip.as_deref().unwrap_or("-")
                )))
                .ok();
        }

        fn handle_menu(&mut self, event_loop: &ActiveEventLoop) {
            let Some(tray) = &self.tray else {
                return;
            };
            let check_id = tray.check.id().clone();
            let open_id = tray.open.id().clone();
            let protection_id = tray.protection.id().clone();
            let update_id = tray.update_lock.id().clone();
            let autostart_id = tray.autostart.id().clone();
            let exit_id = tray.exit.id().clone();
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == check_id {
                    self.post("/api/check", serde_json::json!({}));
                } else if event.id == open_id {
                    let _ = self.desktop.spawn(&[]);
                } else if event.id == protection_id {
                    self.toggle("/api/protection", "protection");
                } else if event.id == update_id {
                    self.toggle("/api/update-lock", "update_lock");
                } else if event.id == autostart_id {
                    self.toggle("/api/autostart", "ui");
                } else if event.id == exit_id {
                    if let Err(err) = self.quit_desktop() {
                        util::error(format!("tray exit: {err:#}"));
                        rfd::MessageDialog::new()
                            .set_title("GPTGuard 退出失败")
                            .set_description(format!("无法通知主窗口退出。{err:#}"))
                            .show();
                    } else {
                        event_loop.exit();
                    }
                    return;
                }
            }
        }

        fn handle_tray_click(&mut self) {
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    let _ = self.desktop.spawn(&[]);
                }
            }
        }

        fn post(&self, path: &str, body: serde_json::Value) {
            let url = format!("http://127.0.0.1:{}{}", self.port, path);
            let token = self.token.clone();
            std::thread::spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => return,
                };
                runtime.block_on(async move {
                    let client = match reqwest::Client::builder()
                        .timeout(Duration::from_secs(30))
                        .build()
                    {
                        Ok(client) => client,
                        Err(_) => return,
                    };
                    let _ = client
                        .post(&url)
                        .header("x-gptguard-token", token)
                        .json(&body)
                        .send()
                        .await;
                    // The next tick refreshes from state.json.
                });
            });
        }

        fn quit_desktop(&self) -> Result<()> {
            // Older workers only know the authenticated shutdown endpoint.
            // Send it before the broadcast, so a fresh launch cannot receive
            // a delayed HTTP shutdown intended for the previous session.
            self.stop_worker();
            self.desktop.request_exit()
        }

        fn stop_worker(&self) {
            let path = "/api/shutdown";
            let url = format!("http://127.0.0.1:{}{}", self.port, path);
            let token = self.token.clone();
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let _ = runtime.block_on(async move {
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(1))
                    .build()?;
                client
                    .post(url)
                    .header("x-gptguard-token", token)
                    .json(&serde_json::json!({}))
                    .send()
                    .await
            });
        }

        fn toggle(&self, path: &str, key: &str) {
            let url = format!("http://127.0.0.1:{}/api/status", self.port);
            let post_url = format!("http://127.0.0.1:{}{}", self.port, path);
            let token = self.token.clone();
            let key = key.to_string();
            std::thread::spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => return,
                };
                runtime.block_on(async move {
                    let client = match reqwest::Client::builder()
                        .timeout(Duration::from_secs(15))
                        .build()
                    {
                        Ok(client) => client,
                        Err(_) => return,
                    };
                    let Ok(response) = client
                        .get(&url)
                        .header("x-gptguard-token", &token)
                        .send()
                        .await
                    else {
                        return;
                    };
                    let Ok(value) = response.json::<serde_json::Value>().await else {
                        return;
                    };
                    let enabled = if key == "ui" {
                        value["config"]["ui"]["autostart"]
                            .as_bool()
                            .unwrap_or(false)
                    } else {
                        value["config"][&key]["enabled"].as_bool().unwrap_or(false)
                    };
                    let _ = client
                        .post(&post_url)
                        .header("x-gptguard-token", &token)
                        .json(&serde_json::json!({ "enabled": !enabled }))
                        .send()
                        .await;
                });
            });
        }
    }

    impl ApplicationHandler for TrayApp {
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
            if self.tray.is_none() {
                if let Err(err) = self.init() {
                    util::error(format!("tray init: {err:#}"));
                    return;
                }
                self.refresh();
            }
        }

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _window_id: winit::window::WindowId,
            _event: winit::event::WindowEvent,
        ) {
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.handle_menu(event_loop);
            if event_loop.exiting() || self.desktop.exit_requested() {
                event_loop.exit();
                return;
            }
            self.handle_tray_click();
            if self.last_refresh.elapsed() > Duration::from_secs(2) {
                self.last_refresh = Instant::now();
                self.refresh();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(200),
            ));
        }
    }

    fn build_icon() -> Icon {
        let size = 32u32;
        let mut rgba = vec![0u8; (size * size * 4) as usize];
        for y in 0..size {
            for x in 0..size {
                let dx = x as f32 - 15.5;
                let dy = y as f32 - 15.5;
                let distance = (dx * dx + dy * dy).sqrt();
                let inside = distance < 14.0;
                let ring = distance > 11.5 && distance < 14.0;
                let index = ((y * size + x) * 4) as usize;
                if inside {
                    rgba[index] = if ring { 235 } else { 57 };
                    rgba[index + 1] = if ring { 245 } else { 185 };
                    rgba[index + 2] = if ring { 255 } else { 123 };
                    rgba[index + 3] = 255;
                }
            }
        }
        // A small white check mark.
        for (x, y) in [
            (9, 16),
            (10, 17),
            (11, 18),
            (12, 19),
            (13, 20),
            (14, 21),
            (15, 22),
            (16, 21),
            (17, 20),
            (18, 19),
            (19, 18),
            (20, 17),
            (21, 16),
            (22, 15),
            (23, 14),
        ] {
            let index = ((y * size + x) * 4) as usize;
            rgba[index] = 255;
            rgba[index + 1] = 255;
            rgba[index + 2] = 255;
            rgba[index + 3] = 255;
        }
        Icon::from_rgba(rgba, size, size).expect("valid tray icon")
    }

    pub fn run(start_guard: bool) -> Result<()> {
        run_with_paths(AppPaths::discover(), start_guard)
    }

    pub fn run_with_paths(paths: AppPaths, start_guard: bool) -> Result<()> {
        run_impl(paths, start_guard, false)
    }

    pub fn run_gui() -> Result<()> {
        run_impl(AppPaths::discover(), true, true)
    }

    fn run_impl(paths: AppPaths, start_guard: bool, open_panel: bool) -> Result<()> {
        let desktop = DesktopSession::join(&paths)?;
        paths.ensure()?;
        util::init_logging(&paths.log_dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(paths.data_dir.join("tray.lock"))?;
        if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
            anyhow::bail!("托盘已经在运行");
        }
        let _lock = lock;
        let config = Config::load_or_create(&paths)?;
        ensure_daemon(&paths, start_guard, &desktop)?;
        if open_panel {
            let url = format!(
                "http://127.0.0.1:{}/#token={}",
                config.ui.web_port, config.ui.admin_token
            );
            let _ = util::open_url(&url);
        }

        let event_loop = EventLoop::<()>::with_user_event()
            .build()
            .context("create winit event loop")?;
        event_loop.set_control_flow(ControlFlow::Wait);
        let mut app = TrayApp::new(paths, &config, desktop);
        event_loop
            .run_app(&mut app)
            .context("run tray event loop")?;
        Ok(())
    }

    fn ensure_daemon(paths: &AppPaths, _start_guard: bool, desktop: &DesktopSession) -> Result<()> {
        if daemon_alive(paths) {
            return Ok(());
        }
        util::info("starting daemon from tray");
        desktop.spawn(&["--cli", "run"])?;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(150));
            if daemon_alive(paths) {
                return Ok(());
            }
        }
        anyhow::bail!("守护启动超时；请从命令行运行 gptguard run 查看错误")
    }

    fn daemon_alive(paths: &AppPaths) -> bool {
        service::daemon_running(paths)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn tray_quit_stops_the_worker_and_reaches_the_window_even_when_offline() {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path();
            let paths = AppPaths {
                config_dir: dir.into(),
                data_dir: dir.into(),
                config_file: dir.join("config.json"),
                state_file: dir.join("state.json"),
                log_dir: dir.join("logs"),
                backups_dir: dir.join("backups"),
                lock_file: dir.join("guard.lock"),
            };
            let api = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let relay = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let mut cfg = Config::load_or_create(&paths).unwrap();
            cfg.ui.web_port = api.local_addr().unwrap().port();
            cfg.codex.listen = relay.local_addr().unwrap().to_string();
            cfg.codex.codex_home = Some(dir.join("codex").display().to_string());
            cfg.protection.network_guard = false;
            cfg.protection.kill_on_block = false;
            cfg.protection.targets.clear();
            cfg.save(&paths).unwrap();
            drop((api, relay));
            let rt = tokio::runtime::Runtime::new().unwrap();
            let worker = rt.spawn(service::run(cfg.clone(), paths.clone(), false));
            let deadline = Instant::now() + Duration::from_secs(10);
            while std::net::TcpStream::connect(("127.0.0.1", cfg.ui.web_port)).is_err() {
                assert!(
                    Instant::now() < deadline,
                    "worker did not open management port"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            for _ in 0..2 {
                let window = DesktopSession::join(&paths).unwrap();
                assert!(!window.exit_requested());
                let app = TrayApp::new(paths.clone(), &cfg, DesktopSession::join(&paths).unwrap());
                app.quit_desktop().unwrap();
                assert!(window.exit_requested());
                let until = Instant::now() + Duration::from_secs(8);
                while daemon_alive(&paths) {
                    assert!(Instant::now() < until, "worker still running after quit");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            rt.block_on(worker).unwrap().unwrap();
        }
    }
}

#[cfg(windows)]
pub use imp::{run, run_gui, run_with_paths};

#[cfg(not(windows))]
pub fn run(_start_guard: bool) -> anyhow::Result<()> {
    anyhow::bail!("托盘目前只支持 Windows；其他平台请使用 gptguard run 和网页面板")
}

#[cfg(not(windows))]
pub fn run_with_paths(_paths: crate::config::AppPaths, start_guard: bool) -> anyhow::Result<()> {
    run(start_guard)
}

#[cfg(not(windows))]
pub fn run_gui() -> anyhow::Result<()> {
    run(true)
}
