use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

fn write_docs_bindings(out_dir: &Path) {
    std::fs::copy("src/bindings-docs.rs", out_dir.join("bindings.rs"))
        .expect("copy pregenerated docs.rs bindings");
    println!("cargo:rerun-if-changed=src/bindings-docs.rs");
}

/// Locate the libntirpc object that `pkg-config` will actually link.
///
/// Explicit `-L` paths from pkg-config are checked first, in the order the
/// linker will search them. They are emitted as `cargo:rustc-link-search`
/// before `-lntirpc`, so they take precedence over any default directory; if
/// they are ignored here, a custom prefix selected via `PKG_CONFIG_PATH` could
/// be probed as the system library instead.
///
/// Only when pkg-config reports no link path (it commonly prints a bare
/// `-lntirpc`, leaving [`pkg_config::Library::link_paths`] empty) do we ask the
/// compiler to resolve `-l<name>`, which covers `LIBRARY_PATH` and the
/// standard/multiarch directories.
fn find_library(library: &pkg_config::Library, compiler: &OsStr) -> Option<PathBuf> {
    for directory in &library.link_paths {
        for lib in &library.libs {
            let exact = directory.join(format!("lib{lib}.so"));
            if exact.exists() {
                return Some(exact);
            }
        }
        for lib in &library.libs {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            let prefix = format!("lib{lib}.so.");
            let mut versioned: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
                })
                .collect();
            versioned.sort();
            if let Some(path) = versioned.pop() {
                return Some(path);
            }
        }
        for lib in &library.libs {
            let archive = directory.join(format!("lib{lib}.a"));
            if archive.exists() {
                return Some(archive);
            }
        }
    }

    for lib in &library.libs {
        for name in [format!("lib{lib}.so"), format!("lib{lib}.a")] {
            let Ok(output) = Command::new(compiler)
                .arg(format!("-print-file-name={name}"))
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            if path.is_absolute() && path.exists() {
                return Some(path);
            }
        }
    }
    None
}

/// Determine whether the linked libntirpc carries the `rdma_call_expires`
/// member of the private `struct rpc_dplx_rec`.
///
/// The member was introduced together with the exported `clnt_tli_ncreate_opt`
/// symbol (upstream v9.15), but the reported library version did not change
/// (it stayed `7.2`), so the ABI cannot be inferred from the version string.
/// Inspect the dynamic symbol table instead. `None` means "could not tell".
fn detect_rdma_call_expires(library: &pkg_config::Library, compiler: &OsStr) -> Option<bool> {
    let path = find_library(library, compiler)?;
    let static_archive = path.extension().is_some_and(|extension| extension == "a");
    let nm = env::var_os("NM").unwrap_or_else(|| "nm".into());

    let mut command = Command::new(nm);
    if !static_archive {
        command.arg("-D");
    }
    let output = command.arg("--defined-only").arg(&path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).contains("clnt_tli_ncreate_opt"))
}

/// Resolve `VFSI_LIBNTIRPC_HAS_RDMA_EXPIRES` from an explicit override, the
/// linked library's symbols, and finally the version number. Ambiguous
/// versions fail the build instead of silently selecting the wrong layout.
fn resolve_rdma_call_expires(library: &pkg_config::Library, major: u32, compiler: &OsStr) -> bool {
    println!("cargo:rerun-if-env-changed=LIBNTIRPC_RPC_DPLX_RDMA_EXPIRES");
    match env::var("LIBNTIRPC_RPC_DPLX_RDMA_EXPIRES").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        Ok(other) => panic!(
            "LIBNTIRPC_RPC_DPLX_RDMA_EXPIRES must be `0` or `1`, not {other:?}; \
             it overrides the automatic rpc_dplx_rec layout probe"
        ),
        Err(_) => match detect_rdma_call_expires(library, compiler) {
            Some(value) => value,
            None if major >= 14 => true,
            None if (7..=13).contains(&major) => panic!(
                "cannot determine the libntirpc rpc_dplx_rec layout for version {}; \
                 the `rdma_call_expires` member is present in some releases that \
                 report this same version. Set LIBNTIRPC_RPC_DPLX_RDMA_EXPIRES=0 or =1 \
                 (or install `nm` so the layout can be probed from the library).",
                library.version
            ),
            None => false,
        },
    }
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
    // Newer libntirpc's pkg-config file exposes include/ntirpc, while its
    // generated config.h includes <ntirpc/version.h> from the parent.
    let include_parent = include.parent().expect("libntirpc include has no parent");
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
    // The compiler and archiver come from the environment (CC/AR plus the
    // target-specific CC_<target>/CFLAGS_<target> and CFLAGS variants) so
    // cross builds and clang-only systems work instead of hard-coding
    // gcc/ar. They are resolved before the ABI probe because the probe asks
    // the compiler to locate the linked library.
    let target = env::var("TARGET").unwrap_or_default();
    let host = env::var("HOST").unwrap_or_default();
    let cross = !target.is_empty() && target != host;
    let target_key = target.replace('-', "_");
    let compiler = env::var_os(format!("CC_{target_key}"))
        .or_else(|| env::var_os("CC"))
        .unwrap_or_else(|| {
            if cross {
                OsString::from(format!("{target}-gcc"))
            } else {
                OsString::from("cc")
            }
        });
    let archiver = env::var_os(format!("AR_{target_key}"))
        .or_else(|| env::var_os("AR"))
        .unwrap_or_else(|| {
            if cross {
                OsString::from(format!("{target}-ar"))
            } else {
                OsString::from("ar")
            }
        });

    let has_rdma_call_expires = resolve_rdma_call_expires(&library, major, &compiler);

    // auth_destroy is a reference-counting macro/static-inline API, not an
    // exported symbol. Compile a stable callable shim so Rust never bypasses
    // libntirpc's ownership protocol by invoking ah_destroy directly.
    let helper_object = out_dir.join("auth_helpers.o");
    let mut helper_compile = Command::new(&compiler);
    helper_compile
        .arg("-c")
        .arg("-O2")
        .arg("-fPIC")
        // Supported Linux libntirpc packages build SVCXPRT with IPv6. The
        // define is not propagated through pkg-config but is part of the ABI,
        // so the shims and the bindgen output must agree on it (see below).
        .arg("-D_GNU_SOURCE=1")
        .arg("-DINET6=1")
        .arg(format!("-I{}", include.display()))
        .arg(format!("-I{}", include_parent.display()));
    if fallback_config {
        helper_compile.arg(format!("-I{}", out_dir.display()));
    }
    for variable in ["CFLAGS".to_string(), format!("CFLAGS_{target_key}")] {
        if let Some(flags) = env::var_os(&variable) {
            helper_compile.args(flags.to_string_lossy().split_whitespace());
        }
    }
    if has_rdma_call_expires {
        helper_compile.arg("-DVFSI_LIBNTIRPC_HAS_RDMA_EXPIRES=1");
    }
    if env::var_os("CARGO_FEATURE_RPCSEC_GSS").is_some() {
        helper_compile.arg("-DVFSI_RPCSEC_GSS=1");
        // GSSAPI headers are needed only for `rpcsec-gss`; the base bindings
        // deliberately avoid them. Probe the Kerberos/GSS pkg-config modules
        // for their include directory and fail early with an actionable
        // message when no GSSAPI implementation is installed.
        match ["krb5-gssapi", "mit-krb5-gssapi", "libgssglue", "gssglue"]
            .iter()
            .find_map(|name| pkg_config::Config::new().probe(name).ok())
        {
            Some(gss) => {
                for path in gss.include_paths {
                    helper_compile.arg(format!("-I{}", path.display()));
                }
            }
            None => assert!(
                Path::new("/usr/include/gssapi/gssapi.h").exists(),
                "the `rpcsec-gss` feature requires GSSAPI headers; \
                 install libkrb5-dev (or libgssglue-dev)"
            ),
        }
    }
    let status = helper_compile
        .arg("src/auth_helpers.c")
        .arg("-o")
        .arg(&helper_object)
        .status()
        .unwrap_or_else(|error| panic!("failed to run {compiler:?}: {error}"));
    assert!(
        status.success(),
        "the C compiler failed to build auth_helpers.c"
    );
    let status = Command::new(&archiver)
        .arg("rcs")
        .arg(out_dir.join("libntirpc_helpers.a"))
        .arg(&helper_object)
        .status()
        .unwrap_or_else(|error| panic!("failed to run {archiver:?}: {error}"));
    assert!(
        status.success(),
        "the archiver failed to archive auth_helpers.o"
    );
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=ntirpc_helpers");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=AR");
    println!("cargo:rerun-if-env-changed=CFLAGS");

    let mut bindings = bindgen::Builder::default()
        .header("src/wrapper.h")
        .clang_arg(format!("-I{}", include.display()))
        .clang_arg(format!("-I{}", include_parent.display()))
        // Keep bindgen's view of SVCXPRT identical to the shims compiled
        // above; otherwise `size_of::<SVCXPRT>()` (and the `xp_pktinfo`
        // union) would not match the linked library.
        .clang_arg("-D_GNU_SOURCE=1")
        .clang_arg("-DINET6=1")
        .blocklist_type("rpcblist")
        .blocklist_function("xdr_quadruple")
        .blocklist_function("strtold")
        // `_GNU_SOURCE` exposes glibc's extended-float helpers, which are
        // irrelevant to libntirpc and reference `_Float32x`/`_Float64x`/
        // `_Float128` types that bindgen cannot represent portably.
        .blocklist_type("_Float32")
        .blocklist_type("_Float32x")
        .blocklist_type("_Float64x")
        .blocklist_type("_Float128")
        .blocklist_type("_Float128x")
        .blocklist_item("strtof(32|64|128)x?(_l)?")
        .blocklist_item("strfromf(32|64|128)x?(_l)?")
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
