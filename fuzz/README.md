# vNFS fuzz targets

The targets feed malformed server-controlled data into the same COMPOUND
response validator and GETATTR decoder used by `vfsi-nfs`:

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz run nfs_compound_response
cargo +nightly fuzz run nfs_attribute_list
```

CI runs a bounded smoke campaign. Longer local campaigns can add
`-- -max_total_time=600` (or another libFuzzer option). Corpus and crash
artifacts are deliberately ignored; preserve a reproducer as a normal unit
test before fixing it.
