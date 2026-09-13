use std::env;
use std::path::PathBuf;

fn write_docs_bindings(out_dir: &std::path::Path) {
    std::fs::copy("src/bindings-docs.rs", out_dir.join("bindings.rs"))
        .expect("copy pregenerated docs.rs bindings");
    println!("cargo:rerun-if-changed=src/bindings-docs.rs");
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by Cargo"));

    // docs.rs builds without network access or native development packages.
    // Rustdoc needs type declarations, but it never links or executes RPC
    // symbols, so checked-in bindings are sufficient for that environment.
    if env::var_os("DOCS_RS").is_some() {
        write_docs_bindings(&out_dir);
        println!("cargo:legacy_free_cb=0");
        return;
    }

    // Production builds deliberately use the administrator-provided system
    // library. This keeps Cargo builds offline/hermetic: build.rs never clones
    // or downloads native source code behind Cargo's back.
    let library = pkg_config::Config::new()
        .atleast_version("4.3")
        .probe("libntirpc")
        .expect("libntirpc >= 4.3 was not found; install libntirpc-dev");
    let major = library
        .version
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    if major < 15 {
        println!("cargo:legacy_free_cb=1");
    } else {
        println!("cargo:legacy_free_cb=0");
    }
    let include = library
        .include_paths
        .first()
        .expect("libntirpc pkg-config metadata has no include directory");

    bindgen::Builder::default()
        .header("src/wrapper.h")
        .clang_arg(format!("-I{}", include.display()))
        .blocklist_type("rpcblist")
        .blocklist_function("xdr_quadruple")
        .blocklist_function("strtold")
        .blocklist_type("_Float64x")
        .blocklist_function("qecvt_r")
        .blocklist_function("qfcvt_r")
        .blocklist_function("qecvt")
        .blocklist_function("qfcvt")
        .blocklist_function("qgcvt")
        .generate()
        .expect("generate libntirpc bindings")
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("write libntirpc bindings");

    println!("cargo:include={}", include.display());
    println!("cargo:include2=/usr/include");
    println!("cargo:rerun-if-changed=src/wrapper.h");
}
