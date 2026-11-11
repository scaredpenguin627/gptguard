//! Local management panel. Loopback only, same-origin checks, bearer token for
//! every API call. The token is never placed in a URL query or a cookie.

use std::collections::HashMap;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::codex::config_tx;
use crate::codex::provider;
use crate::codex::routes;
use crate::config::{parse_net, Config};
use crate::service::AppState;
use crate::util;

pub async fn serve(
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let port = state.config().ui.web_port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("bind panel 127.0.0.1:{port}"))?;
    util::info(format!("management panel on http://127.0.0.1:{port}"));
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("serve management panel")?;
    Ok(())
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/config", get(api_status))
        .route("/api/check", post(api_check))
        .route("/api/protection", post(api_protection))
        .route("/api/whitelist", post(api_whitelist))
        .route("/api/doctor", get(api_status))
        .route("/api/logs", get(api_logs))
        .route("/api/codex/takeover", post(api_takeover))
        .route("/api/codex/restore", post(api_restore))
        .route("/api/codex/probe", post(api_probe))
        .route("/api/codex/subscription", post(api_subscription))
        .route("/api/update-lock", post(api_update_lock))
        .route("/api/autostart", post(api_autostart))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

fn host_allowed(state: &AppState, headers: &HeaderMap) -> bool {
    let port = state.config().ui.web_port;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let allowed = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if !allowed.iter().any(|value| value.eq_ignore_ascii_case(host)) {
        return false;
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let expected = [
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ];
        if !expected.iter().any(|value| value == origin) {
            return false;
        }
    }
    true
}

fn authorized(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    if !host_allowed(state, headers) {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "host_rejected",
            "只接受本机回环地址和同源请求",
        ));
    }
    let token = headers
        .get("x-aiguard-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if token != state.config().ui.admin_token {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "token_required",
            "需要管理口令；请从启动地址的 #token= 片段获取",
        ));
    }
    Ok(())
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "ok": false,
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}

async fn api_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let cfg = state.config();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "runtime": state.runtime(),
            "proxy": state.proxy_status(),
            "config": config_view(&cfg),
            "doctor": state.doctor().into_iter().map(|(name, ok, detail)| serde_json::json!({
                "name": name, "ok": ok, "detail": detail
            })).collect::<Vec<_>>(),
            "panel_url": state.panel_url()
        })),
    )
        .into_response()
}

fn config_view(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "protection": {
            "enabled": cfg.protection.enabled,
            "ip_check_mode": cfg.protection.ip_check_mode,
            "whitelist": cfg.protection.whitelist,
            "interval_seconds": cfg.protection.interval_seconds,
            "http_proxy": cfg.protection.http_proxy.as_deref().map(util::mask_url),
            "targets": cfg.protection.targets,
            "network_guard": cfg.protection.network_guard,
            "kill_on_block": cfg.protection.kill_on_block,
        },
        "codex": {
            "enabled": cfg.codex.enabled,
            "listen": cfg.codex.listen,
            "model": cfg.codex.model,
            "fixed_model": cfg.codex.fixed_model,
            "account_mode": cfg.codex.account_mode,
            "state_fallback": cfg.codex.state_fallback,
            "injection": cfg.codex.injection,
            "direct": cfg.codex.direct,
            "routes": cfg.codex.routes.iter().map(|route| serde_json::json!({
                "id": route.id,
                "label": route.label,
                "url": util::mask_url(&route.url),
                "enabled": route.enabled
            })).collect::<Vec<_>>(),
            "subscriptions": cfg.codex.subscriptions.len(),
        },
        "update_lock": {
            "enabled": cfg.update_lock.enabled,
            "domains": cfg.update_lock.domains,
            "paths": cfg.update_lock.paths,
        },
        "ui": {
            "web_port": cfg.ui.web_port,
            "autostart": cfg.ui.autostart,
        }
    })
}

async fn api_check(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let runtime = state.check_now().await;
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "runtime": runtime})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct ProtectionBody {
    enabled: bool,
}

async fn api_protection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProtectionBody>,
) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let mut cfg = state.config();
    cfg.protection.enabled = body.enabled;
    if let Err(err) = state.save_config(&cfg) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            &format!("{err:#}"),
        );
    }
    if !body.enabled {
        let _ = crate::guard::enforce(&cfg, false);
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

#[derive(Deserialize)]
struct WhitelistBody {
    entries: Vec<String>,
}

async fn api_whitelist(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<WhitelistBody>,
) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let mut entries = Vec::new();
    for entry in body.entries {
        let entry = entry.trim().to_string();
        if entry.is_empty() {
            continue;
        }
        if let Err(err) = parse_net(&entry) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_whitelist",
                &format!("{entry}: {err}"),
            );
        }
        entries.push(entry);
    }
    let mut cfg = state.config();
    cfg.protection.whitelist = entries;
    if let Err(err) = state.save_config(&cfg) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            &format!("{err:#}"),
        );
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

async fn api_logs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let mut files: Vec<_> = std::fs::read_dir(&state.paths.log_dir)
        .map(|dir| {
            dir.filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let text = files
        .last()
        .and_then(|path| util::read_tail(path, 250).ok())
        .unwrap_or_else(|| "没有日志".into());
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "log": text})),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
struct TakeoverBody {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

async fn api_takeover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TakeoverBody>,
) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let cfg = state.config();
    let auth = body.auth.as_deref().unwrap_or("unknown");
    match config_tx::takeover(&cfg, &state.paths, body.profile.as_deref(), auth) {
        Ok(info) => {
            let _ = state.proxy.reload(cfg);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "provider": info.selection.provider_id,
                    "upstream": provider::upstream_label(&info.selection),
                    "official": info.selection.official,
                    "changed": info.changed
                })),
            )
                .into_response()
        }
        Err(err) => api_error(
            StatusCode::BAD_REQUEST,
            "takeover_failed",
            &format!("{err:#}"),
        ),
    }
}

#[derive(Deserialize, Default)]
struct RestoreBody {
    #[serde(default)]
    force: bool,
}

async fn api_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RestoreBody>,
) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let cfg = state.config();
    match config_tx::restore(&cfg, &state.paths, body.force) {
        Ok(()) => {
            let _ = state.proxy.reload(cfg);
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(err) => api_error(StatusCode::CONFLICT, "restore_failed", &format!("{err:#}")),
    }
}

async fn api_probe(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let report = state.probe_codex().await;
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "probe": report})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SubscriptionBody {
    url: String,
}

async fn api_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscriptionBody>,
) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let mut cfg = state.config();
    match routes::import_subscription(&mut cfg, &state.paths, body.url.trim()).await {
        Ok(result) => {
            let _ = state.save_config(&cfg);
            (
                StatusCode::OK,
                Json(serde_json::json!({"ok": true, "import": result})),
            )
                .into_response()
        }
        Err(err) => api_error(
            StatusCode::BAD_REQUEST,
            "subscription_failed",
            &format!("{err:#}"),
        ),
    }
}

#[derive(Deserialize)]
struct ToggleBody {
    enabled: bool,
}

async fn api_update_lock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ToggleBody>,
) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    let mut cfg = state.config();
    cfg.update_lock.enabled = body.enabled;
    let report = if body.enabled {
        crate::guard::update_lock(&cfg, &state.paths.data_dir)
    } else {
        crate::guard::remove_update_lock(&state.paths.data_dir)
    };
    if !report.errors.is_empty() {
        cfg.update_lock.enabled = false;
    }
    if let Err(err) = state.save_config(&cfg) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            &format!("{err:#}"),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": report.errors.is_empty(), "report": report})),
    )
        .into_response()
}

async fn api_autostart(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ToggleBody>,
) -> Response {
    if let Err(response) = authorized(&state, &headers) {
        return response;
    }
    match crate::guard::set_autostart(body.enabled) {
        Ok(()) => {
            let mut cfg = state.config();
            cfg.ui.autostart = body.enabled;
            let _ = state.save_config(&cfg);
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(err) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "autostart_failed",
            &format!("{err:#}"),
        ),
    }
}

/// Keep the endpoint surface discoverable for tests and future panel versions.
pub fn routes_map() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("GET", "/api/status"),
        ("POST", "/api/check"),
        ("POST", "/api/protection"),
        ("POST", "/api/whitelist"),
        ("POST", "/api/codex/takeover"),
        ("POST", "/api/codex/restore"),
        ("POST", "/api/codex/probe"),
        ("POST", "/api/update-lock"),
        ("POST", "/api/autostart"),
    ])
}
