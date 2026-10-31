use std::env;
use std::path::PathBuf;

// Bindgen's raw bitfield/flexible-array accessors expose the C pointer
// contract directly. Keep their audited obligations next to generated unsafe
// blocks instead of exempting the entire generated module from the lint.
fn document_raw_accessors(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    // Bindgen can emit either formatted or single-line Rust when rustfmt is
    // unavailable. Locate each block and its enclosing generated function
    // independently of physical lines.
    for (offset, _) in source.match_indices("unsafe {") {
        let function = source[..offset]
            .rsplit_once("fn ")
            .expect("unsafe helper must have a function")
            .1
            .split(['(', '<'])
            .next()
            .unwrap_or("")
            .trim();
        let reason = if function.starts_with("raw_") || function.ends_with("_raw") {
            "the unsafe raw accessor requires live backing storage and an in-bounds bit range; writes require exclusive access."
        } else if matches!(function, "_flags2" | "set__flags2" | "new_bitfield_1") {
            "the generated conversion is between same-width C integers; every bit pattern is valid for both representations."
        } else if matches!(function, "as_slice" | "as_mut_slice") {
            "the unsafe flexible-array accessor requires len initialized elements in the backing allocation, exclusively borrowed for mutation."
        } else {
            panic!("review new bindgen unsafe helper: {function}");
        };
        let prefix = source[..offset].rsplit('\n').next().unwrap_or("");
        let indent = &prefix[..prefix.len() - prefix.trim_start().len()];
        output.push_str(&source[cursor..offset]);
        if !prefix.trim().is_empty() {
            output.push('\n');
            output.push_str(indent);
        }
        output.push_str("// SAFETY: ");
        output.push_str(reason);
        output.push('\n');
        output.push_str(indent);
        output.push_str("unsafe {");
        cursor = offset + "unsafe {".len();
    }
    output.push_str(&source[cursor..]);
    output
}

fn main() {
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest_dir.join("src");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    if env::var_os("DOCS_RS").is_some() {
        let source = std::fs::read_to_string(src_dir.join("bindings-docs.rs"))
            .expect("read pregenerated docs.rs bindings");
        std::fs::write(out_dir.join("bindings.rs"), document_raw_accessors(&source))
            .expect("write documented docs.rs bindings");
        println!("cargo:rerun-if-changed=src/bindings-docs.rs");
        return;
    }
    // Include directory provided by libntirpc-sys (via its `links` metadata).
    let ntirpc_inc = env::var("DEP_NTIRPC_INCLUDE")
        .expect("DEP_NTIRPC_INCLUDE not set; libntirpc-sys must be a dependency");
    // The installed config.h includes "ntirpc/version.h", resolved through
    // the bundled installation's parent include directory.
    let ntirpc_build_inc = env::var("DEP_NTIRPC_INCLUDE2").unwrap_or_default();

    // Compile the wrappers around the nfsv41.h static inline codecs.
    // cc honors Cargo's target, CC/AR/CFLAGS overrides, and emits their rebuild
    // dependencies. In particular, CI's sanitizer flags must reach these codecs.
    let mut native = cc::Build::new();
    native
        .file(src_dir.join("wrapper.c"))
        .opt_level(2)
        // Preserve the generated codecs' previous compiler-default warnings.
        .warnings(false)
        // The archive is linked into the Python extension shared object.
        .pic(true)
        .flag("-fno-strict-aliasing")
        // Ganesha's codec calls xdr_pointer with `char **` where ntirpc's
        // prototype takes `void **`; GCC 14 turns this constraint violation
        // into an error by default.
        .flag("-Wno-incompatible-pointer-types")
        .include(&ntirpc_inc)
        .include(&src_dir);
    if !ntirpc_build_inc.is_empty() {
        native.include(&ntirpc_build_inc);
    }
    native.compile("nfsv41");
    // libntirpc-sys owns linkage of the packaged static archive and its
    // feature-dependent system dependencies. Never select a second ntirpc.
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
        .wrap_unsafe_ops(true)
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
    std::fs::write(&bindings_path, document_raw_accessors(&rewritten)).expect("rewrite bindings");
}
