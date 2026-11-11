#![cfg_attr(windows, windows_subsystem = "windows")]

//! Double-click entry point. It starts the tray, makes sure the daemon is
//! running, and opens the local panel once. The CLI stays in `gptguard.exe`.

fn main() {
    if let Err(err) = gptguard::tray::run_gui() {
        let paths = gptguard::config::AppPaths::discover();
        let _ = paths.ensure();
        let _ = gptguard::util::init_logging(&paths.log_dir);
        gptguard::util::error(format!("GUI startup failed: {err:#}"));
    }
}
