#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let result = if std::env::args_os().len() == 1 {
        gptguard::ui::run()
    } else {
        gptguard::cli::run()
    };
    if let Err(err) = result {
        // GUI launches have no console. Keep the error in the application log
        // and show it through the next native status refresh instead of
        // opening a terminal window for beginners.
        let paths = gptguard::config::AppPaths::discover();
        let _ = paths.ensure();
        let _ = gptguard::util::init_logging(&paths.log_dir);
        gptguard::util::error(format!("启动失败: {err:#}"));
        if std::env::args_os().len() > 1 {
            eprintln!("错误: {err:#}");
            std::process::exit(1);
        }
    }
}
