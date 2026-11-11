//! Windows-first egress enforcement: hosts entries, per-program firewall
//! rules, process termination, update locking and login autostart.
//!
//! Every function reports what actually happened. A missing privilege never
//! turns into a silent "protected" state.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, TargetConfig};
use crate::util;

pub const GUARD_BEGIN: &str = "# BEGIN AIGuard egress block";
pub const GUARD_END: &str = "# END AIGuard egress block";
pub const UPDATE_BEGIN: &str = "# BEGIN AIGuard update lock";
pub const UPDATE_END: &str = "# END AIGuard update lock";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allowed,
    Blocked,
    Offline,
    Disabled,
    Error,
    Stopped,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Blocked => "blocked",
            Self::Offline => "offline",
            Self::Disabled => "disabled",
            Self::Error => "error",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnforcementReport {
    pub elevated: bool,
    pub hosts_blocked: bool,
    pub firewall_blocked: bool,
    pub update_locked: bool,
    pub killed: Vec<String>,
    pub errors: Vec<String>,
}

impl EnforcementReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

pub fn hosts_path() -> PathBuf {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
        root.join("System32")
            .join("drivers")
            .join("etc")
            .join("hosts")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/etc/hosts")
    }
}

/// Replace (or remove) a marked block in a hosts file body.
pub fn rewrite_hosts(original: &str, domains: &[String], begin: &str, end: &str) -> String {
    let mut kept = Vec::new();
    let mut skipping = false;
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed == begin {
            skipping = true;
            continue;
        }
        if trimmed == end {
            skipping = false;
            continue;
        }
        if !skipping {
            kept.push(line);
        }
    }
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    let mut output = kept.join("\n");
    if !domains.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(begin);
        output.push('\n');
        for domain in domains {
            output.push_str(&format!("0.0.0.0 {domain}\n"));
            output.push_str(&format!("::1 {domain}\n"));
        }
        output.push_str(end);
        output.push('\n');
    } else if !output.is_empty() {
        output.push('\n');
    }
    output
}

pub fn apply_hosts_block(domains: &[String], begin: &str, end: &str) -> Result<()> {
    if !util::is_elevated() {
        bail!("需要管理员权限才能修改 hosts");
    }
    let path = hosts_path();
    let original = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let updated = rewrite_hosts(&original, domains, begin, end);
    if updated != original {
        fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

pub fn remove_hosts_block(begin: &str, end: &str) -> Result<()> {
    apply_hosts_block(&[], begin, end)
}

fn firewall_rule_name(target: &TargetConfig) -> String {
    let label: String = target
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("AIGuard Block {label}")
}

pub fn apply_firewall(targets: &[TargetConfig]) -> Result<()> {
    if !util::is_elevated() {
        bail!("需要管理员权限才能修改 Windows 防火墙");
    }
    for target in targets {
        if target.path.trim().is_empty() {
            continue;
        }
        let path = Path::new(&target.path);
        if !path.is_file() {
            bail!("目标程序不存在: {}", target.path);
        }
        let rule = firewall_rule_name(target);
        let _ = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={rule}"),
            ])
            .output();
        let output = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={rule}"),
                "dir=out",
                "action=block",
                &format!("program={}", target.path),
                "enable=yes",
                "profile=any",
            ])
            .output()
            .context("run netsh advfirewall")?;
        if !output.status.success() {
            bail!(
                "netsh 添加规则失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

pub fn remove_firewall(targets: &[TargetConfig]) -> Result<()> {
    if !util::is_elevated() {
        bail!("需要管理员权限才能修改 Windows 防火墙");
    }
    for target in targets {
        if target.path.trim().is_empty() {
            continue;
        }
        let rule = firewall_rule_name(target);
        let _ = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={rule}"),
            ])
            .output();
    }
    Ok(())
}

pub fn target_paths(config: &Config) -> Vec<String> {
    config
        .protection
        .targets
        .iter()
        .map(|target| target.path.clone())
        .filter(|path| !path.trim().is_empty())
        .collect()
}

/// Apply the block/allow decision. Update locking is deliberately separate.
pub fn enforce(config: &Config, block: bool) -> EnforcementReport {
    let mut report = EnforcementReport {
        elevated: util::is_elevated(),
        ..Default::default()
    };
    if config.protection.network_guard {
        if block {
            match apply_hosts_block(&config.protection.domains, GUARD_BEGIN, GUARD_END) {
                Ok(()) => report.hosts_blocked = true,
                Err(err) => report.errors.push(format!("hosts: {err:#}")),
            }
            match apply_firewall(&config.protection.targets) {
                Ok(()) => report.firewall_blocked = true,
                Err(err) => report.errors.push(format!("firewall: {err:#}")),
            }
        } else {
            match remove_hosts_block(GUARD_BEGIN, GUARD_END) {
                Ok(()) => {}
                Err(err) => report.errors.push(format!("hosts: {err:#}")),
            }
            match remove_firewall(&config.protection.targets) {
                Ok(()) => {}
                Err(err) => report.errors.push(format!("firewall: {err:#}")),
            }
        }
    }
    if block && config.protection.kill_on_block {
        report.killed = util::kill_processes(&target_paths(config));
    }
    report
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateLockRecord {
    domains: Vec<String>,
    paths: Vec<String>,
    user: String,
}

fn update_record_path(config_dir: &Path) -> PathBuf {
    config_dir.join("update-lock.json")
}

pub fn update_lock(config: &Config, record_dir: &Path) -> EnforcementReport {
    let mut report = EnforcementReport {
        elevated: util::is_elevated(),
        ..Default::default()
    };
    if !config.update_lock.enabled {
        report.errors.push("update_lock is disabled".into());
        return report;
    }
    if !util::is_elevated() {
        report.errors.push("需要管理员权限才能锁定自动更新".into());
        return report;
    }
    if let Err(err) = apply_hosts_block(&config.update_lock.domains, UPDATE_BEGIN, UPDATE_END) {
        report.errors.push(format!("update hosts: {err:#}"));
    }
    let user = util::run_capture("whoami", &[])
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut locked = Vec::new();
    for path in &config.update_lock.paths {
        let candidate = Path::new(path);
        if !candidate.exists() {
            continue;
        }
        let acl = format!("{user}:(OI)(CI)(W,D)");
        let output = Command::new("icacls")
            .arg(candidate)
            .arg("/deny")
            .arg(&acl)
            .output();
        match output {
            Ok(output) if output.status.success() => locked.push(path.clone()),
            Ok(output) => report.errors.push(format!(
                "icacls {}: {}",
                candidate.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(err) => report
                .errors
                .push(format!("icacls {}: {err}", candidate.display())),
        }
    }
    let record = UpdateLockRecord {
        domains: config.update_lock.domains.clone(),
        paths: locked,
        user,
    };
    if let Ok(text) = serde_json::to_string_pretty(&record) {
        let _ = fs::create_dir_all(record_dir);
        let _ = fs::write(update_record_path(record_dir), text);
    }
    report.update_locked = true;
    report
}

pub fn remove_update_lock(record_dir: &Path) -> EnforcementReport {
    let mut report = EnforcementReport {
        elevated: util::is_elevated(),
        ..Default::default()
    };
    if !util::is_elevated() {
        report
            .errors
            .push("需要管理员权限才能解除自动更新锁定".into());
        return report;
    }
    if let Err(err) = remove_hosts_block(UPDATE_BEGIN, UPDATE_END) {
        report.errors.push(format!("update hosts: {err:#}"));
    }
    let record_path = update_record_path(record_dir);
    if let Ok(raw) = fs::read_to_string(&record_path) {
        if let Ok(record) = serde_json::from_str::<UpdateLockRecord>(&raw) {
            for path in record.paths {
                let acl = format!("{}:(OI)(CI)(W,D)", record.user);
                let _ = Command::new("icacls")
                    .arg(&path)
                    .arg("/remove:d")
                    .arg(&acl)
                    .output();
            }
        }
        let _ = fs::remove_file(record_path);
    }
    report.update_locked = false;
    report
}

pub fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
            .ok()
            .and_then(|key| key.get_value::<String, _>("AIGuard").ok())
            .is_some()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")?;
        if enabled {
            let exe = util::exe_path();
            let command = format!("\"{}\" tray --start-guard", exe.display());
            key.set_value("AIGuard", &command)?;
        } else {
            let _ = key.delete_value("AIGuard");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        bail!("当前平台暂不支持写入登录自启动")
    }
}

pub fn doctor(config: &Config) -> Vec<(String, bool, String)> {
    let mut rows = Vec::new();
    rows.push((
        "管理员权限".into(),
        util::is_elevated(),
        "hosts、防火墙和更新锁需要管理员权限；普通权限只能做进程终止与启动前检查".into(),
    ));
    let hosts = hosts_path();
    rows.push((
        "hosts 文件".into(),
        hosts.is_file(),
        hosts.display().to_string(),
    ));
    for target in &config.protection.targets {
        if target.path.trim().is_empty() {
            continue;
        }
        rows.push((
            format!("目标程序 {}", target.name),
            Path::new(&target.path).is_file(),
            target.path.clone(),
        ));
    }
    let codex_home = config.codex_home();
    rows.push((
        "Codex 配置目录".into(),
        codex_home.is_dir(),
        codex_home.display().to_string(),
    ));
    rows.push((
        "出口白名单".into(),
        !config.protection.whitelist.is_empty(),
        format!("{} 条规则", config.protection.whitelist.len()),
    ));
    rows.push((
        "登录自启动".into(),
        autostart_enabled(),
        "HKCU Run: AIGuard".into(),
    ));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_marked_block_without_touching_other_lines() {
        let original = "127.0.0.1 localhost\n# comment\n";
        let text = rewrite_hosts(original, &["chatgpt.com".into()], GUARD_BEGIN, GUARD_END);
        assert!(text.contains("0.0.0.0 chatgpt.com"));
        assert!(text.contains("127.0.0.1 localhost"));
        assert!(text.contains("# comment"));
        let cleaned = rewrite_hosts(&text, &[], GUARD_BEGIN, GUARD_END);
        assert!(!cleaned.contains("chatgpt.com"));
        assert!(cleaned.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn replaces_existing_block_instead_of_duplicating() {
        let once = rewrite_hosts("", &["a.example".into()], GUARD_BEGIN, GUARD_END);
        let twice = rewrite_hosts(&once, &["b.example".into()], GUARD_BEGIN, GUARD_END);
        assert_eq!(twice.matches(GUARD_BEGIN).count(), 1);
        assert!(!twice.contains("a.example"));
        assert!(twice.contains("b.example"));
    }
}
