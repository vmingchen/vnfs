use std::env;

fn main() {
    println!(
        "cargo:rustc-link-search=/usr/lib/{}-{}-{}",
        env::var("CARGO_CFG_TARGET_ARCH").unwrap(),
        env::var("CARGO_CFG_TARGET_OS").unwrap(),
        env::var("CARGO_CFG_TARGET_ENV").unwrap(),
    );
    println!("cargo:rustc-link-search=native={}", env::var("OUT_DIR").unwrap());
    println!("cargo:rustc-link-lib=static=sumlib");
    println!("cargo:rustc-link-lib=dylib=tirpc");
}