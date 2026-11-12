//! Configuration, paths and validation.
//!
//! The file is deliberately one JSON document so the CLI, the tray, the web
//! panel and the daemon all read exactly the same settings.

use std::fs;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ipnet::IpNet;
use rand::RngCore;
use serde::{Deserialize, Serialize};

pub const APP_NAME: &str = "GPTGuard";
pub const DISPLAY_NAME: &str = "GPTGuard";
pub const DEFAULT_PORT: u16 = 17841;
pub const DEFAULT_MODEL: &str = "gpt-6-astra";
/// Advisory catalog only. Codex accepts model names from the upstream, and a
/// release can add models faster than this binary. Validation never rejects an
/// unknown model solely because it is absent here.
pub const DEFAULT_MODELS: [&str; 7] = [
    "gpt-6-astra",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6.1-sol",
    "gpt-6.1-terra",
    "gpt-6.2-astra",
    "gpt-6.2-sol",
];
const CONFIG_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpCheckMode {
    Both,
    Http,
    Stun,
}

impl Default for IpCheckMode {
    fn default() -> Self {
        Self::Both
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Cli,
    Desktop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    pub name: String,
    pub path: String,
    pub kind: TargetKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtectionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ip_check_mode: IpCheckMode,
    #[serde(default)]
    pub whitelist: Vec<String>,
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    #[serde(default)]
    pub http_proxy: Option<String>,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
    #[serde(default = "default_true")]
    pub network_guard: bool,
    /// Always block non-loopback traffic for the configured executables, even
    /// when the exit IP is allowed. The apps can only use the local relay.
    #[serde(default)]
    pub proxy_only: bool,
    #[serde(default = "default_true")]
    pub kill_on_block: bool,
    #[serde(default = "default_blocked_domains")]
    pub domains: Vec<String>,
}

fn default_interval() -> u64 {
    3
}
fn default_true() -> bool {
    true
}
fn default_blocked_domains() -> Vec<String> {
    vec![
        "chatgpt.com".into(),
        "chat.openai.com".into(),
        "api.openai.com".into(),
        "auth.openai.com".into(),
        "auth0.openai.com".into(),
        "platform.openai.com".into(),
        "codex.openai.com".into(),
    ]
}

impl Default for ProtectionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ip_check_mode: IpCheckMode::Both,
            whitelist: Vec::new(),
            interval_seconds: 3,
            http_proxy: None,
            targets: detect_targets(),
            network_guard: true,
            proxy_only: false,
            kill_on_block: true,
            domains: default_blocked_domains(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountMode {
    Auto,
    Personal,
    Team,
}

impl Default for AccountMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateFallback {
    Strict,
    Passthrough,
}

impl Default for StateFallback {
    fn default() -> Self {
        Self::Passthrough
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub url: String,
    /// A single outbound node, never an entire imported Mihomo configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mihomo: Option<serde_json::Value>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub fixed_model: bool,
    /// Advisory model list shown in the panel and CLI. Unknown models are still
    /// forwarded; this is not an access-control allowlist.
    #[serde(default = "default_models")]
    pub models: Vec<String>,
    #[serde(default = "default_auxiliary_models")]
    pub auxiliary_models: Vec<String>,
    #[serde(default = "default_auxiliary_prefixes")]
    pub auxiliary_prefixes: Vec<String>,
    #[serde(default = "default_calibration_prefixes")]
    pub calibration_prefixes: Vec<String>,
    /// Extra block counts accepted for future state shapes. Account defaults
    /// remain 10/33 for personal and 12/33 for Team.
    #[serde(default)]
    pub extra_state_blocks: Vec<usize>,
    #[serde(default)]
    pub account_mode: AccountMode,
    #[serde(default = "default_true")]
    pub injection: bool,
    #[serde(default)]
    pub state_fallback: StateFallback,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
    #[serde(default = "default_refresh")]
    pub refresh_seconds: u64,
    #[serde(default = "default_probe_timeout")]
    pub probe_timeout_seconds: u64,
    #[serde(default = "default_max_probes")]
    pub max_probes: usize,
    #[serde(default = "default_cooldown")]
    pub cooldown_seconds: u64,
    #[serde(default = "default_true")]
    pub direct: bool,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub subscriptions: Vec<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
    /// Optional manual upstream. When empty the takeover transaction decides.
    #[serde(default)]
    pub upstream: Option<String>,
    /// "official", "relay" or "auto". Auto follows the takeover selection.
    #[serde(default = "default_upstream_kind")]
    pub upstream_kind: String,
}

fn default_listen() -> String {
    format!("127.0.0.1:{DEFAULT_PORT}")
}
fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}
fn default_models() -> Vec<String> {
    DEFAULT_MODELS
        .iter()
        .map(|model| (*model).to_string())
        .collect()
}
fn default_auxiliary_models() -> Vec<String> {
    vec![
        "codex-auto-review".into(),
        "codex-mini-latest".into(),
        "gpt-5-codex-mini".into(),
    ]
}
fn default_auxiliary_prefixes() -> Vec<String> {
    vec!["codex-".into()]
}
fn default_calibration_prefixes() -> Vec<String> {
    vec!["gpt-".into(), "o".into()]
}
fn default_ttl() -> u64 {
    3600
}
fn default_refresh() -> u64 {
    1200
}
fn default_probe_timeout() -> u64 {
    20
}
fn default_max_probes() -> usize {
    6
}
fn default_cooldown() -> u64 {
    180
}
fn default_upstream_kind() -> String {
    "auto".into()
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: default_listen(),
            model: default_model(),
            fixed_model: false,
            models: default_models(),
            auxiliary_models: default_auxiliary_models(),
            auxiliary_prefixes: default_auxiliary_prefixes(),
            calibration_prefixes: default_calibration_prefixes(),
            extra_state_blocks: Vec::new(),
            account_mode: AccountMode::Auto,
            injection: true,
            state_fallback: StateFallback::Passthrough,
            ttl_seconds: default_ttl(),
            refresh_seconds: default_refresh(),
            probe_timeout_seconds: default_probe_timeout(),
            max_probes: default_max_probes(),
            cooldown_seconds: default_cooldown(),
            direct: true,
            routes: Vec::new(),
            subscriptions: Vec::new(),
            codex_home: None,
            upstream: None,
            upstream_kind: default_upstream_kind(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateLockConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_update_domains")]
    pub domains: Vec<String>,
    #[serde(default = "default_update_paths")]
    pub paths: Vec<String>,
}

fn default_update_domains() -> Vec<String> {
    vec![
        "persistent.oaistatic.com".into(),
        "cdn.oaistatic.com".into(),
        "updates.chatgpt.com".into(),
        "update.chatgpt.com".into(),
    ]
}

fn default_update_paths() -> Vec<String> {
    #[cfg(windows)]
    {
        let local = dirs::data_local_dir().unwrap_or_default();
        vec![
            local.join("Programs").join("ChatGPT").display().to_string(),
            local.join("ChatGPT").display().to_string(),
            local.join("Programs").join("codex").display().to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

impl Default for UpdateLockConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            domains: default_update_domains(),
            paths: default_update_paths(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub open_panel_on_start: bool,
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    #[serde(default)]
    pub admin_token: String,
}

fn default_web_port() -> u16 {
    17842
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            autostart: false,
            open_panel_on_start: false,
            web_port: default_web_port(),
            admin_token: random_token(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub protection: ProtectionConfig,
    #[serde(default)]
    pub codex: CodexConfig,
    #[serde(default)]
    pub mihomo: crate::mihomo::MihomoConfig,
    #[serde(default)]
    pub update_lock: UpdateLockConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            protection: ProtectionConfig::default(),
            codex: CodexConfig::default(),
            mihomo: crate::mihomo::MihomoConfig::default(),
            update_lock: UpdateLockConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub log_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub lock_file: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Self {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME);
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME);
        Self {
            config_file: config_dir.join("config.json"),
            state_file: data_dir.join("state.json"),
            log_dir: data_dir.join("logs"),
            backups_dir: data_dir.join("backups"),
            lock_file: data_dir.join("gptguard.lock"),
            config_dir,
            data_dir,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.log_dir,
            &self.backups_dir,
        ] {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }
}

impl Config {
    pub fn load_or_create(paths: &AppPaths) -> Result<Self> {
        paths.ensure()?;
        if paths.config_file.exists() {
            let raw = fs::read_to_string(&paths.config_file)
                .with_context(|| format!("read {}", paths.config_file.display()))?;
            let mut cfg: Config = serde_json::from_str(&raw)
                .with_context(|| format!("parse {}", paths.config_file.display()))?;
            if cfg.version == 1 {
                // v0.3/v0.4 stored the same runtime model but exposed the
                // browser panel and a user-supplied Mihomo executable.  Keep
                // the document readable for rollback, but stop using the
                // external executable when the native v2 runtime takes over.
                cfg.version = CONFIG_VERSION;
                cfg.ui.open_panel_on_start = false;
                cfg.mihomo.executable = None;
                cfg.save(paths)?;
            }
            if cfg.ui.admin_token.is_empty() {
                cfg.ui.admin_token = random_token();
                cfg.save(paths)?;
            }
            cfg.validate()?;
            Ok(cfg)
        } else {
            let mut cfg = Config::default();
            cfg.ui.admin_token = random_token();
            cfg.validate()?;
            cfg.save(paths)?;
            Ok(cfg)
        }
    }

    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        self.validate()?;
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&paths.config_file, text.as_bytes())
            .with_context(|| format!("write {}", paths.config_file.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION && self.version != 1 {
            bail!("unsupported config version {}", self.version);
        }
        if self.protection.interval_seconds < 1 || self.protection.interval_seconds > 300 {
            bail!("protection.interval_seconds must be 1-300");
        }
        for entry in &self.protection.whitelist {
            parse_net(entry).with_context(|| format!("invalid whitelist entry {entry:?}"))?;
        }
        if let Some(proxy) = &self.protection.http_proxy {
            if !proxy.trim().is_empty() {
                validate_proxy_url(proxy)?;
            }
        }
        for target in &self.protection.targets {
            if target.path.trim().is_empty() {
                continue;
            }
            if target.path.contains(['\r', '\n']) {
                bail!("target path contains a newline");
            }
        }

        let addr: SocketAddr = self
            .codex
            .listen
            .parse()
            .context("codex.listen must be a loopback IP:port")?;
        if !addr.ip().is_loopback() || addr.port() == 0 {
            bail!("codex.listen must be a loopback IP:port");
        }
        if self.ui.web_port < 1024 {
            bail!("ui.web_port must be 1024-65535");
        }
        if self.ui.web_port == addr.port() {
            bail!("ui.web_port must differ from codex.listen");
        }
        if !valid_model_id(&self.codex.model) {
            bail!("codex.model contains unsupported characters");
        }
        if self.codex.models.len() > 256 {
            bail!("codex.models has too many entries");
        }
        for model in &self.codex.models {
            if !valid_model_id(model) {
                bail!("codex.models contains invalid model id {model:?}");
            }
        }
        if self.codex.auxiliary_models.len() > 128
            || self.codex.auxiliary_prefixes.len() > 64
            || self.codex.calibration_prefixes.len() > 64
        {
            bail!("too many model classification entries");
        }
        if self.codex.extra_state_blocks.len() > 16 {
            bail!("codex.extra_state_blocks has too many entries");
        }
        for blocks in &self.codex.extra_state_blocks {
            if !(1..=128).contains(blocks) {
                bail!("codex.extra_state_blocks entries must be 1-128");
            }
        }
        if self.codex.ttl_seconds < 120 || self.codex.ttl_seconds > 3600 {
            bail!("codex.ttl_seconds must be 120-3600");
        }
        if self.codex.refresh_seconds < 30 || self.codex.refresh_seconds >= self.codex.ttl_seconds {
            bail!("codex.refresh_seconds must be 30 and shorter than ttl_seconds");
        }
        if self.codex.probe_timeout_seconds < 1 || self.codex.probe_timeout_seconds > 60 {
            bail!("codex.probe_timeout_seconds must be 1-60");
        }
        if self.codex.max_probes < 1 || self.codex.max_probes > 20 {
            bail!("codex.max_probes must be 1-20");
        }
        if self.codex.cooldown_seconds < 30 || self.codex.cooldown_seconds > 3600 {
            bail!("codex.cooldown_seconds must be 30-3600");
        }
        if !matches!(
            self.codex.upstream_kind.as_str(),
            "auto" | "official" | "relay"
        ) {
            bail!("codex.upstream_kind must be auto, official or relay");
        }
        if let Some(upstream) = &self.codex.upstream {
            validate_upstream(upstream)?;
        }
        for route in &self.codex.routes {
            if !route.enabled {
                continue;
            }
            if let Some(node) = &route.mihomo {
                if !route.url.is_empty() {
                    bail!("route must contain either url or mihomo, not both");
                }
                crate::mihomo::validate_node(node)?;
            } else {
                validate_proxy_url(&route.url).context("invalid route URL")?;
            }
        }
        crate::mihomo::validate_config(self)?;
        if self.codex.direct == false && self.codex.routes.iter().all(|route| !route.enabled) {
            // Not fatal: the proxy will report no_routes, which is clearer than
            // silently falling back to direct.
        }
        if self.update_lock.enabled {
            if self.update_lock.domains.len() > 64 || self.update_lock.paths.len() > 32 {
                bail!("too many update-lock domains or paths");
            }
            for domain in &self.update_lock.domains {
                validate_domain(domain)?;
            }
        }
        Ok(())
    }

    pub fn codex_home(&self) -> PathBuf {
        if let Some(home) = self
            .codex
            .codex_home
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            return PathBuf::from(home);
        }
        if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home);
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex")
    }

    pub fn proxy_enabled(&self) -> bool {
        self.codex.enabled
    }
}

pub fn parse_net(value: &str) -> Result<IpNet> {
    let trimmed = value.trim();
    if let Ok(net) = trimmed.parse::<IpNet>() {
        return Ok(net);
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(ipnet::IpNet::from(ip));
    }
    bail!("not an IP or CIDR")
}

pub fn ip_allowed(ip: IpAddr, whitelist: &[String]) -> bool {
    whitelist.iter().any(|entry| {
        parse_net(entry)
            .map(|net| net.contains(&ip))
            .unwrap_or(false)
    })
}

pub fn validate_proxy_url(value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("proxy URL is empty");
    }
    let parsed = url::Url::parse(trimmed).context("proxy URL is not a URL")?;
    match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" => {}
        other => bail!("unsupported proxy scheme {other}"),
    }
    if parsed.host_str().is_none() {
        bail!("proxy URL has no host");
    }
    if parsed.port().is_none() {
        bail!("proxy URL has no port");
    }
    Ok(())
}

/// Detect a proxy that is already running on this machine.  The native UI
/// uses this as a zero-configuration hint; it never changes the Windows
/// system proxy and never exposes the port to the user.
pub fn detect_local_proxy() -> Option<String> {
    for key in ["GPTGUARD_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() && validate_proxy_url(value).is_ok() {
                return Some(value.to_string());
            }
        }
    }
    #[cfg(windows)]
    if let Some(proxy) = detect_windows_proxy() {
        return Some(proxy);
    }
    for port in [7890u16, 7891, 1080, 10808, 20170] {
        if TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], port)),
            std::time::Duration::from_millis(120),
        )
        .is_ok()
        {
            return Some(format!("http://127.0.0.1:{port}"));
        }
    }
    None
}

#[cfg(windows)]
fn detect_windows_proxy() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").ok()?;
    if enabled == 0 {
        return None;
    }
    let raw: String = key.get_value("ProxyServer").ok()?;
    for entry in raw
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let value = entry
            .split_once('=')
            .map(|(_, value)| value)
            .unwrap_or(entry)
            .trim();
        if value.eq_ignore_ascii_case("<local>") {
            continue;
        }
        let candidate = if value.contains("://") {
            value.to_string()
        } else {
            format!("http://{value}")
        };
        if validate_proxy_url(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
}

pub fn validate_upstream(value: &str) -> Result<()> {
    let parsed = url::Url::parse(value).context("upstream is not a URL")?;
    if parsed.host_str().is_none() || !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("upstream must have a host and no credentials");
    }
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "::1" | "localhost"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        bail!("upstream must use HTTPS (loopback HTTP is allowed for tests)");
    }
    Ok(())
}

/// Model ids are opaque to this project. Only syntax is checked so a value
/// cannot become a header/URL injection. New upstream models do not need a
/// code change here.
pub fn valid_model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':'))
}

pub fn validate_domain(value: &str) -> Result<()> {
    let domain = value.trim();
    if domain.is_empty() || domain.len() > 253 {
        bail!("invalid domain {value:?}");
    }
    if !domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        bail!("invalid domain {value:?}");
    }
    Ok(())
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    // Windows rename does not replace an existing file.
    if path.exists() {
        let backup = path.with_extension("bak-tmp");
        let _ = fs::remove_file(&backup);
        fs::rename(path, &backup)?;
        match fs::rename(&tmp, path) {
            Ok(()) => {
                let _ = fs::remove_file(&backup);
                Ok(())
            }
            Err(err) => {
                let _ = fs::rename(&backup, path);
                Err(err.into())
            }
        }
    } else {
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

pub fn detect_targets() -> Vec<TargetConfig> {
    let mut targets = Vec::new();
    if let Some(codex) = which("codex.exe").or_else(|| which("codex")) {
        targets.push(TargetConfig {
            name: "Codex CLI".into(),
            path: codex.display().to_string(),
            kind: TargetKind::Cli,
        });
    }
    #[cfg(windows)]
    {
        let local = dirs::data_local_dir().unwrap_or_default();
        for candidate in [
            local.join("Programs").join("ChatGPT").join("ChatGPT.exe"),
            local.join("ChatGPT").join("ChatGPT.exe"),
            local.join("Programs").join("ChatGPT").join("chatgpt.exe"),
        ] {
            if candidate.is_file() {
                targets.push(TargetConfig {
                    name: "ChatGPT".into(),
                    path: candidate.display().to_string(),
                    kind: TargetKind::Desktop,
                });
                break;
            }
        }
    }
    targets
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ip_and_cidr() {
        assert!(parse_net("203.0.113.10").is_ok());
        assert!(parse_net("203.0.113.0/24").is_ok());
        assert!(parse_net("::1").is_ok());
        assert!(parse_net("2001:db8::/32").is_ok());
        assert!(parse_net("not-an-ip").is_err());
    }

    #[test]
    fn whitelist_is_fail_closed() {
        let allow = vec!["203.0.113.0/24".to_string()];
        assert!(ip_allowed("203.0.113.7".parse().unwrap(), &allow));
        assert!(!ip_allowed("198.51.100.7".parse().unwrap(), &allow));
        assert!(!ip_allowed("203.0.113.7".parse().unwrap(), &[]));
    }

    #[test]
    fn default_config_is_valid() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.codex.listen, "127.0.0.1:17841");
    }

    #[test]
    fn accepts_models_outside_the_builtin_catalog() {
        let mut cfg = Config::default();
        cfg.codex.model = "gpt-6.2-astra".into();
        cfg.codex.models.push("gpt-7-future".into());
        assert!(cfg.validate().is_ok());
        assert!(cfg.codex.models.iter().any(|model| model == "gpt-6.1-sol"));
        assert!(cfg.codex.models.iter().any(|model| model == "gpt-6.2-sol"));
    }

    #[test]
    fn validates_extra_state_blocks() {
        let mut cfg = Config::default();
        cfg.codex.extra_state_blocks = vec![41, 57];
        assert!(cfg.validate().is_ok());
        cfg.codex.extra_state_blocks = vec![0];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_non_loopback_listen() {
        let mut cfg = Config::default();
        cfg.codex.listen = "0.0.0.0:17841".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_plain_http_upstream() {
        assert!(validate_upstream("http://example.com/v1").is_err());
        assert!(validate_upstream("http://127.0.0.1:8080/v1").is_ok());
        assert!(validate_upstream("https://chatgpt.com/backend-api/codex").is_ok());
    }

    #[test]
    fn migrates_v1_config_to_v2_on_load() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_dir: root.path().join("config"),
            data_dir: root.path().join("data"),
            config_file: root.path().join("config/config.json"),
            state_file: root.path().join("data/state.json"),
            log_dir: root.path().join("data/logs"),
            backups_dir: root.path().join("data/backups"),
            lock_file: root.path().join("data/gptguard.lock"),
        };
        paths.ensure().unwrap();
        fs::write(
            &paths.config_file,
            r#"{"version":1,"mihomo":{"enabled":false,"executable":"C:\\old\\mihomo.exe"}}"#,
        )
        .unwrap();
        let cfg = Config::load_or_create(&paths).unwrap();
        assert_eq!(cfg.version, 2);
        assert!(cfg.mihomo.executable.is_none());
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&paths.config_file).unwrap()).unwrap();
        assert_eq!(saved["version"], 2);
    }
}
