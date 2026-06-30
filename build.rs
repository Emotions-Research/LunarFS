fn main() {
    let feature_fuse = std::env::var("CARGO_FEATURE_FUSE").is_ok();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if feature_fuse && target_os == "macos" {
        let linked_via_pkgconfig = pkg_config::Config::new()
            .atleast_version("1.0")
            .probe("fuse-t")
            .is_ok();

        if !linked_via_pkgconfig {
            println!("cargo:rustc-link-search=native=/usr/local/lib");
            println!("cargo:rustc-link-lib=dylib=fuse-t");
        }

        // Always emit rpath so the binary finds libfuse-t.dylib at runtime
        // without requiring DYLD_LIBRARY_PATH (the prior build omitted this).
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
    }
}
