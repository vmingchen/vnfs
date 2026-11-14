use std::process::Command;
fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC");
    let compiler = std::env::var_os("RUSTC").expect("Cargo provides RUSTC");
    let output = Command::new(compiler)
        .arg("-Vv")
        .output()
        .expect("compiler version");
    assert!(output.status.success());
    let version = String::from_utf8(output.stdout).expect("UTF-8 compiler version");
    println!(
        "cargo:rustc-env=VFERROR_COMPILED_RUSTC={}",
        version.lines().collect::<Vec<_>>().join(" | ")
    );
}
