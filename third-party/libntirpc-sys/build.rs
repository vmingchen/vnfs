use std::env;
use std::path::PathBuf;
use std::process::Command;

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
        .atleast_version("6.3")
        .probe("libntirpc")
        .expect("libntirpc >= 6.3 was not found; install libntirpc-dev");
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
    // Ubuntu 26.04's libntirpc-dev 6.3 package installs headers that include
    // config.h, but omits that generated header on x86_64. Supply the
    // matching distro configuration only for that known packaging defect.
    let fallback_config = if include.join("config.h").exists() {
        false
    } else {
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let ubuntu_26_04 = os_release.lines().any(|line| line == "ID=ubuntu")
            && os_release
                .lines()
                .any(|line| line == "VERSION_ID=\"26.04\"");
        assert!(
            ubuntu_26_04 && library.version.starts_with("6.3"),
            "libntirpc development headers are missing config.h"
        );
        std::fs::copy("src/config-ubuntu-26.04.h", out_dir.join("config.h"))
            .expect("copy Ubuntu 26.04 libntirpc config fallback");
        println!("cargo:rerun-if-changed=src/config-ubuntu-26.04.h");
        true
    };

    // auth_destroy is a reference-counting macro/static-inline API, not an
    // exported symbol. Compile a stable callable shim so Rust never bypasses
    // libntirpc's ownership protocol by invoking ah_destroy directly.
    let helper_object = out_dir.join("auth_helpers.o");
    let mut helper_compile = Command::new("gcc");
    helper_compile
        .arg("-c")
        .arg("-O2")
        .arg("-fPIC")
        // Supported Linux libntirpc packages build SVCXPRT with IPv6. The
        // define is not propagated through pkg-config but is part of the ABI.
        .arg("-D_GNU_SOURCE=1")
        .arg("-DINET6=1")
        .arg(format!("-I{}", include.display()));
    if fallback_config {
        helper_compile.arg(format!("-I{}", out_dir.display()));
    }
    if env::var_os("CARGO_FEATURE_RPCSEC_GSS").is_some() {
        helper_compile.arg("-DVFSI_RPCSEC_GSS=1");
        if major >= 6 {
            helper_compile.arg("-DVFSI_LIBNTIRPC_HAS_CLIENT_XPRT=1");
        }
        if major >= 9 {
            helper_compile.arg("-DVFSI_LIBNTIRPC_HAS_RDMA_EXPIRES=1");
        }
    }
    let status = helper_compile
        .arg("src/auth_helpers.c")
        .arg("-o")
        .arg(&helper_object)
        .status()
        .expect("run gcc for libntirpc auth helper");
    assert!(status.success(), "gcc failed to compile auth_helpers.c");
    let status = Command::new("ar")
        .arg("rcs")
        .arg(out_dir.join("libntirpc_helpers.a"))
        .arg(&helper_object)
        .status()
        .expect("run ar for libntirpc auth helper");
    assert!(status.success(), "ar failed to archive auth_helpers.o");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=ntirpc_helpers");

    let mut bindings = bindgen::Builder::default()
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
        .blocklist_function("qgcvt");
    if fallback_config {
        bindings = bindings.clang_arg(format!("-I{}", out_dir.display()));
    }
    bindings
        .generate()
        .expect("generate libntirpc bindings")
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("write libntirpc bindings");

    println!("cargo:include={}", include.display());
    println!("cargo:include2=/usr/include");
    println!("cargo:rerun-if-changed=src/wrapper.h");
    println!("cargo:rerun-if-changed=src/auth_helpers.c");
}
