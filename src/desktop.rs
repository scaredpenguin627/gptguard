//! An explicit tray quit belongs to the current desktop session. A worker
//! restart or a temporarily unreachable management API is not a quit.
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{ensure, Context, Result};

use crate::{config::AppPaths, util};

const GENERATION_ENV: &str = "GPTGUARD_DESKTOP_GENERATION";
pub(crate) const QUIT_POLL: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub(crate) struct DesktopSession {
    marker: PathBuf,
    generation: u64,
}

fn generation(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.len()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err).context("读取桌面退出通知"),
    }
}

impl DesktopSession {
    pub fn join(paths: &AppPaths) -> Result<Self> {
        Self::open(
            paths.data_dir.join("desktop-exits"),
            std::env::var_os(GENERATION_ENV),
        )
    }

    fn open(marker: PathBuf, inherited: Option<std::ffi::OsString>) -> Result<Self> {
        let generation = generation(&marker)?;
        if let Some(inherited) = inherited {
            ensure!(
                inherited.to_str().and_then(|s| s.parse().ok()) == Some(generation),
                "GPTGuard 已从托盘退出"
            );
        }
        Ok(Self { marker, generation })
    }

    pub fn exit_requested(&self) -> bool {
        generation(&self.marker).is_ok_and(|current| current > self.generation)
    }

    pub fn request_exit(&self) -> Result<()> {
        // One append per explicit quit. The size is the generation, so readers
        // never observe a partially replaced token or rely on clock precision.
        if !self.exit_requested() {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.marker)?
                .write_all(b"\n")
                .context("通知 GPTGuard 退出")?;
        }
        Ok(())
    }

    pub async fn wait_for_exit(&self) {
        while !self.exit_requested() {
            tokio::time::sleep(QUIT_POLL).await;
        }
    }

    pub fn spawn(&self, args: &[&str]) -> Result<()> {
        ensure!(!self.exit_requested(), "GPTGuard 正在退出");
        // Even a child scheduled after the quit must belong to its parent's
        // session. An independent launch from Explorer joins the new one.
        let mut command = Command::new(util::exe_path());
        command
            .args(args)
            .env(GENERATION_ENV, self.generation.to_string());
        util::spawn_background(&mut command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_child_cannot_reopen_a_desktop_after_quit() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("exits");
        let old = DesktopSession::open(marker.clone(), None).unwrap();
        let inherited = old.generation.to_string();
        let window = DesktopSession::open(marker.clone(), Some(inherited.clone().into())).unwrap();
        old.request_exit().unwrap();
        assert!(window.exit_requested());
        assert!(window.spawn(&[]).is_err());
        assert!(DesktopSession::open(marker.clone(), Some(inherited.into())).is_err());
        let relaunched = DesktopSession::open(marker, None).unwrap();
        old.request_exit().unwrap();
        assert!(!relaunched.exit_requested());
    }
}
