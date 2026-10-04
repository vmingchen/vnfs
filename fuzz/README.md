# vNFS fuzz targets

The targets feed malformed server-controlled data into the same COMPOUND
response validator and GETATTR decoder used by `vfsi-nfs`, and into the ONC
RPC / XDR decoders exposed by `libntirpc-sys`:

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz run nfs_compound_response
cargo +nightly fuzz run nfs_attribute_list
cargo +nightly fuzz run libntirpc_rpc_xdr
```

`libntirpc_rpc_xdr` drives `libntirpc`'s `xdr_ncallmsg`, `xdr_nreplymsg`, and
`xdr_wrapstring` decoders in the packaged ntirpc source built by `libntirpc-sys`.
Normal native builds produce a static Release archive and do not automatically
inherit cargo-fuzz's C sanitizer-coverage instrumentation. The Rust boundary
is instrumented; a deeper native campaign needs an explicitly instrumented
ntirpc build and a corpus seeded with well-formed messages.

CI runs a bounded smoke campaign. Longer local campaigns can add
`-- -max_total_time=600` (or another libFuzzer option). Corpus and crash
artifacts are deliberately ignored; preserve a reproducer as a normal unit
test before fixing it.
