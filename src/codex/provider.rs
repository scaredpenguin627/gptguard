//! Codex provider selection, preserving the difference between ChatGPT login
//! and an API-key relay.
//!
//! This is the port of the issue #10 fix: `requires_openai_auth = true` is an
//! authentication statement, not a missing relay URL.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml::Value;

use crate::config::{valid_model_id, AccountMode, CodexConfig};

pub const OFFICIAL_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";
pub const API_UPSTREAM: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    pub provider_id: String,
    pub profile: Option<String>,
    #[serde(skip)]
    pub upstream: String,
    pub auth_kind: String,
    pub env_key: Option<String>,
    pub official: bool,
    pub requires_openai_auth: bool,
    #[serde(skip)]
    pub config_sha256: String,
}

impl Selection {
    pub fn is_relay(&self) -> bool {
        !self.official
    }
}

pub fn resolve(original: &str, profile: Option<&str>, auth_mode: &str) -> Result<Selection> {
    match auth_mode {
        "" | "unknown" | "api_key" | "chatgpt" | "ambiguous" => {}
        _ => bail!("unknown Codex authentication category"),
    }
    let doc: Value = toml::from_str(original).context("Codex config is not valid TOML")?;
    let table = doc.as_table().context("Codex config must be a table")?;

    let profile = if let Some(profile) = profile {
        Some(profile.to_string())
    } else {
        match table.get("profile") {
            Some(Value::String(value)) => Some(value.clone()),
            Some(_) => bail!("Codex profile must be a string"),
            None => None,
        }
    };

    let selected: &toml::map::Map<String, Value> = if let Some(profile) = &profile {
        let profiles = table
            .get("profiles")
            .and_then(Value::as_table)
            .context("selected Codex profile does not exist")?;
        let profile_table = profiles
            .get(profile)
            .and_then(Value::as_table)
            .with_context(|| format!("selected Codex profile {profile} does not exist"))?;
        // A merged view is enough for resolution; installation keeps the
        // original document and only edits the top-level provider.
        let mut merged = table.clone();
        for (key, value) in profile_table {
            merged.insert(key.clone(), value.clone());
        }
        Box::leak(Box::new(merged))
    } else {
        table
    };

    let id = match selected.get("model_provider") {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        Some(Value::String(_)) | None => "openai".to_string(),
        Some(_) => bail!("Codex model_provider must be a string"),
    };
    if id == "gptguard" {
        bail!("managed provider is already selected; restore the previous transaction first");
    }

    let providers = table.get("model_providers").and_then(Value::as_table);
    let fields = providers
        .and_then(|providers| providers.get(&id))
        .and_then(Value::as_table);
    if fields.is_none() && id != "openai" {
        bail!("selected provider has no definition");
    }
    let fields = fields.cloned().unwrap_or_default();

    if let Some(wire) = fields.get("wire_api").and_then(Value::as_str) {
        if wire != "responses" {
            bail!("only Responses API providers are supported");
        }
    }

    let explicit_requires = fields.get("requires_openai_auth").and_then(Value::as_bool);
    let env_key = fields
        .get("env_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let mut endpoint = fields
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if endpoint.is_empty() {
        endpoint = selected
            .get("openai_base_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }

    // Issue #10: a custom provider that explicitly requires OpenAI auth and
    // omits base_url still means the built-in OpenAI upstream. It is not a
    // malformed relay.
    if endpoint.is_empty() {
        if id == "openai" {
            endpoint = if auth_mode == "api_key" {
                API_UPSTREAM.to_string()
            } else {
                OFFICIAL_UPSTREAM.to_string()
            };
        } else if explicit_requires == Some(true) {
            endpoint = if auth_mode == "api_key" {
                API_UPSTREAM.to_string()
            } else {
                OFFICIAL_UPSTREAM.to_string()
            };
        } else {
            bail!(
                "所选 provider {id} 缺少 base_url。请补全 Responses API 地址；不会猜测上游，也不会把官方登录凭据发给中转"
            );
        }
    }

    validate_endpoint(&endpoint)?;
    let parsed = url::Url::parse(&endpoint).expect("validated URL");
    let official = parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("chatgpt.com"))
        && parsed.path().trim_end_matches('/') == "/backend-api/codex";
    let upstream = endpoint.trim_end_matches('/').to_string();

    let requires =
        explicit_requires.unwrap_or(id == "openai" && (official || auth_mode == "api_key"));
    if id == "openai"
        && !official
        && explicit_requires.is_none()
        && env_key.is_none()
        && auth_mode != "api_key"
    {
        bail!(
            "OpenAI base URL 被覆盖但认证方式不明确；请选择带 env_key 或 requires_openai_auth=false 的中转 provider"
        );
    }
    if requires && !official && auth_mode != "api_key" {
        bail!("中转 provider 要求 OpenAI 账号认证；拒绝把账号凭据发到不同主机");
    }
    if requires && auth_mode == "ambiguous" {
        bail!("Codex 凭据混合或无效；请先选择一种登录方式");
    }
    let auth_kind = if requires && auth_mode != "api_key" {
        "chatgpt".to_string()
    } else {
        "api_key".to_string()
    };

    let digest = Sha256::digest(original.as_bytes());
    Ok(Selection {
        provider_id: id,
        profile,
        upstream,
        auth_kind,
        env_key,
        official,
        requires_openai_auth: requires,
        config_sha256: digest.iter().map(|b| format!("{b:02x}")).collect(),
    })
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    let parsed = url::Url::parse(endpoint).context("provider base_url is not a URL")?;
    if parsed.host_str().is_none() {
        bail!("provider base_url must have a host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("provider base_url must not contain credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("provider base_url must not contain a query or fragment");
    }
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "::1" | "localhost"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        bail!("provider base_url must use HTTPS (loopback HTTP is allowed)");
    }
    if parsed.path().contains("..") || parsed.path().contains('\\') {
        bail!("provider base_url path must not contain traversal");
    }
    Ok(())
}

/// Calibration is independent of the model catalog. Any new `gpt-*` model is
/// calibrated by default; auxiliary models are forwarded without probes.
pub fn is_calibration_model(config: &CodexConfig, model: &str) -> bool {
    if config
        .auxiliary_models
        .iter()
        .any(|entry| entry.eq_ignore_ascii_case(model))
    {
        return false;
    }
    if config
        .auxiliary_prefixes
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return false;
    }
    if config
        .calibration_prefixes
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return true;
    }
    config.models.iter().any(|entry| entry == model)
}

/// Auxiliary Codex models (auto-review, titles, etc.) are forwarded unchanged.
/// Only syntax is checked so a model name cannot become a header/URL injection.
pub fn valid_model_name(model: &str) -> bool {
    valid_model_id(model)
}

pub fn accepted_blocks(
    mode: &AccountMode,
    baseline: usize,
    plan_hint: Option<&str>,
) -> (Vec<usize>, String) {
    accepted_blocks_with_extra(mode, baseline, plan_hint, &[])
}

pub fn accepted_blocks_with_extra(
    mode: &AccountMode,
    baseline: usize,
    plan_hint: Option<&str>,
    extra: &[usize],
) -> (Vec<usize>, String) {
    let (mut blocks, source) = match mode {
        AccountMode::Personal => (vec![10, 33], "manual".to_string()),
        AccountMode::Team => (vec![12, 33], "manual".to_string()),
        AccountMode::Auto => {
            if baseline > 0 && baseline != 10 {
                (vec![baseline], "legacy_baseline".to_string())
            } else {
                match plan_hint {
                    Some("team") => (vec![12, 33], "token_hint".to_string()),
                    Some("personal") => (vec![10, 33], "token_hint".to_string()),
                    _ => (vec![10, 33], "default".to_string()),
                }
            }
        }
    };
    for value in extra {
        if !blocks.contains(value) {
            blocks.push(*value);
        }
    }
    blocks.sort_unstable();
    (blocks, source)
}

pub fn plan_hint(authorization: Option<&str>, account_header: Option<&str>) -> Option<String> {
    let auth = authorization?.trim();
    let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    if payload.len() > 12_288 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    let auth = value.get("https://api.openai.com/auth")?;
    let plan = auth
        .get("chatgpt_plan_type")?
        .as_str()?
        .to_ascii_lowercase();
    let account = auth.get("chatgpt_account_id").and_then(|v| v.as_str());
    if let (Some(selected), Some(account)) = (account_header, account) {
        if selected != account {
            return None;
        }
    }
    match plan.as_str() {
        "team" | "business" => Some("team".into()),
        "free" | "plus" | "pro" => Some("personal".into()),
        _ => None,
    }
}

pub fn official_credential(authorization: Option<&str>) -> bool {
    let Some(auth) = authorization else {
        return false;
    };
    let token = auth.trim().strip_prefix("Bearer ").unwrap_or(auth.trim());
    let mut parts = token.split('.');
    let (_, payload, _) = match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return false,
    };
    if parts.next().is_some() {
        return false;
    }
    let Ok(payload) = URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&payload) else {
        return false;
    };
    if value.get("https://api.openai.com/auth").is_some() {
        return true;
    }
    value
        .get("iss")
        .and_then(|v| v.as_str())
        .and_then(|issuer| url::Url::parse(issuer).ok())
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "auth.openai.com" || host == "auth0.openai.com")
}

/// Small helper used by the panel: keep the upstream host, hide credentials.
pub fn upstream_label(selection: &Selection) -> String {
    match url::Url::parse(&selection.upstream) {
        Ok(url) => format!("{}{}", url.host_str().unwrap_or("unknown"), url.path()),
        Err(_) => selection.upstream.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_10_custom_openai_auth_without_base_url_uses_official_upstream() {
        let config = r#"
model_provider = "openai_retry"

[model_providers.openai_retry]
name = "OpenAI"
requires_openai_auth = true
wire_api = "responses"
"#;
        let selection = resolve(config, None, "chatgpt").unwrap();
        assert_eq!(selection.provider_id, "openai_retry");
        assert_eq!(selection.upstream, OFFICIAL_UPSTREAM);
        assert!(selection.official);
        assert_eq!(selection.auth_kind, "chatgpt");
    }

    #[test]
    fn relay_with_base_url_is_not_official() {
        let config = r#"
model_provider = "relay"

[model_providers.relay]
name = "Relay"
base_url = "https://relay.example/v1"
env_key = "RELAY_KEY"
wire_api = "responses"
"#;
        let selection = resolve(config, None, "api_key").unwrap();
        assert_eq!(selection.upstream, "https://relay.example/v1");
        assert!(!selection.official);
        assert_eq!(selection.auth_kind, "api_key");
    }

    #[test]
    fn relay_cannot_request_account_auth() {
        let config = r#"
model_provider = "relay"

[model_providers.relay]
base_url = "https://relay.example/v1"
requires_openai_auth = true
"#;
        assert!(resolve(config, None, "chatgpt").is_err());
    }

    #[test]
    fn legacy_custom_baseline_is_not_expanded() {
        let (blocks, source) = accepted_blocks(&AccountMode::Auto, 11, None);
        assert_eq!(blocks, vec![11]);
        assert_eq!(source, "legacy_baseline");
        let (blocks, _) = accepted_blocks(&AccountMode::Auto, 10, None);
        assert_eq!(blocks, vec![10, 33]);
    }

    #[test]
    fn model_names_are_syntax_checked_not_allowlisted() {
        assert!(valid_model_name("gpt-5.6-sol"));
        assert!(valid_model_name("codex-auto-review"));
        assert!(!valid_model_name("bad model"));
        assert!(!valid_model_name("bad\nmodel"));
        let config = CodexConfig::default();
        assert!(!is_calibration_model(&config, "codex-auto-review"));
        assert!(is_calibration_model(&config, "gpt-6-astra"));
        assert!(is_calibration_model(&config, "gpt-6.1-sol"));
        assert!(is_calibration_model(&config, "gpt-6.2-astra"));
        assert!(is_calibration_model(&config, "gpt-7-future"));
    }
}
