//! Local Responses/SSE relay with turn-state collection.
//!
//! The relay borrows credentials from the incoming Codex request and keeps them
//! only in memory. It never imports auth.json, never refreshes OAuth, and never
//! replays a generation request after an upstream response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::codex::config_tx;
use crate::codex::provider::{self, Selection};
use crate::codex::routes::{self, Route};
use crate::codex::turnstate::{self, Policy, Store, Token};
use crate::config::{AppPaths, Config, StateFallback};
use crate::util;

const MAX_BODY: usize = 64 * 1024 * 1024;
const MAX_PROBE_BODY: usize = 1024 * 1024;

#[derive(Clone)]
pub struct ProxyRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    paths: AppPaths,
    config: RwLock<Config>,
    routes: RwLock<Vec<Route>>,
    selection: RwLock<RuntimeSelection>,
    sessions: Mutex<HashMap<String, Arc<AsyncMutex<Session>>>>,
    limits: Mutex<HashMap<String, Arc<Mutex<Limit>>>>,
    requests: Mutex<u64>,
}

#[derive(Clone, Debug)]
struct RuntimeSelection {
    upstream: String,
    official: bool,
    auth_kind: String,
    owned: bool,
}

struct Session {
    store: Store,
    headers: HeaderMap,
    limit: Arc<Mutex<Limit>>,
    last_used: Instant,
}

#[derive(Default)]
struct Limit {
    blocked: bool,
    status: u16,
    retry_until: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyStatus {
    pub listen: String,
    pub upstream: String,
    pub official: bool,
    pub auth_kind: String,
    pub owned: bool,
    pub config_matches: bool,
    pub routes: usize,
    pub sessions: usize,
    pub requests: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    pub ok: bool,
    pub route: Option<String>,
    pub blocks: Option<usize>,
    pub detail: String,
}

impl ProxyRuntime {
    pub fn new(config: Config, paths: AppPaths) -> Result<Self> {
        let routes = routes::build_routes(&config)?;
        let selection = load_selection(&config, &paths);
        Ok(Self {
            inner: Arc::new(Inner {
                paths,
                config: RwLock::new(config),
                routes: RwLock::new(routes),
                selection: RwLock::new(selection),
                sessions: Mutex::new(HashMap::new()),
                limits: Mutex::new(HashMap::new()),
                requests: Mutex::new(0),
            }),
        })
    }

    pub fn listen(&self) -> String {
        self.inner.config.read().unwrap().codex.listen.clone()
    }

    pub fn reload(&self, config: Config) -> Result<()> {
        let routes = routes::build_routes(&config)?;
        let selection = load_selection(&config, &self.inner.paths);
        *self.inner.routes.write().unwrap() = routes;
        *self.inner.selection.write().unwrap() = selection;
        *self.inner.config.write().unwrap() = config;
        Ok(())
    }

    pub fn status(&self) -> ProxyStatus {
        let config = self.inner.config.read().unwrap().clone();
        let selection = self.inner.selection.read().unwrap().clone();
        let matches = if selection.owned {
            config_tx::current_status(&config, &self.inner.paths).config_matches
        } else {
            true
        };
        ProxyStatus {
            listen: config.codex.listen.clone(),
            upstream: provider::upstream_label(&Selection {
                provider_id: String::new(),
                profile: None,
                upstream: selection.upstream.clone(),
                auth_kind: selection.auth_kind.clone(),
                env_key: None,
                official: selection.official,
                requires_openai_auth: selection.auth_kind == "chatgpt",
                config_sha256: String::new(),
            }),
            official: selection.official,
            auth_kind: selection.auth_kind,
            owned: selection.owned,
            config_matches: matches,
            routes: self.inner.routes.read().unwrap().len(),
            sessions: self.inner.sessions.lock().unwrap().len(),
            requests: *self.inner.requests.lock().unwrap(),
            detail: if matches {
                "转发服务运行中".into()
            } else {
                "Codex 配置在接管后被修改；请求将返回 409".into()
            },
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/backend-api/codex/responses", post(handle_responses))
            .route("/backend-api/codex/responses/compact", post(handle_compact))
            .route("/backend-api/codex/models", get(handle_models))
            .route("/backend-api/codex/alpha/search", post(handle_search))
            .fallback(not_found)
            .with_state(self)
    }

    pub async fn serve(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let listen = self.listen();
        let listener = tokio::net::TcpListener::bind(&listen)
            .await
            .with_context(|| format!("bind Codex relay {listen}"))?;
        util::info(format!("Codex relay listening on http://{listen}"));
        axum::serve(listener, self.router())
            .with_graceful_shutdown(shutdown)
            .await
            .context("serve Codex relay")?;
        Ok(())
    }

    pub async fn probe_now(&self) -> ProbeReport {
        let config = self.inner.config.read().unwrap().clone();
        let selection = self.inner.selection.read().unwrap().clone();
        let routes = self.inner.routes.read().unwrap().clone();
        let headers = HeaderMap::new();
        let (policy, _) = policy_for(&config, &headers, &config.codex.model);
        let mut store = Store::new(policy);
        match probe(
            &config,
            &selection,
            &routes,
            &headers,
            &config.codex.model,
            &mut store,
        )
        .await
        {
            Ok(Some((token, index))) => ProbeReport {
                ok: true,
                route: routes.get(index).map(|route| route.id.clone()),
                blocks: Some(token.blocks),
                detail: "采集到一份可用 state".into(),
            },
            Ok(None) => ProbeReport {
                ok: false,
                route: None,
                blocks: None,
                detail: "本轮没有采集到符合规则的 state".into(),
            },
            Err(err) => ProbeReport {
                ok: false,
                route: None,
                blocks: None,
                detail: format!("{err:#}"),
            },
        }
    }

    fn check_local(&self, headers: &HeaderMap) -> Result<(), Response> {
        let listen = self.listen();
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let expected = [
            listen.clone(),
            listen.replace("127.0.0.1", "localhost"),
            listen.replace("127.0.0.1", "[::1]"),
        ];
        if !expected
            .iter()
            .any(|value| value.eq_ignore_ascii_case(host))
        {
            return Err(local_error(
                StatusCode::FORBIDDEN,
                "local_clients_only",
                "只接受本机回环地址的 Codex 请求",
            ));
        }
        if headers.contains_key(header::ORIGIN) || headers.contains_key("sec-fetch-site") {
            return Err(local_error(
                StatusCode::FORBIDDEN,
                "browser_origin_rejected",
                "不接受浏览器 Origin 请求",
            ));
        }
        Ok(())
    }

    fn check_owned_config(&self) -> Result<(), Response> {
        let selection = self.inner.selection.read().unwrap().clone();
        if !selection.owned {
            return Ok(());
        }
        let config = self.inner.config.read().unwrap().clone();
        let status = config_tx::current_status(&config, &self.inner.paths);
        if !status.config_matches {
            return Err(local_error(
                StatusCode::CONFLICT,
                "codex_config_changed",
                "Codex 配置在接管后被修改；为避免把新凭据送到旧上游，已停止转发。请在面板检查并恢复或重新接管",
            ));
        }
        Ok(())
    }

    fn bump_requests(&self) {
        if let Ok(mut count) = self.inner.requests.lock() {
            *count += 1;
        }
    }
}

async fn not_found() -> Response {
    local_error(
        StatusCode::NOT_FOUND,
        "unsupported_endpoint",
        "此路径不由 AIGuard 转发",
    )
}

fn local_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "type": "aiguard_error",
            "code": code,
            "message": message
        }
    });
    (
        status,
        [
            ("X-AIGuard-Error-Source", "local"),
            ("Cache-Control", "no-store"),
        ],
        axum::Json(body),
    )
        .into_response()
}

async fn handle_responses(State(state): State<ProxyRuntime>, request: Request<Body>) -> Response {
    handle_generation(state, request, false).await
}

async fn handle_compact(State(state): State<ProxyRuntime>, request: Request<Body>) -> Response {
    handle_generation(state, request, true).await
}

async fn handle_models(State(state): State<ProxyRuntime>, request: Request<Body>) -> Response {
    handle_passthrough(state, request, false).await
}

async fn handle_search(State(state): State<ProxyRuntime>, request: Request<Body>) -> Response {
    handle_passthrough(state, request, true).await
}

async fn handle_passthrough(
    state: ProxyRuntime,
    request: Request<Body>,
    is_post: bool,
) -> Response {
    if let Err(response) = state.check_local(request.headers()) {
        return response;
    }
    if request.method() == Method::POST && !is_post {
        return local_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method",
            "method not allowed",
        );
    }
    if let Err(response) = state.check_owned_config() {
        return response;
    }
    if request.headers().contains_key(header::UPGRADE) {
        return local_error(
            StatusCode::UPGRADE_REQUIRED,
            "http_sse_required",
            "只支持 HTTP/SSE，不支持 WebSocket",
        );
    }
    let selection = state.inner.selection.read().unwrap().clone();
    let routes = state.inner.routes.read().unwrap().clone();
    let route = match routes.first() {
        Some(route) => route,
        None => {
            return local_error(StatusCode::SERVICE_UNAVAILABLE, "no_routes", "没有可用出口");
        }
    };
    let path = request.uri().path().to_string();
    let suffix = if path.ends_with("/models") {
        "/models"
    } else {
        "/alpha/search"
    };
    let upstream = format!("{}{}", selection.upstream, suffix);
    let (parts, body) = request.into_parts();
    let body_bytes = match to_bytes(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return local_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "请求体过大",
            );
        }
    };
    let response = match forward(
        &route.client,
        parts.method.clone(),
        &upstream,
        &parts.headers,
        Some(body_bytes),
        &selection,
        None,
    )
    .await
    {
        Ok(response) => response,
        Err(err) => {
            return local_error(
                StatusCode::BAD_GATEWAY,
                "upstream_unavailable",
                &format!("上游连接失败: {err}"),
            );
        }
    };
    state.bump_requests();
    relay_response(response)
}

async fn handle_generation(state: ProxyRuntime, request: Request<Body>, compact: bool) -> Response {
    if let Err(response) = state.check_local(request.headers()) {
        return response;
    }
    if let Err(response) = state.check_owned_config() {
        return response;
    }
    if request.headers().contains_key(header::UPGRADE) {
        return local_error(
            StatusCode::UPGRADE_REQUIRED,
            "http_sse_required",
            "只支持 HTTP/SSE，不支持 WebSocket",
        );
    }
    let config = state.inner.config.read().unwrap().clone();
    let selection = state.inner.selection.read().unwrap().clone();
    let routes = state.inner.routes.read().unwrap().clone();
    if routes.is_empty() {
        return local_error(StatusCode::SERVICE_UNAVAILABLE, "no_routes", "没有可用出口");
    }

    let (parts, body) = request.into_parts();
    let decoded = match decode_request(body, &parts.headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let model = match serde_json::from_slice::<ModelRequest>(&decoded) {
        Ok(request) => request.model.unwrap_or_default(),
        Err(_) => {
            return local_error(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "请求体不是有效的 JSON",
            );
        }
    };
    if !provider::valid_model_name(&model) {
        return local_error(StatusCode::BAD_REQUEST, "invalid_model", "model 名称不合法");
    }
    let calibration = provider::is_calibration_model(&model);
    let inject = !compact && calibration && selection.official && config.codex.injection;

    let auth = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !auth.starts_with("Bearer ") || auth.len() > 16_384 {
        return local_error(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "需要 Codex 登录凭据",
        );
    }
    if !selection.official && provider::official_credential(Some(&auth)) {
        return local_error(
            StatusCode::UNAUTHORIZED,
            "official_credentials_on_relay",
            "检测到官方登录凭据，已阻止发送给中转站",
        );
    }
    let account = parts
        .headers
        .get("ChatGPT-Account-Id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let credential_key = hash_key(&format!("{auth}\u{0}{account}"));
    let limit = get_limit(&state, &credential_key);
    if let Some((status, retry)) = limit.lock().unwrap().rejection() {
        return local_error(
            status,
            if status == 429 {
                "upstream_rate_limited"
            } else {
                "upstream_auth_rejected"
            },
            &format!("上游要求暂停；{} 秒后再试", retry),
        );
    }

    let session_key = hash_key(&format!("{credential_key}\u{0}{model}"));
    let session = get_session(
        &state,
        &session_key,
        &model,
        &config,
        &parts.headers,
        limit.clone(),
    );
    let mut session_guard = session.lock().await;
    session_guard.last_used = Instant::now();
    let now = SystemTime::now();
    let (mut snapshot, mut usable) = session_guard.store.acquire(now);
    if inject && !usable {
        let probe_headers = session_guard.headers.clone();
        let probe_result = probe(
            &config,
            &selection,
            &routes,
            &probe_headers,
            &model,
            &mut session_guard.store,
        )
        .await;
        match probe_result {
            Ok(Some((_token, _))) => {
                let acquired = session_guard.store.acquire(SystemTime::now());
                snapshot = acquired.0;
                usable = acquired.1;
            }
            Ok(None) => {}
            Err(err) => {
                let text = format!("{err:#}");
                if let Some(status) = probe_rejection_status(&text) {
                    limit.lock().unwrap().reject(status, 60);
                    return local_error(
                        StatusCode::from_u16(status).unwrap_or(StatusCode::UNAUTHORIZED),
                        if status == 429 {
                            "upstream_rate_limited"
                        } else {
                            "upstream_auth_rejected"
                        },
                        "上游拒绝凭据或要求暂停；本次请求未转发，也不换出口重试",
                    );
                }
                util::warn(format!("state probe failed: {text}"));
            }
        }
    }
    if inject && !usable {
        if config.codex.state_fallback == StateFallback::Passthrough {
            // Never replay: this is the first and only forward of this request.
            return forward_generation(
                &state,
                &config,
                &selection,
                &routes,
                &parts,
                &decoded,
                &mut session_guard,
                None,
                false,
                true,
            )
            .await;
        }
        return local_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "state_unavailable",
            "没有可用的 turn-state；严格模式下不转发。请检查出口、账号规则或切换为普通转发兜底",
        );
    }

    let route_index = if usable {
        snapshot.route.min(routes.len().saturating_sub(1))
    } else {
        0
    };
    let route = &routes[route_index];
    let active_snapshot = if inject && usable {
        Some(snapshot)
    } else {
        None
    };
    forward_generation(
        &state,
        &config,
        &selection,
        std::slice::from_ref(route),
        &parts,
        &decoded,
        &mut session_guard,
        active_snapshot,
        compact,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn forward_generation(
    state: &ProxyRuntime,
    _config: &Config,
    selection: &RuntimeSelection,
    routes: &[Route],
    parts: &axum::http::request::Parts,
    body: &Bytes,
    session: &mut Session,
    snapshot: Option<turnstate::Snapshot>,
    compact: bool,
    fallback_passthrough: bool,
) -> Response {
    let suffix = if compact {
        "/responses/compact"
    } else {
        "/responses"
    };
    let upstream = format!("{}{}", selection.upstream, suffix);
    let route = match routes.first() {
        Some(route) => route,
        None => return local_error(StatusCode::SERVICE_UNAVAILABLE, "no_routes", "没有可用出口"),
    };
    let token = snapshot.as_ref().and_then(|value| value.token.clone());
    let mut headers = parts.headers.clone();
    headers.remove(header::HOST);
    headers.remove(header::COOKIE);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::TRANSFER_ENCODING);
    headers.remove(header::PROXY_AUTHORIZATION);
    headers.remove(header::ORIGIN);
    headers.remove(header::REFERER);
    headers.remove("sec-fetch-site");
    headers.remove("sec-fetch-mode");
    if selection.official {
        if let Some(token) = &token {
            if let Ok(value) = HeaderValue::from_str(&token.value) {
                headers.insert(HeaderName::from_static("x-codex-turn-state"), value);
            }
        }
    } else {
        headers.remove("x-codex-turn-state");
        let names: Vec<HeaderName> = headers
            .keys()
            .filter(|name| name.as_str().to_ascii_lowercase().starts_with("chatgpt-"))
            .cloned()
            .collect();
        for name in names {
            headers.remove(name);
        }
        headers.remove("originator");
        headers.remove("version");
    }

    let response = match forward(
        &route.client,
        Method::POST,
        &upstream,
        &headers,
        Some(body.clone()),
        selection,
        token.as_ref(),
    )
    .await
    {
        Ok(response) => response,
        Err(err) => {
            return local_error(
                StatusCode::BAD_GATEWAY,
                "upstream_unavailable",
                &format!("上游连接失败，未重放: {err}"),
            );
        }
    };
    state.bump_requests();

    let status = response.status();
    if matches!(status.as_u16(), 401 | 403 | 429) {
        session
            .limit
            .lock()
            .unwrap()
            .reject(status.as_u16(), retry_after(&response));
    }
    if let Some(snapshot) = &snapshot {
        let observed = response
            .headers()
            .get(turnstate::HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if status.is_success() && !observed.is_empty() {
            let now = SystemTime::now();
            if session.store.observe(&observed, snapshot, now) {
                session.store.reject_and_promote(snapshot, now);
                return local_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "state_shape_changed",
                    "响应头 state 不符合规则，正文已拦截；本次可能已计费，不会自动重放",
                );
            }
        }
    }
    let mut response = relay_response(response);
    if fallback_passthrough {
        response.headers_mut().insert(
            "x-aiguard-mode",
            HeaderValue::from_static("fallback-passthrough"),
        );
    }
    response
}

fn relay_response(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if matches!(
            name.as_str().to_ascii_lowercase().as_str(),
            "set-cookie" | "content-length" | "transfer-encoding" | "connection"
        ) {
            continue;
        }
        builder = builder.header(name, value);
    }
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)));
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| local_error(StatusCode::BAD_GATEWAY, "response", "无法构造响应"))
}

#[derive(Deserialize)]
struct ModelRequest {
    #[serde(default)]
    model: Option<String>,
}

async fn decode_request(body: Body, headers: &HeaderMap) -> Result<Bytes, Response> {
    let bytes = to_bytes(body, MAX_BODY).await.map_err(|_| {
        local_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "请求体过大",
        )
    })?;
    let encoding = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if bytes.is_empty() || encoding.is_empty() || encoding == "identity" {
        return Ok(bytes);
    }
    let decoded = match encoding.as_str() {
        "gzip" => {
            use std::io::Read;
            let mut decoder = flate2::read::GzDecoder::new(bytes.as_ref());
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).map_err(|_| {
                local_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_encoding",
                    "gzip 请求体损坏",
                )
            })?;
            out
        }
        "zstd" => zstd::stream::decode_all(bytes.as_ref()).map_err(|_| {
            local_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_encoding",
                "zstd 请求体损坏",
            )
        })?,
        _ => {
            return Err(local_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_encoding",
                "只支持 gzip、zstd 或未压缩请求",
            ));
        }
    };
    if decoded.len() > MAX_BODY {
        return Err(local_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "解压后的请求体过大",
        ));
    }
    Ok(Bytes::from(decoded))
}

async fn forward(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    headers: &HeaderMap,
    body: Option<Bytes>,
    selection: &RuntimeSelection,
    token: Option<&Token>,
) -> Result<reqwest::Response> {
    let mut request = client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST),
        url,
    );
    let mut safe = HeaderMap::new();
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "cookie"
                | "content-length"
                | "transfer-encoding"
                | "connection"
                | "proxy-authorization"
                | "origin"
                | "referer"
                | "sec-fetch-site"
                | "sec-fetch-mode"
                | "accept-encoding"
        ) {
            continue;
        }
        if !selection.official
            && (lower == "x-codex-turn-state"
                || lower.starts_with("chatgpt-")
                || lower == "originator"
                || lower == "version")
        {
            continue;
        }
        safe.append(name.clone(), value.clone());
    }
    safe.remove(header::CONTENT_ENCODING);
    if let Some(token) = token {
        safe.insert(
            HeaderName::from_static("x-codex-turn-state"),
            HeaderValue::from_str(&token.value).context("state header")?,
        );
    }
    request = request.headers(safe);
    if let Some(body) = body {
        request = request.body(body);
    }
    request.send().await.context("send upstream request")
}

fn probe_rejection_status(text: &str) -> Option<u16> {
    for status in [401u16, 403, 429] {
        if text.contains(&status.to_string()) {
            return Some(status);
        }
    }
    None
}

fn retry_after(response: &reqwest::Response) -> u64 {
    response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(60)
        .min(3600)
}

fn get_limit(state: &ProxyRuntime, key: &str) -> Arc<Mutex<Limit>> {
    let mut limits = state.inner.limits.lock().unwrap();
    limits
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(Limit::default())))
        .clone()
}

impl Limit {
    fn rejection(&self) -> Option<(StatusCode, u64)> {
        let now = Instant::now();
        if self.blocked {
            let seconds = self
                .retry_until
                .and_then(|until| until.checked_duration_since(now))
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            return Some((
                StatusCode::from_u16(self.status).unwrap_or(StatusCode::UNAUTHORIZED),
                seconds,
            ));
        }
        if let Some(until) = self.retry_until {
            if until > now {
                return Some((StatusCode::TOO_MANY_REQUESTS, (until - now).as_secs()));
            }
        }
        None
    }

    fn reject(&mut self, status: u16, seconds: u64) {
        self.status = status;
        self.retry_until = Some(Instant::now() + Duration::from_secs(seconds.max(30)));
        if status == 401 || status == 403 {
            self.blocked = true;
        }
    }
}

fn get_session(
    state: &ProxyRuntime,
    key: &str,
    model: &str,
    config: &Config,
    headers: &HeaderMap,
    limit: Arc<Mutex<Limit>>,
) -> Arc<AsyncMutex<Session>> {
    let mut sessions = state.inner.sessions.lock().unwrap();
    if let Some(session) = sessions.get(key) {
        return session.clone();
    }
    let (policy, _) = policy_for(config, headers, model);
    let mut safe = HeaderMap::new();
    for name in [
        "authorization",
        "chatgpt-account-id",
        "user-agent",
        "version",
        "originator",
        "openai-beta",
    ] {
        if let Some(value) = headers.get(name) {
            safe.insert(HeaderName::from_static(name), value.clone());
        }
    }
    let session = Arc::new(AsyncMutex::new(Session {
        store: Store::new(policy),
        headers: safe,
        limit,
        last_used: Instant::now(),
    }));
    sessions.insert(key.to_string(), session.clone());
    session
}

fn policy_for(config: &Config, headers: &HeaderMap, _model: &str) -> (Policy, String) {
    let hint = provider::plan_hint(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        headers
            .get("ChatGPT-Account-Id")
            .and_then(|value| value.to_str().ok()),
    );
    let (blocks, source) =
        provider::accepted_blocks(&config.codex.account_mode, 10, hint.as_deref());
    (
        Policy::new(
            blocks,
            config.codex.ttl_seconds,
            config.codex.refresh_seconds,
        ),
        source,
    )
}

async fn probe(
    config: &Config,
    selection: &RuntimeSelection,
    routes: &[Route],
    headers: &HeaderMap,
    model: &str,
    store: &mut Store,
) -> Result<Option<(Token, usize)>> {
    let mut attempts = 0;
    let mut last_error = None;
    for (index, route) in routes.iter().enumerate() {
        if attempts >= config.codex.max_probes {
            break;
        }
        attempts += 1;
        let body = serde_json::json!({
            "model": model,
            "instructions": "Reply with OK.",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Reply with OK."}]}],
            "stream": true,
            "store": false,
            "parallel_tool_calls": true,
            "include": ["reasoning.encrypted_content"]
        });
        let url = format!("{}/responses", selection.upstream);
        let mut request = route
            .client
            .post(&url)
            .timeout(Duration::from_secs(config.codex.probe_timeout_seconds))
            .header(header::ACCEPT, "text/event-stream");
        for name in [
            "authorization",
            "chatgpt-account-id",
            "user-agent",
            "version",
            "originator",
            "openai-beta",
        ] {
            if let Some(value) = headers.get(name) {
                request = request.header(name, value);
            }
        }
        let response = match request.json(&body).send().await {
            Ok(response) => response,
            Err(err) => {
                last_error = Some(format!("{}: {err}", route.id));
                continue;
            }
        };
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403 | 429) {
            bail!("upstream returned {}; stopping probes", status);
        }
        if !status.is_success() {
            last_error = Some(format!("{}: HTTP {}", route.id, status));
            continue;
        }
        let token_header = response
            .headers()
            .get(turnstate::HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let completed = read_probe_stream(response).await?;
        if !completed {
            last_error = Some(format!("{}: stream did not complete", route.id));
            continue;
        }
        if token_header.is_empty() {
            last_error = Some(format!("{}: no state header", route.id));
            continue;
        }
        let token = match turnstate::parse(&token_header) {
            Ok(token) => token,
            Err(err) => {
                last_error = Some(format!("{}: {err}", route.id));
                continue;
            }
        };
        if store.offer(token.clone(), index, SystemTime::now()) {
            return Ok(Some((token, index)));
        }
        last_error = Some(format!(
            "{}: state shape {} blocks was rejected",
            route.id, token.blocks
        ));
    }
    if let Some(error) = last_error {
        util::warn(format!("probe finished without state: {error}"));
    }
    Ok(None)
}

async fn read_probe_stream(response: reqwest::Response) -> Result<bool> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut completed = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read SSE chunk")?;
        if buffer.len() + chunk.len() > MAX_PROBE_BODY {
            bail!("probe stream too large");
        }
        buffer.extend_from_slice(&chunk);
        let text = String::from_utf8_lossy(&buffer);
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data.trim() == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
                match value.get("type").and_then(|value| value.as_str()) {
                    Some("response.completed") => completed = true,
                    Some("response.failed" | "response.incomplete" | "error") => {
                        bail!("probe stream reported a failure");
                    }
                    _ => {}
                }
            }
        }
        if completed {
            return Ok(true);
        }
    }
    Ok(false)
}

fn hash_key(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn load_selection(config: &Config, paths: &AppPaths) -> RuntimeSelection {
    if let Some(transaction) = config_tx::read_transaction(paths).ok().flatten() {
        return RuntimeSelection {
            upstream: transaction.upstream.clone(),
            official: transaction.official,
            auth_kind: transaction.auth_kind.clone(),
            owned: true,
        };
    }
    let upstream = config
        .codex
        .upstream
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| provider::OFFICIAL_UPSTREAM.to_string());
    let official = match config.codex.upstream_kind.as_str() {
        "official" => true,
        "relay" => false,
        _ => url::Url::parse(&upstream)
            .ok()
            .and_then(|url| {
                url.host_str()
                    .map(|host| host.eq_ignore_ascii_case("chatgpt.com"))
            })
            .unwrap_or(true),
    };
    RuntimeSelection {
        upstream,
        official,
        auth_kind: if official { "chatgpt" } else { "api_key" }.into(),
        owned: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn policy_accepts_expanded_shapes() {
        let config = Config::default();
        let headers = HeaderMap::new();
        let (policy, _) = policy_for(&config, &headers, "gpt-6-astra");
        assert!(policy.accepted_blocks.contains(&33));
        assert!(policy.accepted_blocks.contains(&10));
    }

    #[test]
    fn local_host_check_rejects_non_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.codex.listen = "127.0.0.1:18741".into();
        let paths = AppPaths {
            config_dir: dir.path().join("c"),
            data_dir: dir.path().join("d"),
            config_file: dir.path().join("c/config.json"),
            state_file: dir.path().join("d/state.json"),
            log_dir: dir.path().join("d/logs"),
            backups_dir: dir.path().join("d/backups"),
            lock_file: dir.path().join("d/lock"),
        };
        let runtime = ProxyRuntime::new(config, paths).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:18741"));
        assert!(runtime.check_local(&headers).is_ok());
        headers.insert(header::HOST, HeaderValue::from_static("evil.example:18741"));
        assert!(runtime.check_local(&headers).is_err());
    }
}
