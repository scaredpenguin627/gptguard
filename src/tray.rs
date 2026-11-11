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
        token: String,
        port: u16,
        tray: Option<TrayState>,
        last_refresh: Instant,
    }

    impl TrayApp {
        pub fn new(paths: AppPaths, config: &Config) -> Self {
            Self {
                paths,
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
            let open = MenuItem::new("打开设置面板", true, None);
            let protection = MenuItem::new("开启 / 关闭防护", true, None);
            let update_lock = MenuItem::new("开启 / 关闭自动更新锁定", true, None);
            let autostart = MenuItem::new("开启 / 关闭登录自启动", true, None);
            let exit = MenuItem::new("退出托盘（守护继续运行）", true, None);
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
                    let _ = util::open_url(&self.panel_url());
                } else if event.id == protection_id {
                    self.toggle("/api/protection", "protection");
                } else if event.id == update_id {
                    self.toggle("/api/update-lock", "update_lock");
                } else if event.id == autostart_id {
                    self.toggle("/api/autostart", "ui");
                } else if event.id == exit_id {
                    event_loop.exit();
                }
            }
        }

        fn handle_tray_click(&mut self) {
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    let _ = util::open_url(&self.panel_url());
                }
            }
        }

        fn panel_url(&self) -> String {
            format!("http://127.0.0.1:{}/#token={}", self.port, self.token)
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
        run_impl(start_guard, false)
    }

    pub fn run_gui() -> Result<()> {
        run_impl(true, true)
    }

    fn run_impl(start_guard: bool, open_panel: bool) -> Result<()> {
        let paths = AppPaths::discover();
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
        ensure_daemon(&paths, start_guard)?;
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
        let mut app = TrayApp::new(paths, &config);
        event_loop
            .run_app(&mut app)
            .context("run tray event loop")?;
        Ok(())
    }

    fn ensure_daemon(paths: &AppPaths, _start_guard: bool) -> Result<()> {
        if daemon_alive(paths) {
            return Ok(());
        }
        util::info("starting daemon from tray");
        util::spawn_detached(&cli_path(), &["run"])?;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(150));
            if daemon_alive(paths) {
                return Ok(());
            }
        }
        anyhow::bail!("守护启动超时；请从命令行运行 gptguard run 查看错误")
    }

    fn cli_path() -> std::path::PathBuf {
        let exe = util::exe_path();
        for name in ["gptguard-cli.exe", "gptguard.exe"] {
            let candidate = exe.with_file_name(name);
            if candidate.is_file() && candidate != exe {
                return candidate;
            }
        }
        exe
    }

    fn daemon_alive(paths: &AppPaths) -> bool {
        service::daemon_running(paths)
    }
}

#[cfg(windows)]
pub use imp::{run, run_gui};

#[cfg(not(windows))]
pub fn run(_start_guard: bool) -> anyhow::Result<()> {
    anyhow::bail!("托盘目前只支持 Windows；其他平台请使用 gptguard run 和网页面板")
}

#[cfg(not(windows))]
pub fn run_gui() -> anyhow::Result<()> {
    run(true)
}
