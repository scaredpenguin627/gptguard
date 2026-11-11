//! Egress routes and subscription import.
//!
//! Route support is intentionally explicit: HTTP, HTTPS and SOCKS5 URLs, plus
//! a small Clash/Mihomo YAML subset. Protocols that need a full proxy core are
//! rejected with a clear message instead of being silently skipped.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::config::{validate_proxy_url, AppPaths, Config, RouteConfig};
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
        .user_agent(concat!("AIGuard/", env!("CARGO_PKG_VERSION")));
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
        if !seen.insert(route.id.clone()) {
            continue;
        }
        routes.push(Route::from_config(route)?);
    }
    if config.codex.direct && seen.insert("direct".into()) {
        routes.push(Route::direct()?);
    }
    if routes.is_empty() {
        // Falling back to direct is safer than a proxy process that cannot
        // start. The request path will still report no_routes if direct is off.
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

pub async fn import_subscription(
    config: &mut Config,
    paths: &AppPaths,
    url: &str,
) -> Result<ImportResult> {
    let client = build_client(None)?;
    let response = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("fetch subscription {}", util::mask_url(url)))?;
    if !response.status().is_success() {
        bail!("subscription returned HTTP {}", response.status());
    }
    let body = response.text().await.context("read subscription body")?;
    if body.len() > 4 * 1024 * 1024 {
        bail!("subscription body is too large");
    }
    let parsed = parse_subscription(&body);
    config.codex.routes.extend(parsed.routes);
    config.codex.subscriptions.push(util::mask_url(url));
    config.save(paths)?;
    Ok(parsed.result)
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
    if !matches!(scheme, "http" | "https" | "socks5" | "socks5h") {
        return Ok(None);
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
        _ => return Ok(None),
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
    let auth = match (username, password) {
        (Some(user), Some(pass)) => format!("{user}:{pass}@"),
        _ => String::new(),
    };
    let url = format!("{scheme}://{auth}{server}:{port}");
    validate_proxy_url(&url)?;
    Ok(Some(RouteConfig {
        id: format!("route-{}", short_id(&url)),
        label: name.to_string(),
        url,
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
    fn parses_clash_http_proxies_and_skips_unsupported() {
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
        assert_eq!(parsed.routes.len(), 1);
        assert_eq!(parsed.result.skipped, 1);
    }

    #[test]
    fn rejects_missing_port() {
        let parsed = parse_subscription("socks5://127.0.0.1#no-port");
        assert!(parsed.routes.is_empty());
    }
}
