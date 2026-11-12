fn main() {
    println!("cargo:rerun-if-changed=ui/main.slint");
    println!("cargo:rerun-if-env-changed=GPTGUARD_MIHOMO_PATH");
    slint_build::compile("ui/main.slint").expect("compile Slint UI");

    // Release builds provide the official Windows Mihomo executable through
    // GPTGUARD_MIHOMO_PATH.  Developer builds still compile without network
    // access and use an empty marker; the runtime reports a clear diagnostic
    // if a Mihomo route is enabled in such a build.
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let target = out.join("mihomo-core.bin");
    if let Some(source) = std::env::var_os("GPTGUARD_MIHOMO_PATH") {
        let source = std::path::PathBuf::from(source);
        let metadata = std::fs::metadata(&source)
            .unwrap_or_else(|err| panic!("GPTGUARD_MIHOMO_PATH is invalid: {err}"));
        assert!(
            metadata.is_file(),
            "GPTGUARD_MIHOMO_PATH must point to a file"
        );
        std::fs::copy(&source, &target).unwrap_or_else(|err| panic!("copy embedded Mihomo: {err}"));
    } else {
        std::fs::write(&target, b"").expect("write embedded Mihomo marker");
    }
}
