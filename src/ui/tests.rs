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

#[derive(Default)]
struct Daemon {
    trusted: bool,
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
        "config": {"protection": {"enabled": state.enabled, "whitelist": if state.trusted {vec![IP]} else {vec![]}}},
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
    if enabled && !state.trusted {
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
    assert_eq!(body["ip"], IP);
    assert_eq!(body["proxy"], PROXY);
    if state.exit_changed {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":{"code":"exit_changed","message":"出口已变化，请重新检测"}})),
        );
    }
    state.trusted = true;
    (StatusCode::OK, Json(json!({"ok":true})))
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
        .route("/api/codex/connect-local", post(|| async { Json(json!({"ok":true,"proxy":"http://127.0.0.1:7897"})) }))
        .route("/api/check", post(|| async { Json(json!({"ok":true,"trust_candidate":{"ip":IP,"proxy":PROXY},"runtime":{"detected":true}})) }))
        .route("/api/codex/trust-local-exit", post(trust)).with_state(state.clone());
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
        window: window.as_weak(),
        revision: Default::default(),
        polling: Default::default(),
    };
    bind_actions(&window, &ui);
    window.show().unwrap();
    show_status(&window, fetch_status(&paths));

    click(&window, "开启严格保护");
    window.invoke_protection_clicked(true); // A queued second click must not start a second request.
    wait_for_action(&window);
    assert!(!window.get_protection_enabled());
    assert_eq!(state.lock().unwrap().activation_requests, 1);
    show_status(&window, fetch_status(&paths));
    assert_eq!(window.get_trust_candidate_ip(), IP);
    state.lock().unwrap().exit_changed = true;
    let confirm = format!("信任并开启保护 · {IP}");
    click(&window, &confirm);
    wait_for_action(&window);
    assert!(!state.lock().unwrap().trusted && !window.get_protection_enabled());
    show_status(&window, fetch_status(&paths));
    assert!(window.get_action_text().contains("出口已变化"));

    state.lock().unwrap().exit_changed = false;
    click(&window, "开启严格保护");
    wait_for_action(&window);
    click(&window, &confirm);
    wait_for_action(&window);
    assert!(state.lock().unwrap().trusted && !window.get_protection_enabled());
    show_status(&window, fetch_status(&paths));
    assert!(window.get_action_text().contains("后台需要管理员权限"));
    state.lock().unwrap().elevated = true;
    click(&window, "开启严格保护");
    wait_for_action(&window);
    assert!(window.get_protection_enabled() && window.get_protection_healthy());
    click(&window, "关闭严格保护");
    wait_for_action(&window);
    assert!(!window.get_protection_enabled() && !state.lock().unwrap().enabled);
}
