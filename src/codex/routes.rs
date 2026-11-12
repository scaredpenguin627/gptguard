//! Egress routes and subscription import.
//!
//! Native HTTP/SOCKS routes and isolated Mihomo outbound nodes.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{detect_local_proxy, validate_proxy_url, Config, RouteConfig};
use crate::util;

#[derive(Debug, Clone)]
pub struct Route {
    pub id: String,
    pub label: String,
    pub url: Option<String>,
    pub client: reqwest::Client,
}

impl Route {
    pub fn direct() -> Result<Self> {
        Ok(Self {
            id: "direct".into(),
            label: "直连".into(),
            url: None,
            client: build_client(None)?,
        })
    }

    pub fn from_config(route: &RouteConfig) -> Result<Self> {
        validate_proxy_url(&route.url)?;
        Ok(Self {
            id: route.id.clone(),
            label: route.label.clone(),
            url: Some(route.url.clone()),
            client: build_client(Some(&route.url))?,
        })
    }

    pub fn is_direct(&self) -> bool {
        self.url.is_none()
    }
}

pub fn build_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30 * 60))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("GPTGuard/", env!("CARGO_PKG_VERSION")));
    match proxy {
        Some(proxy) => {
            builder = builder.proxy(reqwest::Proxy::all(proxy).context("invalid proxy URL")?);
        }
        None => {
            // Do not inherit HTTP_PROXY/HTTPS_PROXY for a "direct" route; the
            // whole point of an explicit direct route is the direct path.
            builder = builder.no_proxy();
        }
    }
    builder.build().context("build HTTP client")
}

pub fn build_routes(config: &Config) -> Result<Vec<Route>> {
    let mut routes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for route in config.codex.routes.iter().filter(|route| route.enabled) {
        if route.mihomo.is_some() {
            continue; // Requires a managed sidecar; never reinterpret as direct.
        }
        if !seen.insert(route.id.clone()) {
            continue;
        }
        routes.push(Route::from_config(route)?);
    }
    if config.codex.direct && seen.insert("direct".into()) {
        routes.push(Route::direct()?);
    }
    Ok(routes)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteTest {
    pub id: String,
    pub label: String,
    pub ok: bool,
    pub latency_ms: u128,
    pub detail: String,
}

pub async fn test_route(route: &Route) -> RouteTest {
    let started = std::time::Instant::now();
    let result = route
        .client
        .head("https://chatgpt.com/")
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let latency = started.elapsed().as_millis();
    match result {
        Ok(response) => RouteTest {
            id: route.id.clone(),
            label: route.label.clone(),
            ok: true,
            latency_ms: latency,
            detail: format!("connected (HTTP {})", response.status().as_u16()),
        },
        Err(err) => RouteTest {
            id: route.id.clone(),
            label: route.label.clone(),
            ok: false,
            latency_ms: latency,
            detail: format!("{err}"),
        },
    }
}

pub async fn test_all(routes: &[Route]) -> Vec<RouteTest> {
    let mut out = Vec::new();
    for route in routes {
        out.push(test_route(route).await);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub added: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

pub async fn import_subscription(config: &mut Config, url: &str) -> Result<ImportResult> {
    let detected_proxy = detect_local_proxy();
    let client = build_client(detected_proxy.as_deref())?;
    let response = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("fetch subscription failed"))?;
    if !response.status().is_success() {
        bail!("subscription returned HTTP {}", response.status());
    }
    let body = response
        .text()
        .await
        .map_err(|_| anyhow::anyhow!("read subscription body failed"))?;
    if body.len() > 4 * 1024 * 1024 {
        bail!("subscription body is too large");
    }
    let parsed = parse_subscription(&body);
    merge_routes(config, parsed.routes);
    config.codex.subscriptions.push(util::mask_url(url));
    Ok(parsed.result)
}

pub fn merge_routes(config: &mut Config, routes: Vec<RouteConfig>) {
    if !routes.is_empty() {
        config.codex.direct = false;
        // Every imported route is normalized through the embedded Mihomo
        // supervisor.  That keeps SS/VMess/VLESS/Trojan/Hysteria and ordinary
        // HTTP/SOCKS proxies on the same loopback-only, fail-closed path.
        config.mihomo.enabled = true;
        config.protection.ip_check_mode = crate::config::IpCheckMode::Http;
    }
    for route in routes {
        if let Some(existing) = config.codex.routes.iter_mut().find(|r| r.id == route.id) {
            *existing = route;
        } else {
            config.codex.routes.push(route);
        }
    }
}

pub fn parse_subscription(body: &str) -> ParsedSubscription {
    let mut routes = Vec::new();
    let mut errors = Vec::new();
    let mut skipped = 0;
    let yaml = serde_yaml::from_str::<serde_yaml::Value>(body);
    if let Ok(value) = yaml {
        if let Some(proxies) = value.get("proxies").and_then(|value| value.as_sequence()) {
            for proxy in proxies {
                match proxy_to_route(proxy) {
                    Ok(Some(route)) => routes.push(route),
                    Ok(None) => skipped += 1,
                    Err(err) => errors.push(err.to_string()),
                }
            }
            let added = routes.len();
            return ParsedSubscription {
                routes,
                result: ImportResult {
                    added,
                    skipped,
                    errors,
                },
            };
        }
    }

    let text = decode_subscription_text(body);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_proxy_line(line) {
            Ok(Some(route)) => routes.push(route),
            Ok(None) => skipped += 1,
            Err(err) => errors.push(err.to_string()),
        }
    }
    let added = routes.len();
    ParsedSubscription {
        routes,
        result: ImportResult {
            added,
            skipped,
            errors,
        },
    }
}

pub struct ParsedSubscription {
    pub routes: Vec<RouteConfig>,
    pub result: ImportResult,
}

fn decode_subscription_text(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.contains("://") {
        return trimmed.to_string();
    }
    let compact = trimmed.trim_end_matches('=');
    for engine in [&URL_SAFE_NO_PAD, &URL_SAFE, &STANDARD] {
        if let Ok(bytes) = engine.decode(compact) {
            if let Ok(text) = String::from_utf8(bytes) {
                if text.contains("://") {
                    return text;
                }
            }
        }
    }
    trimmed.to_string()
}

fn parse_proxy_line(line: &str) -> Result<Option<RouteConfig>> {
    let without_fragment = line.split('#').next().unwrap_or(line);
    let parsed = match url::Url::parse(without_fragment) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None),
    };
    let scheme = parsed.scheme();
    if matches!(
        scheme,
        "ss" | "vmess" | "vless" | "trojan" | "hysteria" | "hysteria2"
    ) {
        return parse_sidecar_uri(line, without_fragment, scheme);
    }
    if !matches!(scheme, "http" | "https" | "socks5" | "socks5h") {
        bail!("此协议请使用 Clash/Mihomo YAML 节点导入");
    }
    if parsed.host_str().is_none() || parsed.port().is_none() {
        return Ok(None);
    }
    let label = line
        .split('#')
        .nth(1)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}:{}",
                parsed.host_str().unwrap_or("proxy"),
                parsed.port().unwrap_or(0)
            )
        });
    Ok(Some(RouteConfig {
        id: format!("route-{}", short_id(without_fragment)),
        label,
        url: without_fragment.to_string(),
        mihomo: None,
        enabled: true,
    }))
}

fn proxy_to_route(proxy: &serde_yaml::Value) -> Result<Option<RouteConfig>> {
    let kind = proxy
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let scheme = match kind.as_str() {
        "http" => "http",
        "https" => "https",
        "socks5" | "socks" => "socks5",
        _ => return sidecar_route(proxy),
    };
    let server = proxy
        .get("server")
        .and_then(|value| value.as_str())
        .context("proxy has no server")?;
    let port = proxy
        .get("port")
        .and_then(|value| value.as_u64())
        .context("proxy has no port")?;
    let name = proxy
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("proxy");
    let username = proxy.get("username").and_then(|value| value.as_str());
    let password = proxy.get("password").and_then(|value| value.as_str());
    if port == 0 || port > 65535 {
        bail!("invalid proxy port");
    }
    // Preserve TLS and other protocol options for Mihomo instead of silently
    // discarding them when translating a YAML node into a URL.
    if proxy.as_mapping().is_some_and(|map| {
        map.keys().any(|key| {
            !matches!(
                key.as_str(),
                Some("name" | "type" | "server" | "port" | "username" | "password")
            )
        })
    }) {
        return sidecar_route(proxy);
    }
    let host = if server.contains(':') {
        format!("[{server}]")
    } else {
        server.to_string()
    };
    let mut parsed = url::Url::parse(&format!("{scheme}://{host}:{port}"))?;
    if let Some(user) = username {
        parsed
            .set_username(user)
            .map_err(|_| anyhow::anyhow!("invalid proxy username"))?;
    }
    if let Some(pass) = password {
        parsed
            .set_password(Some(pass))
            .map_err(|_| anyhow::anyhow!("invalid proxy password"))?;
    }
    let url = parsed.to_string();
    validate_proxy_url(&url)?;
    Ok(Some(RouteConfig {
        id: format!("route-{}", short_id(&url)),
        label: name.to_string(),
        url,
        mihomo: None,
        enabled: true,
    }))
}

fn parse_sidecar_uri(
    original: &str,
    without_fragment: &str,
    scheme: &str,
) -> Result<Option<RouteConfig>> {
    let label = original
        .split_once('#')
        .map(|(_, label)| percent_decode(label))
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| scheme.to_string());
    let node = match scheme {
        "ss" => parse_ss_uri(without_fragment)?,
        "vmess" => parse_vmess_uri(without_fragment)?,
        "vless" => parse_vless_uri(without_fragment)?,
        "trojan" => parse_trojan_uri(without_fragment)?,
        "hysteria" | "hysteria2" => parse_hysteria_uri(without_fragment, scheme)?,
        _ => unreachable!(),
    };
    crate::mihomo::validate_node(&node).map_err(|_| anyhow::anyhow!("无效的 {scheme} URI"))?;
    Ok(Some(RouteConfig {
        id: format!("node-{}", short_id(without_fragment)),
        label,
        url: String::new(),
        mihomo: Some(node),
        enabled: true,
    }))
}

fn parse_ss_uri(value: &str) -> Result<Value> {
    let mut parsed = url::Url::parse(value).map_err(|_| anyhow::anyhow!("无效的 ss URI"))?;
    if parsed.host_str().is_none() || parsed.port().is_none() {
        let encoded = value
            .strip_prefix("ss://")
            .ok_or_else(|| anyhow::anyhow!("无效的 ss URI"))?;
        let decoded = decode_uri_base64(encoded)?;
        parsed = url::Url::parse(&format!("ss://{decoded}"))
            .map_err(|_| anyhow::anyhow!("无效的 ss URI"))?;
    }
    let (cipher, password) = if !parsed.username().is_empty() {
        if let Ok(decoded) = decode_uri_base64(parsed.username()) {
            split_ss_credentials(&decoded)?
        } else if parsed.username().contains(':') {
            split_ss_credentials(parsed.username())?
        } else {
            (
                percent_decode(parsed.username()),
                parsed.password().map(percent_decode).unwrap_or_default(),
            )
        }
    } else {
        let encoded = value
            .strip_prefix("ss://")
            .and_then(|raw| raw.split_once('@').map(|(user, _)| user))
            .ok_or_else(|| anyhow::anyhow!("无效的 ss URI"))?;
        split_ss_credentials(&decode_uri_base64(encoded)?)?
    };
    Ok(json!({
        "type": "ss",
        "server": parsed.host_str().unwrap_or_default(),
        "port": parsed.port().unwrap_or(0),
        "cipher": cipher,
        "password": password,
    }))
}

fn split_ss_credentials(value: &str) -> Result<(String, String)> {
    let (cipher, password) = value
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("无效的 ss URI"))?;
    if cipher.is_empty() || password.is_empty() {
        bail!("无效的 ss URI");
    }
    Ok((percent_decode(cipher), percent_decode(password)))
}

fn parse_vmess_uri(value: &str) -> Result<Value> {
    let encoded = value
        .strip_prefix("vmess://")
        .ok_or_else(|| anyhow::anyhow!("无效的 vmess URI"))?;
    let raw = decode_uri_base64(encoded)?;
    let source: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("无效的 vmess URI"))?;
    let server = source["add"].as_str().unwrap_or_default();
    let port = source["port"]
        .as_u64()
        .or_else(|| source["port"].as_str().and_then(|v| v.parse().ok()))
        .unwrap_or(0);
    let uuid = source["id"].as_str().unwrap_or_default();
    let mut node = json!({
        "type": "vmess",
        "server": server,
        "port": port,
        "uuid": uuid,
        "alterId": source["aid"].as_u64().unwrap_or(0),
        "cipher": source["scy"].as_str().unwrap_or("auto"),
        "udp": true,
    });
    apply_transport(&mut node, &source);
    Ok(node)
}

fn parse_vless_uri(value: &str) -> Result<Value> {
    let parsed = url::Url::parse(value).map_err(|_| anyhow::anyhow!("无效的 vless URI"))?;
    let mut node = json!({
        "type": "vless",
        "server": parsed.host_str().unwrap_or_default(),
        "port": parsed.port().unwrap_or(0),
        "uuid": percent_decode(parsed.username()),
        "udp": true,
    });
    apply_query_transport(&mut node, &parsed);
    Ok(node)
}

fn parse_trojan_uri(value: &str) -> Result<Value> {
    let parsed = url::Url::parse(value).map_err(|_| anyhow::anyhow!("无效的 trojan URI"))?;
    let mut node = json!({
        "type": "trojan",
        "server": parsed.host_str().unwrap_or_default(),
        "port": parsed.port().unwrap_or(0),
        "password": percent_decode(parsed.username()),
        "udp": true,
    });
    apply_query_transport(&mut node, &parsed);
    node["tls"] = json!(true);
    Ok(node)
}

fn parse_hysteria_uri(value: &str, scheme: &str) -> Result<Value> {
    let parsed = url::Url::parse(value).map_err(|_| anyhow::anyhow!("无效的 {scheme} URI"))?;
    let mut node = json!({
        "type": scheme,
        "server": parsed.host_str().unwrap_or_default(),
        "port": parsed.port().unwrap_or(0),
        "auth-str": percent_decode(parsed.username()),
        "udp": true,
    });
    if scheme == "hysteria2" {
        node["password"] = json!(percent_decode(parsed.username()));
        node.as_object_mut().unwrap().remove("auth-str");
    }
    apply_query_transport(&mut node, &parsed);
    Ok(node)
}

fn apply_transport(node: &mut Value, source: &Value) {
    let network = source["net"].as_str().unwrap_or_default();
    if !network.is_empty() {
        node["network"] = json!(network);
    }
    if source["tls"].as_str().is_some_and(|v| v == "tls") || source["tls"].as_bool() == Some(true) {
        node["tls"] = json!(true);
    }
    if let Some(host) = source["host"].as_str().filter(|v| !v.is_empty()) {
        node["ws-opts"]["headers"]["Host"] = json!(host);
    }
    if let Some(path) = source["path"].as_str().filter(|v| !v.is_empty()) {
        node["ws-opts"]["path"] = json!(path);
    }
    if let Some(sni) = source["sni"].as_str().filter(|v| !v.is_empty()) {
        node["servername"] = json!(sni);
    }
}

fn apply_query_transport(node: &mut Value, parsed: &url::Url) {
    let query = parsed
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let network = query.get("type").map(|v| v.as_ref()).unwrap_or_default();
    if !network.is_empty() {
        node["network"] = json!(network);
    }
    if query
        .get("security")
        .is_some_and(|v| v == "tls" || v == "reality")
    {
        node["tls"] = json!(true);
    }
    if let Some(sni) = query.get("sni").filter(|v| !v.is_empty()) {
        node["servername"] = json!(sni.as_ref());
    }
    if let Some(path) = query.get("path").filter(|v| !v.is_empty()) {
        node["ws-opts"]["path"] = json!(path.as_ref());
    }
    if let Some(host) = query.get("host").filter(|v| !v.is_empty()) {
        node["ws-opts"]["headers"]["Host"] = json!(host.as_ref());
    }
    if let Some(public_key) = query.get("pbk").filter(|v| !v.is_empty()) {
        node["reality-opts"]["public-key"] = json!(public_key.as_ref());
    }
    if let Some(short_id) = query.get("sid").filter(|v| !v.is_empty()) {
        node["reality-opts"]["short-id"] = json!(short_id.as_ref());
    }
}

fn decode_uri_base64(value: &str) -> Result<String> {
    let compact = value.trim().trim_end_matches('=');
    for engine in [&URL_SAFE_NO_PAD, &URL_SAFE, &STANDARD] {
        if let Ok(bytes) = engine.decode(compact) {
            if let Ok(text) = String::from_utf8(bytes) {
                return Ok(text);
            }
        }
    }
    bail!("无效的 URI 编码")
}

fn percent_decode(value: &str) -> String {
    url::form_urlencoded::parse(format!("x={value}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| value.to_string())
}

fn sidecar_route(proxy: &serde_yaml::Value) -> Result<Option<RouteConfig>> {
    let mut node = serde_json::to_value(proxy).context("invalid node data")?;
    match node.get("type").and_then(|v| v.as_str()) {
        Some("https") => {
            node["type"] = serde_json::json!("http");
            node["tls"] = serde_json::json!(true);
        }
        Some("socks") => node["type"] = serde_json::json!("socks5"),
        _ => {}
    }
    crate::mihomo::validate_node(&node)?;
    let label = node
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("proxy")
        .to_string();
    node.as_object_mut().unwrap().remove("name");
    Ok(Some(RouteConfig {
        id: format!("node-{}", short_id(&node.to_string())),
        label,
        url: String::new(),
        mihomo: Some(node),
        enabled: true,
    }))
}

fn short_id(value: &str) -> String {
    let hash = util::sha256_hex(value.as_bytes());
    hash[..10].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uri_list() {
        let body = "socks5://u:p@127.0.0.1:1080#HK\nhttp://127.0.0.1:7890\n";
        let parsed = parse_subscription(body);
        assert_eq!(parsed.routes.len(), 2);
        assert_eq!(parsed.routes[0].label, "HK");
    }

    #[test]
    fn parses_base64_subscription() {
        let body = URL_SAFE_NO_PAD.encode("http://127.0.0.1:7890#A\nhttp://127.0.0.1:7891#B\n");
        let parsed = parse_subscription(&body);
        assert_eq!(parsed.routes.len(), 2);
    }

    #[test]
    fn parses_common_sidecar_uri_protocols() {
        let vmess = URL_SAFE_NO_PAD.encode(
            serde_json::to_string(&json!({
                "v": "2", "ps": "vmess", "add": "example.test", "port": 443,
                "id": "00000000-0000-0000-0000-000000000000", "aid": 0,
                "net": "ws", "path": "/codex", "host": "example.test", "tls": "tls"
            }))
            .unwrap(),
        );
        let ss = URL_SAFE_NO_PAD.encode("aes-128-gcm:secret@example.test:8388");
        let body = format!(
            "ss://{ss}#ss\nvmess://{vmess}#vmess\nvless://00000000-0000-0000-0000-000000000000@example.test:443?type=ws&security=tls&path=%2Fvless#vless\ntrojan://secret@example.test:443?sni=example.test#trojan\nhysteria://secret@example.test:443?sni=example.test#hysteria\nhysteria2://secret@example.test:443?sni=example.test#hysteria2\n"
        );
        let parsed = parse_subscription(&body);
        assert_eq!(parsed.routes.len(), 6, "{:?}", parsed.result.errors);
        let types = parsed
            .routes
            .iter()
            .map(|route| route.mihomo.as_ref().unwrap()["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            ["ss", "vmess", "vless", "trojan", "hysteria", "hysteria2"]
        );
    }

    #[test]
    fn parses_clash_http_and_sidecar_nodes() {
        let body = r#"
proxies:
  - name: local
    type: http
    server: 127.0.0.1
    port: 7890
  - name: ss-node
    type: ss
    server: example.com
    port: 8388
"#;
        let parsed = parse_subscription(body);
        assert_eq!(parsed.routes.len(), 2);
        assert_eq!(parsed.result.skipped, 0);
        assert_eq!(parsed.routes[1].mihomo.as_ref().unwrap()["type"], "ss");
    }

    #[test]
    fn rejects_missing_port() {
        let parsed = parse_subscription("socks5://127.0.0.1#no-port");
        assert!(parsed.routes.is_empty());
    }

    #[test]
    fn empty_pool_never_falls_back_to_direct() {
        let mut cfg = Config::default();
        cfg.codex.direct = false;
        assert!(build_routes(&cfg).unwrap().is_empty());
    }

    #[test]
    fn merging_imported_routes_enables_embedded_mihomo() {
        let mut cfg = Config::default();
        let parsed = parse_subscription(
            "proxies:\n  - {name: ss, type: ss, server: example.test, port: 443, cipher: aes-128-gcm, password: secret}",
        );
        merge_routes(&mut cfg, parsed.routes);
        assert!(cfg.mihomo.enabled);
        assert!(!cfg.codex.direct);
        assert_eq!(
            cfg.protection.ip_check_mode,
            crate::config::IpCheckMode::Http
        );
    }

    #[test]
    fn imports_protocol_fields_but_not_top_level_settings() {
        let parsed = parse_subscription(
            r#"
tun: {enable: true}
external-controller: 0.0.0.0:9090
rules: [MATCH,DIRECT]
listeners: [{type: http, port: 5555}]
proxies:
  - {name: a, type: ss, server: example.test, port: 443, cipher: aes-128-gcm, password: secret}
  - {name: b, type: vmess, server: example.test, port: 443, uuid: test, alterId: 0, network: ws, ws-opts: {path: /ws}}
  - {name: c, type: vless, server: example.test, port: 443, uuid: test, tls: true, reality-opts: {public-key: key, short-id: ab}}
  - {name: d, type: trojan, server: example.test, port: 443, password: secret, sni: tls.test}
  - {name: e, type: hysteria, server: example.test, port: 443, auth-str: secret, up: 10, down: 50}
  - {name: f, type: hysteria2, server: example.test, port: 443, password: secret, obfs: salamander, obfs-password: obfs}
"#,
        );
        assert_eq!(parsed.routes.len(), 6, "{:?}", parsed.result.errors);
        assert_eq!(
            parsed.routes[2].mihomo.as_ref().unwrap()["reality-opts"]["public-key"],
            "key"
        );
        assert_eq!(
            parsed.routes[1].mihomo.as_ref().unwrap()["ws-opts"]["path"],
            "/ws"
        );
        for route in parsed.routes {
            assert!(route.url.is_empty());
            assert!(route.mihomo.as_ref().unwrap().get("tun").is_none());
        }
        let bad = parse_subscription("proxies:\n  - {type: vless, server: x, port: 443, dialer-proxy: DIRECT, password: SECRET}");
        assert!(bad.routes.is_empty());
        assert_eq!(bad.result.errors.len(), 1);
        assert!(!bad.result.errors[0].contains("SECRET"));
    }
}
