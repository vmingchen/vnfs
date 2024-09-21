use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

static OUT_DIR: LazyLock<PathBuf> = LazyLock::new(|| PathBuf::from(env::var("OUT_DIR").unwrap()));
static ROOT_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()));
static SRC_DIR: LazyLock<PathBuf> = LazyLock::new(|| ROOT_DIR.join("src"));

fn run<P: AsRef<Path>>(mut cmd: Command, path: P) {
    let dir = OUT_DIR.join(path.as_ref());
    println!("Running {:?} in {:?}", cmd, dir);
    cmd.current_dir(dir).status().unwrap();
}

fn make() {
    let mut cmd = Command::new("make");
    cmd.arg("all");
    run(cmd, &*SRC_DIR);

    let mut cmd = Command::new("mv");
    cmd.arg("libsumlib.a");
    cmd.arg("sum_server");
    cmd.arg(format!("{}", OUT_DIR.display()));
    run(cmd, &*SRC_DIR);

    let mut cmd = Command::new("make");
    cmd.arg("clean");
    run(cmd, &*SRC_DIR);
}

fn main() {
    make();
    // println!("cargo:rustc-link-search=native={}", OUT_DIR.display());
    println!("cargo:rustc-link-search={}", OUT_DIR.display());
    println!(
        "cargo:rustc-link-search=/usr/lib/{}-{}-{}",
        env::var("CARGO_CFG_TARGET_ARCH").unwrap(),
        env::var("CARGO_CFG_TARGET_OS").unwrap(),
        env::var("CARGO_CFG_TARGET_ENV").unwrap(),
    );
    println!("cargo:rustc-link-lib=static=tirpc");
    println!("cargo:rustc-link-lib=static=sumlib");
    bindgen::Builder::default()
        .header(format!("{}/sum.h", SRC_DIR.display()))
        .clang_arg("-I/usr/include/tirpc")
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
