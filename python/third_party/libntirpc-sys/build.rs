use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

static OUT_DIR: LazyLock<PathBuf> = LazyLock::new(|| PathBuf::from(env::var("OUT_DIR").unwrap()));
static LIBNTIRPC_DIR: LazyLock<PathBuf> = LazyLock::new(|| OUT_DIR.join("ntirpc"));
static LIBNTIRPC_BUILD_DIR: LazyLock<PathBuf> = LazyLock::new(|| LIBNTIRPC_DIR.join("build"));
static LIBNTIRPC_INSTALL_DIR: LazyLock<PathBuf> = LazyLock::new(|| LIBNTIRPC_DIR.join("install"));

fn run<P: AsRef<Path>>(mut cmd: Command, path: P) {
    let dir = OUT_DIR.join(path.as_ref());
    println!("Running {:?} in {:?}", cmd, dir);
    cmd.current_dir(dir).status().unwrap();
}

fn download_and_extract() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("git clone --recursive https://github.com/nfs-ganesha/ntirpc.git");
    run(cmd, "");
}

/// Force a static build of libntirpc. The upstream CMake hardcodes
/// `add_library(ntirpc SHARED ...)`; replace it so the archive ends up in the
/// install tree and the Rust extension links statically (no `LD_LIBRARY_PATH`
/// needed at import time).
fn force_static() {
    let src_cmake = LIBNTIRPC_DIR.join("src/CMakeLists.txt");
    let patched = std::fs::read_to_string(&src_cmake)
        .expect("read ntirpc src/CMakeLists.txt")
        .replace("add_library(ntirpc SHARED", "add_library(ntirpc STATIC");
    std::fs::write(&src_cmake, patched).expect("patch ntirpc CMakeLists.txt to STATIC");
}

fn configure() {
    let mut cmd = Command::new("mkdir");
    cmd.arg(&*LIBNTIRPC_BUILD_DIR);
    run(cmd, "");
    let mut cmd = Command::new("cmake");
    cmd.arg("-Wno-dev"); // supress developer warnings
    cmd.arg("-DUSE_LTTNG=Off");
    // Disable the bundled prometheus monitoring stack: its global teardown
    // corrupts the heap on exit (intermittent SIGABRT), and nothing in the
    // NFS client uses it.
    cmd.arg("-DUSE_MONITORING=Off");
    cmd.arg("-DCMAKE_BUILD_TYPE=RelWithDebInfo");
    // Objects linked into the Python extension (a shared object) must be
    // position-independent; the static library is compiled without -fPIC by
    // default.
    cmd.arg("-DCMAKE_POSITION_INDEPENDENT_CODE=ON");
    cmd.arg(format!(
        "-DCMAKE_INSTALL_PREFIX={}",
        LIBNTIRPC_INSTALL_DIR.display()
    ));
    cmd.arg(&*LIBNTIRPC_DIR);
    run(cmd, &*LIBNTIRPC_BUILD_DIR);
}

fn make() {
    let mut cmd = Command::new("make");
    cmd.arg(format!(
        "-j{}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
    ));
    run(cmd, &*LIBNTIRPC_BUILD_DIR);
}

fn install() {
    let mut cmd = Command::new("make");
    cmd.arg(format!(
        "-j{}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
    ));
    cmd.arg("install");
    run(cmd, &*LIBNTIRPC_BUILD_DIR);
}

fn main() {
    if !LIBNTIRPC_DIR.exists() {
        download_and_extract();
    }

    if !LIBNTIRPC_INSTALL_DIR.exists() {
        force_static();
        configure();
    }
    make();
    install();

    println!(
        "cargo:rustc-link-search=native={}/lib",
        LIBNTIRPC_INSTALL_DIR.display()
    );
    println!("cargo:rustc-link-lib=static=ntirpc");
    // The static archive pulls in pthread/dl symbols; declare them so the
    // final cdylib links cleanly.
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=dl");
    // Some ntirpc objects reference the GSSAPI OID constant (even when the
    // server-side GSS stack is unused); resolve it from the system library.
    println!("cargo:rustc-link-lib=gssapi_krb5");
    // libntirpc uses userspace RCU for its connection bookkeeping.
    println!("cargo:rustc-link-lib=urcu-bp");

    // Expose the ntirpc include directory to dependent crates via the `links`
    // mechanism (DEP_NTIRPC_INCLUDE build-script env var).
    println!(
        "cargo:include={}/include/ntirpc",
        LIBNTIRPC_INSTALL_DIR.display()
    );
    // The installed config.h includes "ntirpc/version.h", which is only
    // resolvable from the build tree; expose that too (DEP_NTIRPC_INCLUDE2).
    println!("cargo:include2={}", LIBNTIRPC_BUILD_DIR.display());

    bindgen::Builder::default()
        .header(concat!(env!("CARGO_MANIFEST_DIR"), "/src/wrapper.h"))
        .clang_arg(format!(
            "-I{}/include/ntirpc",
            LIBNTIRPC_INSTALL_DIR.display()
        ))
        .clang_arg(format!("-I{}", LIBNTIRPC_BUILD_DIR.display()))
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
        .write_to_file(OUT_DIR.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
