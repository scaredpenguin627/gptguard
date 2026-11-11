//! Transactional Codex `config.toml` takeover.
//!
//! The original file is backed up before any edit. Restore is refused when the
//! file changed after installation unless the caller explicitly forces it and
//! accepts a second backup. Comments and unrelated tables are preserved by
//! `toml_edit`.

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
            "Codex 已接管；修改连接设置后请重新接管".into()
        } else {
            "Codex 配置在接管后被修改；转发将停止，请检查后恢复或重新接管".into()
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
    doc["model_provider"] = value("aiguard");
    if config.codex.fixed_model {
        doc["model"] = value(&config.codex.model);
    }

    let providers = doc
        .entry("model_providers")
        .or_insert(Item::Table(Table::new()));
    let providers = providers
        .as_table_mut()
        .context("model_providers must be a table")?;
    providers.remove("aiguard");
    let mut managed = Table::new();
    managed["name"] = value("ChatGPTGuard");
    managed["base_url"] = value(format!(
        "http://127.0.0.1:{}/backend-api/codex",
        listen_port(&config.codex.listen)
    ));
    managed["wire_api"] = value("responses");
    managed["requires_openai_auth"] = value(selection.requires_openai_auth);
    if !selection.official {
        managed["env_key"] = value(
            selection
                .env_key
                .clone()
                .unwrap_or_else(|| "OPENAI_API_KEY".into()),
        );
    }
    providers["aiguard"] = Item::Table(managed);

    if let Some(profile) = &selection.profile {
        if let Some(profile_table) = doc
            .get_mut("profiles")
            .and_then(|item| item.as_table_mut())
            .and_then(|profiles| profiles.get_mut(profile))
            .and_then(|item| item.as_table_mut())
        {
            profile_table["model_provider"] = value("aiguard");
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
            lock_file: root.join("data/aiguard.lock"),
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
        assert!(installed.contains("model_provider = \"aiguard\""));
        assert!(installed.contains("# my config"));
        assert!(installed.contains("model = \"gpt-6-astra\""));
        assert!(installed.contains("http://127.0.0.1:17841/backend-api/codex"));
        assert!(installed.contains("requires_openai_auth = true"));

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
}
