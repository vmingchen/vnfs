fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let config_path = std::path::PathBuf::from(&crate_dir).join("cbindgen.toml");
    let config = cbindgen::Config::from_file(config_path).expect("read cbindgen.toml");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("vfsi.h");
    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
        .expect("generate vfsi.h")
        .write_to_file(out);

    // Keep the cdylib relocatable: collect libntirpc's shared objects beside
    // the final library and add an origin-relative RUNPATH. Consumers can
    // deploy libvfsi_c.so together with the vfsi-libs directory without
    // knowing Cargo's nested build directory.
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
        let profile_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap())
            .ancestors()
            .nth(3)
            .expect("unexpected Cargo OUT_DIR")
            .to_path_buf();
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
                std::fs::copy(entry.path(), bundled_dir.join(name))
                    .expect("copy libntirpc runtime library");
            }
        }
        println!("cargo:rustc-link-arg=-Wl,-rpath,{loader_origin}/vfsi-libs");
    }
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
}
