fn main() {
    println!("cargo:rustc-check-cfg=cfg(libntirpc_legacy_free_cb)");
    if std::env::var_os("DEP_NTIRPC_LEGACY_FREE_CB").as_deref() == Some(std::ffi::OsStr::new("1")) {
        println!("cargo:rustc-cfg=libntirpc_legacy_free_cb");
    }
}
