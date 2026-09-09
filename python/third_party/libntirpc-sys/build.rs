use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

// Keep Python wheels and source builds reproducible. Updating ntirpc is an
// explicit dependency change that must go through CI, rather than whatever
// happened to be at the tip of its default branch during a user's install.
const LIBNTIRPC_REVISION: &str = "24a7d5fca4c2e3e3cc8bb94bc301d8c2a40677a8"; // v15.2

static OUT_DIR: LazyLock<PathBuf> = LazyLock::new(|| PathBuf::from(env::var("OUT_DIR").unwrap()));
static LIBNTIRPC_DIR: LazyLock<PathBuf> = LazyLock::new(|| OUT_DIR.join("ntirpc"));
static LIBNTIRPC_BUILD_DIR: LazyLock<PathBuf> = LazyLock::new(|| LIBNTIRPC_DIR.join("build"));
static LIBNTIRPC_INSTALL_DIR: LazyLock<PathBuf> = LazyLock::new(|| LIBNTIRPC_DIR.join("install"));

fn run<P: AsRef<Path>>(mut cmd: Command, path: P) {
    let dir = OUT_DIR.join(path.as_ref());
    println!("Running {:?} in {:?}", cmd, dir);
    let status = cmd
        .current_dir(dir)
        .status()
        .expect("failed to start native dependency build command");
    assert!(status.success(), "native dependency build command failed");
}

fn download_and_extract() {
    let mut clone = Command::new("git");
    clone.args([
        "clone",
        "--no-checkout",
        "https://github.com/nfs-ganesha/ntirpc.git",
        "ntirpc",
    ]);
    run(clone, "");

    let mut checkout = Command::new("git");
    checkout.args(["checkout", "--detach", LIBNTIRPC_REVISION]);
    run(checkout, "ntirpc");

    let mut submodules = Command::new("git");
    submodules.args(["submodule", "update", "--init", "--recursive"]);
    run(submodules, "ntirpc");
}

/// Prepare the upstream source for embedding. Force a static build so the
/// archive ends up in the install tree, and skip upstream's C test programs:
/// they are unrelated to the bindings and one unconditionally links the
/// monitoring library even when `USE_MONITORING=Off`.
fn prepare_source() {
    let src_cmake = LIBNTIRPC_DIR.join("src/CMakeLists.txt");
    let patched = std::fs::read_to_string(&src_cmake)
        .expect("read ntirpc src/CMakeLists.txt")
        .replace("add_library(ntirpc SHARED", "add_library(ntirpc STATIC");
    std::fs::write(&src_cmake, patched).expect("patch ntirpc CMakeLists.txt to STATIC");

    let root_cmake = LIBNTIRPC_DIR.join("CMakeLists.txt");
    let patched = std::fs::read_to_string(&root_cmake)
        .expect("read ntirpc CMakeLists.txt")
        .replace(
            "add_subdirectory(tests)",
            "# Tests disabled by libntirpc-sys",
        );
    std::fs::write(&root_cmake, patched).expect("disable ntirpc C tests");
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
        prepare_source();
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
