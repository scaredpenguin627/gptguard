fn main() {
    if let Err(err) = gptguard::cli::run() {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}
