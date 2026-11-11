fn main() {
    if let Err(err) = aiguard::cli::run() {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}
