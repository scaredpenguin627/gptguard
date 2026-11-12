//! Transactional Codex `config.toml` takeover.
//!
//! The original file is backed up before any edit. Restore is refused when the
//! file changed after installation unless the caller explicitly forces it and
//! accepts a second backup. Comments and unrelated tables are preserved by
//! `toml_edit`. The installed endpoint keeps the selected provider id stable so
//! Codex can continue matching chats created under that provider.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::codex::provider::{self, Selection};
use crate::config::{atomic_write, AppPaths, Config, DEFAULT_PORT};
use crate::util;

const TRANSACTION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub version: u32,
    pub config_path: String,
    pub backup_path: String,
    pub original_sha256: String,
    pub installed_sha256: String,
    pub provider_id: String,
    pub upstream: String,
    pub auth_kind: String,
    pub official: bool,
    pub profile: Option<String>,
    pub installed_at: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeoverInfo {
    pub transaction: Transaction,
    pub selection: Selection,
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub active: bool,
    pub config_path: String,
    pub provider_id: Option<String>,
    pub upstream: Option<String>,
    pub auth_kind: Option<String>,
    pub official: Option<bool>,
    pub installed_at: Option<String>,
    pub config_matches: bool,
    pub backup_path: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyRepairPreview {
    pub needed: bool,
    pub provider_before: Option<String>,
    pub provider_after: Option<String>,
    pub changes: Vec<String>,
    pub backup_available: bool,
    pub safe_to_apply: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeoverPreview {
    pub needed: bool,
    pub provider_id: Option<String>,
    pub current_endpoint: Option<String>,
    pub desired_endpoint: String,
    pub changes: Vec<String>,
    pub safe_to_apply: bool,
    pub detail: String,
}

pub fn transaction_path(paths: &AppPaths) -> PathBuf {
    paths.data_dir.join("codex-transaction.json")
}

pub(crate) fn read_transaction(paths: &AppPaths) -> Result<Option<Transaction>> {
    let path = transaction_path(paths);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let transaction: Transaction = serde_json::from_str(&raw).context("parse Codex transaction")?;
    if transaction.version != TRANSACTION_VERSION {
        bail!("unsupported Codex transaction version");
    }
    Ok(Some(transaction))
}

/// Return the migration that the native UI should show before touching a
/// legacy configuration.  Old releases selected a new `gptguard` provider;
/// that changes the identity stored in existing chats and causes Codex's
/// "model provider mismatch" error.
pub fn legacy_repair_preview(config: &Config, paths: &AppPaths) -> Result<LegacyRepairPreview> {
    let config_path = config.codex_home().join("config.toml");
    if !config_path.is_file() {
        return Ok(LegacyRepairPreview {
            needed: false,
            provider_before: None,
            provider_after: None,
            changes: Vec::new(),
            backup_available: false,
            safe_to_apply: false,
            detail: "没有找到 Codex 配置".into(),
        });
    }
    let original = fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let doc: toml::Value = toml::from_str(&original).context("Codex config is not valid TOML")?;
    let provider_before = doc
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    if provider_before.as_deref() != Some("gptguard") {
        return Ok(LegacyRepairPreview {
            needed: false,
            provider_before: provider_before.clone(),
            provider_after: provider_before,
            changes: Vec::new(),
            backup_available: false,
            safe_to_apply: false,
            detail: "没有发现旧版 GPTGuard provider".into(),
        });
    }
    let transaction = read_transaction(paths)?;
    let backup_available = transaction
        .as_ref()
        .map(|tx| paths.backups_dir.join(&tx.backup_path).is_file())
        .unwrap_or(false)
        || newest_codex_backup(paths).is_some();
    let safe_to_apply = transaction.is_some() && backup_available;
    let mut changes = vec![
        "恢复原来的 model_provider 身份".to_string(),
        "删除 GPTGuard 旧 provider 选择".to_string(),
        "保留原始配置备份".to_string(),
    ];
    if !safe_to_apply {
        changes.push("没有可信事务备份时，只允许用户在高级恢复中选择备份".to_string());
    }
    Ok(LegacyRepairPreview {
        needed: true,
        provider_before,
        provider_after: Some("openai（优先从备份恢复）".into()),
        changes,
        backup_available,
        safe_to_apply,
        detail: if safe_to_apply {
            "检测到旧版 GPTGuard 接管；确认后恢复原 provider 身份".into()
        } else {
            "检测到旧版 GPTGuard 接管，但没有可自动确认的备份".into()
        },
    })
}

/// Build the small diff shown by the native wizard before it writes Codex.
/// This deliberately inspects only the provider identity and endpoint; the
/// takeover transaction preserves every unrelated TOML table and comment.
pub fn takeover_preview(config: &Config, paths: &AppPaths) -> Result<TakeoverPreview> {
    let desired_endpoint = format!(
        "http://127.0.0.1:{}/backend-api/codex",
        listen_port(&config.codex.listen)
    );
    let config_path = config.codex_home().join("config.toml");
    if !config_path.is_file() {
        return Ok(TakeoverPreview {
            needed: true,
            provider_id: None,
            current_endpoint: None,
            desired_endpoint,
            changes: vec!["等待 Codex 生成 config.toml".into()],
            safe_to_apply: false,
            detail: "没有找到 Codex 配置".into(),
        });
    }
    let original = fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let doc: toml::Value = toml::from_str(&original).context("Codex config is not valid TOML")?;
    let provider_id = doc
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or("openai")
        .to_string();
    if provider_id == "gptguard" {
        let legacy = legacy_repair_preview(config, paths)?;
        return Ok(TakeoverPreview {
            needed: true,
            provider_id: Some(provider_id),
            current_endpoint: None,
            desired_endpoint,
            changes: legacy.changes,
            safe_to_apply: legacy.safe_to_apply,
            detail: legacy.detail,
        });
    }
    let current_endpoint = if provider_id == "openai" {
        doc.get("openai_base_url")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    } else {
        doc.get("model_providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get(&provider_id))
            .and_then(toml::Value::as_table)
            .and_then(|provider| provider.get("base_url"))
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    let mut changes = vec![format!("保持 model_provider = \"{provider_id}\"")];
    if current_endpoint.as_deref() != Some(desired_endpoint.as_str()) {
        let field = if provider_id == "openai" {
            "openai_base_url".to_string()
        } else {
            format!("model_providers.{provider_id}.base_url")
        };
        changes.push(format!("写入 {field} = \"{desired_endpoint}\""));
    }
    let needed = current_endpoint.as_deref() != Some(desired_endpoint.as_str());
    if !needed {
        changes.clear();
    }
    Ok(TakeoverPreview {
        needed,
        provider_id: Some(provider_id),
        current_endpoint,
        desired_endpoint,
        changes,
        safe_to_apply: true,
        detail: if needed {
            "确认后只修改本机 endpoint，provider 身份保持不变".into()
        } else {
            "Codex 已指向 GPTGuard 本机 endpoint".into()
        },
    })
}

/// Repair a legacy config before a new takeover.  The exact pre-GPTGuard file
/// is preferred; without it we refuse to guess a provider identity.
pub fn repair_legacy_provider(config: &Config, paths: &AppPaths) -> Result<bool> {
    let config_path = config.codex_home().join("config.toml");
    if !config_path.is_file() {
        return Ok(false);
    }
    let current = fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let current_doc: toml::Value =
        toml::from_str(&current).context("Codex config is not valid TOML")?;
    if current_doc
        .get("model_provider")
        .and_then(toml::Value::as_str)
        != Some("gptguard")
    {
        return Ok(false);
    }

    if let Some(tx) = read_transaction(paths)? {
        let current_hash = util::sha256_hex(current.as_bytes());
        if current_hash != tx.installed_sha256 {
            bail!("旧版 GPTGuard 接管后的 Codex 配置已被修改；请在高级恢复中确认冲突备份");
        }
        let backup = paths.backups_dir.join(&tx.backup_path);
        let original = fs::read_to_string(&backup)
            .with_context(|| format!("read legacy backup {}", backup.display()))?;
        if util::sha256_hex(original.as_bytes()) != tx.original_sha256 {
            bail!("旧版 Codex 备份校验失败；没有覆盖当前配置");
        }
        atomic_write(&config_path, original.as_bytes())?;
        // The old transaction has now been consumed.  Removing it lets the
        // current v2 takeover create a fresh transaction against the restored
        // provider identity instead of reporting a stale active takeover.
        fs::remove_file(transaction_path(paths))?;
        return Ok(true);
    }

    // A few early development builds did not persist a transaction.  Only
    // recover when the old table clearly describes our loopback ChatGPT
    // relay; otherwise a native UI must ask for manual selection.
    let old = current_doc
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get("gptguard"))
        .and_then(toml::Value::as_table)
        .context("旧版 GPTGuard provider 没有定义")?;
    let base_url = old
        .get("base_url")
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let requires_auth = old
        .get("requires_openai_auth")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let parsed = url::Url::parse(base_url).ok();
    let loopback = parsed
        .as_ref()
        .and_then(url::Url::host_str)
        .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1"));
    if !loopback || !requires_auth {
        bail!("无法确认旧版 provider 的来源；请在高级恢复中选择原始 Codex 备份");
    }

    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup_name = format!("config-legacy-conflict-{stamp}.toml");
    fs::write(paths.backups_dir.join(&backup_name), &current)?;
    let mut doc: toml_edit::DocumentMut = current.parse().context("parse Codex TOML")?;
    doc["model_provider"] = value("openai");
    doc["openai_base_url"] = value(base_url);
    if let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
        if let Some(old_provider) = providers.get("gptguard").cloned() {
            let openai = providers
                .entry("openai")
                .or_insert(Item::Table(Table::new()));
            if let (Some(old), Some(openai)) = (old_provider.as_table(), openai.as_table_mut()) {
                for key in ["wire_api", "requires_openai_auth"] {
                    if let Some(item) = old.get(key) {
                        openai[key] = item.clone();
                    }
                }
            }
        }
        providers.remove("gptguard");
    }
    atomic_write(&config_path, doc.to_string().as_bytes())?;
    Ok(true)
}

fn newest_codex_backup(paths: &AppPaths) -> Option<PathBuf> {
    let mut entries = fs::read_dir(&paths.backups_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("config-")
                && entry.path().extension().is_some_and(|ext| ext == "toml")
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok());
    entries.pop().map(|entry| entry.path())
}

pub fn current_status(config: &Config, paths: &AppPaths) -> Status {
    let config_path = config.codex_home().join("config.toml");
    let transaction = match read_transaction(paths) {
        Ok(transaction) => transaction,
        Err(err) => {
            return Status {
                active: false,
                config_path: config_path.display().to_string(),
                provider_id: None,
                upstream: None,
                auth_kind: None,
                official: None,
                installed_at: None,
                config_matches: false,
                backup_path: None,
                detail: format!("事务文件损坏: {err:#}"),
            };
        }
    };
    let Some(transaction) = transaction else {
        return Status {
            active: false,
            config_path: config_path.display().to_string(),
            provider_id: None,
            upstream: None,
            auth_kind: None,
            official: None,
            installed_at: None,
            config_matches: false,
            backup_path: None,
            detail: "未接管 Codex 配置".into(),
        };
    };
    let current = fs::read_to_string(&config_path).unwrap_or_default();
    let current_hash = util::sha256_hex(current.as_bytes());
    let config_matches = current_hash == transaction.installed_sha256;
    Status {
        active: true,
        config_path: config_path.display().to_string(),
        provider_id: Some(transaction.provider_id.clone()),
        upstream: Some(transaction.upstream.clone()),
        auth_kind: Some(transaction.auth_kind.clone()),
        official: Some(transaction.official),
        installed_at: Some(transaction.installed_at.clone()),
        config_matches,
        backup_path: Some(
            paths
                .backups_dir
                .join(&transaction.backup_path)
                .display()
                .to_string(),
        ),
        detail: if config_matches {
            "Codex 已接管；修改连接设置后先恢复，再重新接管".into()
        } else {
            "Codex 配置在接管后被修改；转发将停止，请检查后恢复（必要时加 --force）".into()
        },
    }
}

pub fn takeover(
    config: &Config,
    paths: &AppPaths,
    profile: Option<&str>,
    auth_mode: &str,
) -> Result<TakeoverInfo> {
    paths.ensure()?;
    // Migrate the old provider identity before resolving the new takeover.
    // This is deliberately backup-first and never invents a provider when the
    // old transaction cannot be verified.
    let _ = repair_legacy_provider(config, paths)?;
    if let Some(transaction) = read_transaction(paths)? {
        bail!(
            "已有 Codex 接管事务（{}）；请先运行 `gptguard codex restore`，再重新接管",
            transaction.installed_at
        );
    }
    let codex_dir = config.codex_home();
    let config_path = codex_dir.join("config.toml");
    if !config_path.is_file() {
        bail!(
            "没有找到 Codex 配置 {}；请先登录 Codex 并生成配置",
            config_path.display()
        );
    }
    let original = fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let selection = provider::resolve(&original, profile, auth_mode)?;
    let original_hash = util::sha256_hex(original.as_bytes());

    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup_name = format!("config-{stamp}.toml");
    let backup_path = paths.backups_dir.join(&backup_name);
    fs::write(&backup_path, &original)
        .with_context(|| format!("write backup {}", backup_path.display()))?;

    let installed = render_managed_config(config, &original, &selection)?;
    let installed_hash = util::sha256_hex(installed.as_bytes());
    let changed = installed_hash != original_hash;
    if changed {
        atomic_write(&config_path, installed.as_bytes())?;
    }

    let transaction = Transaction {
        version: TRANSACTION_VERSION,
        config_path: config_path.display().to_string(),
        backup_path: backup_name,
        original_sha256: original_hash,
        installed_sha256: installed_hash,
        provider_id: selection.provider_id.clone(),
        upstream: selection.upstream.clone(),
        auth_kind: selection.auth_kind.clone(),
        official: selection.official,
        profile: selection.profile.clone(),
        installed_at: util::now_iso(),
        model: if config.codex.fixed_model {
            Some(config.codex.model.clone())
        } else {
            None
        },
    };
    let raw = serde_json::to_string_pretty(&transaction)?;
    atomic_write(&transaction_path(paths), raw.as_bytes())?;

    Ok(TakeoverInfo {
        transaction,
        selection,
        changed,
    })
}

fn render_managed_config(config: &Config, original: &str, selection: &Selection) -> Result<String> {
    let mut doc: DocumentMut = original.parse().context("parse Codex TOML")?;
    if config.codex.fixed_model {
        doc["model"] = value(&config.codex.model);
    }

    let local_endpoint = format!(
        "http://127.0.0.1:{}/backend-api/codex",
        listen_port(&config.codex.listen)
    );

    // Keep the provider id stable. Codex binds a saved chat to that id, so
    // replacing it with a new `gptguard` id makes existing chats unusable.
    // The built-in `openai` provider has a dedicated top-level endpoint
    // override; user-defined providers keep their table and only change the
    // endpoint while the transaction is active.
    if selection.provider_id == "openai" {
        doc["openai_base_url"] = value(local_endpoint);
    } else {
        let providers = doc
            .entry("model_providers")
            .or_insert(Item::Table(Table::new()));
        let providers = providers
            .as_table_mut()
            .context("model_providers must be a table")?;
        let provider = providers
            .get_mut(&selection.provider_id)
            .with_context(|| {
                format!(
                    "selected provider {} has no definition",
                    selection.provider_id
                )
            })?
            .as_table_mut()
            .with_context(|| format!("provider {} must be a table", selection.provider_id))?;
        provider["base_url"] = value(local_endpoint);
    }

    if let Some(profile) = &selection.profile {
        if let Some(profile_table) = doc
            .get_mut("profiles")
            .and_then(|item| item.as_table_mut())
            .and_then(|profiles| profiles.get_mut(profile))
            .and_then(|item| item.as_table_mut())
        {
            if config.codex.fixed_model {
                profile_table["model"] = value(&config.codex.model);
            }
        }
    }

    Ok(doc.to_string())
}

fn listen_port(listen: &str) -> u16 {
    listen
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

pub fn restore(config: &Config, paths: &AppPaths, force: bool) -> Result<()> {
    let Some(transaction) = read_transaction(paths)? else {
        bail!("没有可恢复的 Codex 接管事务");
    };
    let config_path = config.codex_home().join("config.toml");
    let current = fs::read_to_string(&config_path).unwrap_or_default();
    let current_hash = util::sha256_hex(current.as_bytes());
    if current_hash != transaction.installed_sha256 {
        if !force {
            bail!(
                "Codex 配置在接管后被修改；为免覆盖你的改动，已拒绝自动恢复。确认后可加 --force，当前文件会另存备份"
            );
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let conflict = paths
            .backups_dir
            .join(format!("config-conflict-{stamp}.toml"));
        fs::write(&conflict, &current)
            .with_context(|| format!("write conflict backup {}", conflict.display()))?;
    }
    let backup = paths.backups_dir.join(&transaction.backup_path);
    let original =
        fs::read_to_string(&backup).with_context(|| format!("read backup {}", backup.display()))?;
    if util::sha256_hex(original.as_bytes()) != transaction.original_sha256 {
        bail!("备份文件校验失败；保留现场，未写入 Codex 配置");
    }
    atomic_write(&config_path, original.as_bytes())?;
    fs::remove_file(transaction_path(paths))?;
    Ok(())
}

/// Update the model in an installed managed config without re-running takeover
/// (which would back up the already-managed file as if it were the original).
pub fn set_installed_model(config: &Config, paths: &AppPaths, model: &str) -> Result<bool> {
    let Some(mut transaction) = read_transaction(paths)? else {
        return Ok(false);
    };
    let config_path = config.codex_home().join("config.toml");
    let current = fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    if util::sha256_hex(current.as_bytes()) != transaction.installed_sha256 {
        bail!("Codex 配置在接管后被修改；拒绝写入模型。请先恢复或重新接管");
    }
    let mut doc: DocumentMut = current.parse().context("parse Codex TOML")?;
    doc["model"] = value(model);
    if let Some(profile) = &transaction.profile {
        if let Some(profile_table) = doc
            .get_mut("profiles")
            .and_then(|item| item.as_table_mut())
            .and_then(|profiles| profiles.get_mut(profile))
            .and_then(|item| item.as_table_mut())
        {
            profile_table["model"] = value(model);
        }
    }
    let updated = doc.to_string();
    atomic_write(&config_path, updated.as_bytes())?;
    transaction.installed_sha256 = util::sha256_hex(updated.as_bytes());
    transaction.model = Some(model.to_string());
    let raw = serde_json::to_string_pretty(&transaction)?;
    atomic_write(&transaction_path(paths), raw.as_bytes())?;
    Ok(true)
}

pub fn check_owned_config(config: &Config, paths: &AppPaths) -> Result<Transaction> {
    let transaction = read_transaction(paths)?.context("Codex 配置尚未接管")?;
    let config_path = config.codex_home().join("config.toml");
    let current = fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let current_hash = util::sha256_hex(current.as_bytes());
    if current_hash != transaction.installed_sha256 {
        bail!("Codex 配置在接管后被修改；停止转发。请在面板检查并恢复/重新接管");
    }
    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppPaths;
    use std::path::Path;

    fn temp_paths(root: &Path) -> AppPaths {
        AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            config_file: root.join("config/config.json"),
            state_file: root.join("data/state.json"),
            log_dir: root.join("data/logs"),
            backups_dir: root.join("data/backups"),
            lock_file: root.join("data/gptguard.lock"),
        }
    }

    #[test]
    fn takeover_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        paths.ensure().unwrap();
        let codex = dir.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        let original = r#"# my config
model = "gpt-5.6-sol"
model_provider = "openai"

[model_providers.openai]
name = "OpenAI"
wire_api = "responses"
requires_openai_auth = true
"#;
        fs::write(codex.join("config.toml"), original).unwrap();
        let mut config = Config::default();
        config.codex.codex_home = Some(codex.display().to_string());
        config.codex.fixed_model = true;
        config.codex.model = "gpt-6-astra".into();

        let info = takeover(&config, &paths, None, "chatgpt").unwrap();
        assert!(info.selection.official);
        let installed = fs::read_to_string(codex.join("config.toml")).unwrap();
        assert!(installed.contains("model_provider = \"openai\""));
        assert!(installed.contains("# my config"));
        assert!(installed.contains("model = \"gpt-6-astra\""));
        assert!(installed.contains("http://127.0.0.1:17841/backend-api/codex"));
        assert!(
            installed.contains("openai_base_url = \"http://127.0.0.1:17841/backend-api/codex\"")
        );
        assert!(!installed.contains("model_providers.gptguard"));

        let status = current_status(&config, &paths);
        assert!(status.active);
        assert!(status.config_matches);

        restore(&config, &paths, false).unwrap();
        assert_eq!(
            fs::read_to_string(codex.join("config.toml")).unwrap(),
            original
        );
        assert!(!transaction_path(&paths).exists());
    }

    #[test]
    fn takeover_preserves_custom_provider_identity() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        paths.ensure().unwrap();
        let codex = dir.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        let original = r#"model_provider = "relay"

[model_providers.relay]
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
env_key = "OPENAI_API_KEY"
"#;
        fs::write(codex.join("config.toml"), original).unwrap();
        let mut config = Config::default();
        config.codex.codex_home = Some(codex.display().to_string());

        let info = takeover(&config, &paths, None, "api_key").unwrap();
        assert_eq!(info.selection.provider_id, "relay");
        let installed = fs::read_to_string(codex.join("config.toml")).unwrap();
        assert!(installed.contains("model_provider = \"relay\""));
        assert!(installed.contains("base_url = \"http://127.0.0.1:17841/backend-api/codex\""));
        assert!(installed.contains("env_key = \"OPENAI_API_KEY\""));

        restore(&config, &paths, false).unwrap();
        assert_eq!(
            fs::read_to_string(codex.join("config.toml")).unwrap(),
            original
        );
    }

    #[test]
    fn takeover_preview_shows_endpoint_without_changing_provider() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        paths.ensure().unwrap();
        let codex = dir.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("config.toml"),
            "model_provider = \"openai\"\n\n[model_providers.openai]\nrequires_openai_auth = true\n",
        )
        .unwrap();
        let mut config = Config::default();
        config.codex.codex_home = Some(codex.display().to_string());
        let preview = takeover_preview(&config, &paths).unwrap();
        assert!(preview.needed);
        assert_eq!(preview.provider_id.as_deref(), Some("openai"));
        assert!(preview
            .changes
            .iter()
            .any(|change| change.contains("openai_base_url")));
    }

    #[test]
    fn takeover_requires_restoring_an_existing_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        paths.ensure().unwrap();
        let codex = dir.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(codex.join("config.toml"), "model_provider = \"openai\"\n").unwrap();
        let mut config = Config::default();
        config.codex.codex_home = Some(codex.display().to_string());
        takeover(&config, &paths, None, "chatgpt").unwrap();
        let error = takeover(&config, &paths, None, "chatgpt").unwrap_err();
        assert!(error
            .to_string()
            .contains("先运行 `gptguard codex restore`"));
    }

    #[test]
    fn restore_refuses_external_edit_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        paths.ensure().unwrap();
        let codex = dir.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        let original = "model_provider = \"openai\"\n";
        fs::write(codex.join("config.toml"), original).unwrap();
        let mut config = Config::default();
        config.codex.codex_home = Some(codex.display().to_string());
        takeover(&config, &paths, None, "chatgpt").unwrap();
        fs::write(codex.join("config.toml"), "model_provider = \"relay\"\n").unwrap();
        assert!(restore(&config, &paths, false).is_err());
        restore(&config, &paths, true).unwrap();
        assert_eq!(
            fs::read_to_string(codex.join("config.toml")).unwrap(),
            original
        );
    }

    #[test]
    fn repairs_legacy_gptguard_provider_without_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        paths.ensure().unwrap();
        let codex = dir.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("config.toml"),
            r#"model_provider = "gptguard"

[model_providers.gptguard]
name = "GPTGuard"
base_url = "http://127.0.0.1:17841/backend-api/codex"
wire_api = "responses"
requires_openai_auth = true
"#,
        )
        .unwrap();
        let mut config = Config::default();
        config.codex.codex_home = Some(codex.display().to_string());

        assert!(repair_legacy_provider(&config, &paths).unwrap());
        let repaired = fs::read_to_string(codex.join("config.toml")).unwrap();
        assert!(repaired.contains("model_provider = \"openai\""));
        assert!(repaired.contains("openai_base_url = \"http://127.0.0.1:17841/backend-api/codex\""));
        assert!(!repaired.contains("model_provider = \"gptguard\""));
        assert!(!repaired.contains("[model_providers.gptguard]"));
    }

    #[test]
    fn repairs_legacy_transaction_before_new_takeover() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        paths.ensure().unwrap();
        let codex = dir.path().join("codex");
        fs::create_dir_all(&codex).unwrap();
        let original = r#"model_provider = "openai"

[model_providers.openai]
name = "OpenAI"
wire_api = "responses"
requires_openai_auth = true
"#;
        let legacy = r#"model_provider = "gptguard"

[model_providers.gptguard]
name = "GPTGuard"
base_url = "http://127.0.0.1:17841/backend-api/codex"
wire_api = "responses"
requires_openai_auth = true
"#;
        let config_path = codex.join("config.toml");
        fs::write(&config_path, legacy).unwrap();
        let backup_name = "config-legacy-original.toml";
        fs::write(paths.backups_dir.join(backup_name), original).unwrap();
        let transaction = Transaction {
            version: TRANSACTION_VERSION,
            config_path: config_path.display().to_string(),
            backup_path: backup_name.into(),
            original_sha256: util::sha256_hex(original.as_bytes()),
            installed_sha256: util::sha256_hex(legacy.as_bytes()),
            provider_id: "gptguard".into(),
            upstream: "http://127.0.0.1:17841/backend-api/codex".into(),
            auth_kind: "chatgpt".into(),
            official: true,
            profile: None,
            installed_at: "2026-01-01T00:00:00Z".into(),
            model: None,
        };
        fs::write(
            transaction_path(&paths),
            serde_json::to_vec_pretty(&transaction).unwrap(),
        )
        .unwrap();

        let mut config = Config::default();
        config.codex.codex_home = Some(codex.display().to_string());
        let info = takeover(&config, &paths, None, "chatgpt").unwrap();
        assert_eq!(info.selection.provider_id, "openai");
        let installed = fs::read_to_string(config_path).unwrap();
        assert!(installed.contains("model_provider = \"openai\""));
        assert!(!installed.contains("gptguard"));
        assert!(transaction_path(&paths).is_file());
    }
}
