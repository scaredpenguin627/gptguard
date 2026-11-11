//! Small shared helpers. No business logic lives here.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static ELEVATED: OnceLock<bool> = OnceLock::new();

pub fn init_logging(log_dir: &Path) -> Result<()> {
    fs::create_dir_all(log_dir)?;
    let path = log_dir.join(format!(
        "aiguard-{}.log",
        chrono::Local::now().format("%Y%m%d")
    ));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open log {}", path.display()))?;
    let _ = LOG_FILE.set(Mutex::new(file));
    Ok(())
}

pub fn log(level: &str, message: impl AsRef<str>) {
    let line = format!(
        "{} {:<5} {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        level,
        message.as_ref()
    );
    eprint!("{line}");
    if let Some(file) = LOG_FILE.get() {
        if let Ok(mut file) = file.lock() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

pub fn info(message: impl AsRef<str>) {
    log("INFO", message);
}
pub fn warn(message: impl AsRef<str>) {
    log("WARN", message);
}
pub fn error(message: impl AsRef<str>) {
    log("ERROR", message);
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339()
}

pub fn is_elevated() -> bool {
    *ELEVATED.get_or_init(|| {
        #[cfg(windows)]
        {
            Command::new("net")
                .arg("session")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        }
        #[cfg(unix)]
        {
            unsafe { libc::geteuid() == 0 }
        }
        #[cfg(not(any(windows, unix)))]
        {
            false
        }
    })
}

pub fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        text.push_str(&stderr);
        anyhow::bail!("{program} exited with {}: {}", output.status, text.trim());
    }
    Ok(text)
}

/// Return canonical paths of running processes whose executable matches any target.
pub fn find_processes(paths: &[String]) -> Vec<(u32, String, String)> {
    if paths.is_empty() {
        return Vec::new();
    }
    let wanted: Vec<String> = paths
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| normalize_path(Path::new(p)))
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut system = match System::new_all() {
        Ok(system) => system,
        Err(_) => return Vec::new(),
    };
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
    );
    let mut found = Vec::new();
    for (pid, process) in system.processes() {
        if let Some(exe) = process.exe() {
            let normalized = normalize_path(exe);
            if wanted.iter().any(|want| want == &normalized) {
                found.push((
                    pid.as_u32(),
                    process.name().to_string_lossy().into_owned(),
                    exe.display().to_string(),
                ));
            }
        }
    }
    found
}

pub fn kill_processes(paths: &[String]) -> Vec<String> {
    let mut killed = Vec::new();
    let Ok(system) = System::new_all() else {
        return killed;
    };
    for (pid, name, exe) in find_processes(paths) {
        if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
            if process.kill() {
                killed.push(format!("{name} ({pid}) {exe}"));
            }
        }
    }
    killed
}

pub fn normalize_path(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let canonical = absolute.canonicalize().unwrap_or(absolute);
    let text = canonical.display().to_string();
    #[cfg(windows)]
    {
        text.to_lowercase().replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        text
    }
}

pub fn mask_url(value: &str) -> String {
    match url::Url::parse(value) {
        Ok(mut url) => {
            if !url.username().is_empty() {
                let _ = url.set_username("***");
            }
            if url.password().is_some() {
                let _ = url.set_password(Some("***"));
            }
            url.to_string()
        }
        Err(_) => "<invalid-url>".into(),
    }
}

pub fn read_tail(path: &Path, lines: usize) -> Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut collected: Vec<&str> = text.lines().rev().take(lines).collect();
    collected.reverse();
    Ok(collected.join("\n"))
}

pub fn open_url(url: &str) -> Result<()> {
    open::that(url).context("open browser")
}

pub fn spawn_detached(program: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {}", program.display()))?;
    Ok(())
}

pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("aiguard.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_hex_and_stable() {
        let first = sha256_hex(b"aiguard");
        assert_eq!(first.len(), 64);
        assert_eq!(first, sha256_hex(b"aiguard"));
        assert_ne!(first, sha256_hex(b"aiguard2"));
    }

    #[test]
    fn masks_credentials() {
        let masked = mask_url("socks5://user:pass@127.0.0.1:1080");
        assert!(masked.contains("***"));
        assert!(!masked.contains("pass"));
    }
}
