fn main() {
    if std::env::var_os("CARGO_FEATURE_FFI").is_some() && std::env::var_os("DOCS_RS").is_none() {
        assert_eq!(
            std::env::var("DEP_NTIRPC_NATIVE_REPLY_VERIFIER").as_deref(),
            Ok("1"),
            "vfsi-nfs requires source-built libntirpc-sys with native reply verification"
        );
    }
}
