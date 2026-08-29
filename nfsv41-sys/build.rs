use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest_dir.join("src");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    // The python/ workspace builds a standalone extension and wants libntirpc
    // linked statically (see python/third_party/libntirpc-sys/build.rs). Only
    // that workspace sets VNFS_STATIC_NTIRPC, so the main workspace keeps the
    // historical dynamic-link flags.
    let static_ntirpc = env::var("VNFS_STATIC_NTIRPC").as_deref() == Ok("1");

    // Include directory provided by libntirpc-sys (via its `links` metadata).
    let ntirpc_inc = env::var("DEP_NTIRPC_INCLUDE")
        .expect("DEP_NTIRPC_INCLUDE not set; libntirpc-sys must be a dependency");
    // The installed config.h includes "ntirpc/version.h", resolvable only
    // from the libntirpc build tree (DEP_NTIRPC_INCLUDE2).
    let ntirpc_build_inc = env::var("DEP_NTIRPC_INCLUDE2").unwrap_or_default();

    // Compile the wrappers around the nfsv41.h static inline codecs.
    let mut gcc = Command::new("gcc");
    gcc.arg("-c")
        .arg("-O2")
        .arg("-fno-strict-aliasing")
        // Ganesha's codec calls xdr_pointer with `char **` where ntirpc's
        // prototype takes `void **`; GCC 14 turns this constraint violation
        // into an error by default.
        .arg("-Wno-incompatible-pointer-types")
        .arg(format!("-I{}", ntirpc_inc))
        .arg(format!("-I{}", src_dir.display()));
    if !ntirpc_build_inc.is_empty() {
        gcc.arg(format!("-I{}", ntirpc_build_inc));
    }
    let status = gcc
        .arg(src_dir.join("wrapper.c"))
        .arg("-o")
        .arg(out_dir.join("wrapper.o"))
        .status()
        .unwrap();
    assert!(status.success(), "gcc failed to compile wrapper.c");

    let status = Command::new("ar")
        .arg("rcs")
        .arg(out_dir.join("libnfsv41.a"))
        .arg(out_dir.join("wrapper.o"))
        .status()
        .unwrap();
    assert!(status.success(), "ar failed");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=nfsv41");
    if static_ntirpc {
        // wrapper.o (embedded in this crate's rlib) references symbols in
        // libntirpc; resolve them from the static archive.
        println!("cargo:rustc-link-lib=static=ntirpc");
    } else {
        // wrapper.o references symbols in libntirpc.so; re-assert the link so
        // consumers (including this crate's own tests) get the dynamic
        // library linked too.
        println!("cargo:rustc-link-lib=dylib=ntirpc");
        println!("cargo:rustc-link-lib=dylib=ntirpcmonitoring");
    }
    println!("cargo:rerun-if-changed=src/wrapper.c");
    println!("cargo:rerun-if-changed=src/wrapper.h");
    println!("cargo:rerun-if-changed=src/nfsv41.h");
    println!("cargo:rerun-if-changed=src/ganesha_rpc.h");
    println!("cargo:rerun-if-changed=src/config.h");

    let mut bindgen = bindgen::Builder::default()
        .header(src_dir.join("wrapper.h").to_str().unwrap())
        .clang_arg(format!("-I{}", ntirpc_inc))
        .clang_arg(format!("-I{}", src_dir.display()));
    if !ntirpc_build_inc.is_empty() {
        bindgen = bindgen.clang_arg(format!("-I{}", ntirpc_build_inc));
    }
    bindgen
        .blocklist_type("rpcblist")
        // Following are unsupported because of usage u128
        .blocklist_function("xdr_quadruple")
        .blocklist_function("strtold")
        .blocklist_type("_Float64x")
        .blocklist_function("qecvt_r")
        .blocklist_function("qfcvt_r")
        .blocklist_function("qecvt")
        .blocklist_function("qfcvt")
        .blocklist_function("qgcvt")
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    // Edition 2024 requires extern blocks to be `unsafe`; rewrite the
    // generated ones that bindgen still emits as plain `extern "C" {`,
    // leaving the blocks bindgen already marks `unsafe` untouched.
    let bindings_path = out_dir.join("bindings.rs");
    let generated = std::fs::read_to_string(&bindings_path).expect("read bindings");
    let rewritten = generated
        .replace("unsafe extern \"C\" {", "\u{0}unsafe_extern\u{0}")
        .replace("extern \"C\" {", "unsafe extern \"C\" {")
        .replace("\u{0}unsafe_extern\u{0}", "unsafe extern \"C\" {");
    std::fs::write(&bindings_path, rewritten).expect("rewrite bindings");
}
