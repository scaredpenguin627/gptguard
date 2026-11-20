use super::*;
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use i_slint_backend_testing::ElementHandle;
use std::sync::Mutex;

const PROXY: &str = "http://127.0.0.1:7897/";
const IP: &str = "203.0.113.7";
const NEXT_IP: &str = "198.51.100.8";

#[derive(Default)]
struct Daemon {
    rules: Vec<String>,
    next_node: bool,
    enabled: bool,
    exit_changed: bool,
    elevated: bool,
    activation_requests: usize,
}
type Fixture = Arc<Mutex<Daemon>>;

async fn status(State(state): State<Fixture>) -> Json<Value> {
    let state = state.lock().unwrap();
    Json(json!({
        "daemon_elevated": state.elevated, "local_proxy": PROXY,
        "config": {"protection": {"enabled": state.enabled, "whitelist": state.rules}},
        // Background status no longer contains the preceding manual check.
        "runtime": {"status": if state.enabled {"allowed"} else {"disabled"}, "allowed": state.enabled, "detected": false}
    }))
}

async fn protection(
    State(state): State<Fixture>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    state.activation_requests += 1;
    let enabled = body["enabled"] == true;
    if enabled && state.rules.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":{"code":"whitelist_required","message":"请先确认出口"}})),
        );
    }
    if enabled && !state.elevated {
        return (
            StatusCode::FORBIDDEN,
            Json(
                json!({"error":{"code":"daemon_elevation_required","message":"后台需要管理员权限"}}),
            ),
        );
    }
    state.enabled = enabled;
    (StatusCode::OK, Json(json!({"ok":true,"enabled":enabled})))
}

async fn trust(State(state): State<Fixture>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    let ip = if state.next_node { NEXT_IP } else { IP };
    assert_eq!(body["ip"], ip);
    assert_eq!(body["proxy"], PROXY);
    if state.exit_changed {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":{"code":"exit_changed","message":"出口已变化，请重新检测"}})),
        );
    }
    state.rules.push(ip.into());
    (StatusCode::OK, Json(json!({"ok":true})))
}

async fn check(State(state): State<Fixture>) -> Json<Value> {
    let state = state.lock().unwrap();
    let ip = if state.next_node { NEXT_IP } else { IP };
    let candidate = (!crate::config::ip_allowed(ip.parse().unwrap(), &state.rules))
        .then(|| json!({"ip":ip,"proxy":PROXY}));
    Json(json!({"ok":true,"trust_candidate":candidate,"runtime":{"detected":true}}))
}

async fn add_rule(State(state): State<Fixture>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body["append"], true);
    state
        .lock()
        .unwrap()
        .rules
        .push(body["entries"][0].as_str().unwrap().into());
    Json(json!({"ok":true}))
}

async fn remove_rule(State(state): State<Fixture>, Json(body): Json<Value>) -> Json<Value> {
    state
        .lock()
        .unwrap()
        .rules
        .retain(|rule| Some(rule.as_str()) != body["entry"].as_str());
    Json(json!({"ok":true}))
}

fn click(window: &MainWindow, label: &str) {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .unwrap_or_else(|| panic!("missing button: {label}"))
        .invoke_accessible_default_action();
}

fn wait_for_action(window: &MainWindow) {
    assert!(
        window.get_operation_busy(),
        "click must acknowledge immediately"
    );
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    let start = std::time::Instant::now();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            if !weak.unwrap().get_operation_busy() || start.elapsed() > Duration::from_secs(15) {
                slint::quit_event_loop().unwrap();
            }
        },
    );
    slint::run_event_loop_until_quit().unwrap();
    assert!(
        !window.get_operation_busy(),
        "timed out waiting for click result"
    );
}

#[test]
fn protection_buttons_confirm_the_exit_and_show_the_daemons_actual_state() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let state = Fixture::default();
    let app = Router::new()
        .route("/api/status", get(status))
        .route("/api/protection", post(protection))
        .route(
            "/api/codex/connect-local",
            post(|| async { Json(json!({"ok":true,"proxy":"http://127.0.0.1:7897"})) }),
        )
        .route("/api/check", post(check))
        .route("/api/whitelist", post(add_rule))
        .route("/api/whitelist/remove", post(remove_rule))
        .route("/api/codex/trust-local-exit", post(trust))
        .with_state(state.clone());
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let paths = Arc::new(AppPaths {
        config_file: dir.join("config.json"),
        config_dir: dir.into(),
        data_dir: dir.into(),
        state_file: dir.join("state.json"),
        lock_file: dir.join("daemon.lock"),
        log_dir: dir.join("logs"),
        backups_dir: dir.join("backups"),
    });
    let mut cfg = Config::default();
    cfg.ui.web_port = port;
    cfg.ui.admin_token = "fixture".into();
    cfg.save(&paths).unwrap();
    let window = MainWindow::new().unwrap();
    let ui = Ui {
        paths: paths.clone(),
        desktop: DesktopSession::join(&paths).unwrap(),
        window: window.as_weak(),
        revision: Default::default(),
        polling: Default::default(),
    };
    bind_actions(&window, &ui);
    window.show().unwrap();
    show_status(&window, fetch_status(&paths));

    assert!(ElementHandle::find_by_accessible_label(&window, "后台诊断")
        .next()
        .is_some());
    click(&window, "收起诊断");
    click(&window, "查看诊断");
    click(&window, "开启严格保护");
    window.invoke_protection_clicked(true); // A queued second click must not start a second request.
    wait_for_action(&window);
    assert!(!window.get_protection_enabled());
    assert_eq!(state.lock().unwrap().activation_requests, 1);
    show_status(&window, fetch_status(&paths));
    assert_eq!(window.get_trust_candidate_ip(), IP);
    click(&window, "取消确认");
    assert!(state.lock().unwrap().rules.is_empty() && !window.get_protection_enabled());
    assert!(window.get_trust_candidate_ip().is_empty());
    click(&window, "开启严格保护");
    wait_for_action(&window);
    state.lock().unwrap().exit_changed = true;
    let confirm = format!("信任并开启保护 · {IP}");
    click(&window, &confirm);
    wait_for_action(&window);
    assert!(state.lock().unwrap().rules.is_empty() && !window.get_protection_enabled());
    show_status(&window, fetch_status(&paths));
    assert!(window.get_action_text().contains("出口已变化"));

    state.lock().unwrap().exit_changed = false;
    click(&window, "开启严格保护");
    wait_for_action(&window);
    click(&window, &confirm);
    wait_for_action(&window);
    assert_eq!(state.lock().unwrap().rules, [IP]);
    assert!(!window.get_protection_enabled());
    show_status(&window, fetch_status(&paths));
    assert!(window.get_action_text().contains("后台需要管理员权限"));
    state.lock().unwrap().elevated = true;
    click(&window, "开启严格保护");
    wait_for_action(&window);
    assert!(window.get_protection_enabled() && window.get_protection_healthy());
    click(&window, "关闭严格保护");
    wait_for_action(&window);
    assert!(!window.get_protection_enabled() && !state.lock().unwrap().enabled);

    // Switch to another measured exit while the previous rule is still present.
    state.lock().unwrap().next_node = true;
    click(&window, "检测出口");
    wait_for_action(&window);
    show_status(&window, fetch_status(&paths));
    assert_eq!(window.get_trust_candidate_ip(), NEXT_IP);
    click(&window, &format!("信任出口 · {NEXT_IP}"));
    wait_for_action(&window);
    assert_eq!(state.lock().unwrap().rules, [IP, NEXT_IP]);

    click(&window, "管理白名单");
    window.set_whitelist_input("wrong".into());
    click(&window, "添加规则");
    wait_for_action(&window);
    assert!(window.get_action_failed());
    assert_eq!(window.get_whitelist_input(), "wrong");
    window.set_whitelist_input("2001:db8::/64".into());
    click(&window, "添加规则");
    wait_for_action(&window);
    assert!(window.get_whitelist_input().is_empty());
    click(&window, &format!("删除规则 {IP}"));
    wait_for_action(&window);
    assert_eq!(state.lock().unwrap().rules, [NEXT_IP, "2001:db8::/64"]);
    assert_eq!(window.get_whitelist_entries().row_count(), 2);

    // A busy API operation must not suppress the explicit tray exit.
    let _watch = watch_tray_exit(ui.desktop.clone());
    window.set_operation_busy(true);
    ui.desktop.request_exit().unwrap();
    let timeout = slint::Timer::default();
    let timed_out = std::rc::Rc::new(std::cell::Cell::new(false));
    let expired = timed_out.clone();
    timeout.start(
        slint::TimerMode::SingleShot,
        Duration::from_secs(5),
        move || {
            expired.set(true);
            slint::quit_event_loop().unwrap();
        },
    );
    slint::run_event_loop_until_quit().unwrap();
    assert!(!timed_out.get());
    assert!(!DesktopSession::join(&paths).unwrap().exit_requested());
}
