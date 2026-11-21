fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let config_path = std::path::PathBuf::from(&crate_dir).join("cbindgen.toml");
    let config = cbindgen::Config::from_file(config_path).expect("read cbindgen.toml");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let out = out_dir.join("vfsi.h");
    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
        .expect("generate vfsi.h")
        .write_to_file(out);

    // Retain a relocatable layout for any shared ntirpc objects supplied by
    // a dependency override. Normal libntirpc-sys builds link ntirpc statically;
    // their vfsi-libs directory need not be deployed. Shared overrides can use
    // the origin-relative RUNPATH without exposing Cargo's nested build path.
    // Keep a build-specific copy for the SDK installer. An empty directory
    // marks a static build so the installer can ignore stale profile files.
    let runtime_dir = out_dir.join("vfsi-libs");
    match std::fs::remove_dir_all(&runtime_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("reset build-specific vfsi-libs: {error}"),
    }
    std::fs::create_dir(&runtime_dir).expect("create build-specific vfsi-libs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" || target_os == "macos" {
        let ntirpc_include = std::path::PathBuf::from(
            std::env::var("DEP_NTIRPC_INCLUDE").expect("DEP_NTIRPC_INCLUDE not set"),
        );
        let ntirpc_install = ntirpc_include
            .parent()
            .and_then(std::path::Path::parent)
            .expect("unexpected libntirpc include path");
        let ntirpc_lib = ntirpc_install.join("lib");
        let profile_dir = out_dir
            .ancestors()
            .nth(3)
            .expect("unexpected Cargo OUT_DIR");
        let bundled_dir = profile_dir.join("vfsi-libs");
        let library_marker = if target_os == "linux" {
            ".so"
        } else {
            ".dylib"
        };
        let loader_origin = if target_os == "linux" {
            "$ORIGIN"
        } else {
            "@loader_path"
        };

        std::fs::create_dir_all(&bundled_dir).expect("create vfsi-libs directory");
        for entry in std::fs::read_dir(&ntirpc_lib).expect("read libntirpc directory") {
            let entry = entry.expect("read libntirpc entry");
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("libntirpc")
                && name.to_string_lossy().contains(library_marker)
            {
                let runtime = runtime_dir.join(&name);
                std::fs::copy(entry.path(), &runtime)
                    .expect("copy build-specific libntirpc runtime library");
                std::fs::copy(runtime, bundled_dir.join(name))
                    .expect("copy profile libntirpc runtime library");
            }
        }
        println!("cargo:rustc-link-arg=-Wl,-rpath,{loader_origin}/vfsi-libs");
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
