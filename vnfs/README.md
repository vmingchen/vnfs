# vnfs

A vectorized [NFSv4.1][rfcv4_1] client library written in Rust.

NFSv4 supports *compounds* that coalesce many small file operations into one
round trip, which can dramatically cut network latency for metadata-heavy
workloads. The POSIX file API cannot use this, so `vnfs` exposes a
vectorized `VecFs` API (the idea from the FAST'17 paper
[vNFS: Maximizing NFS Performance with Compounds and Vectorized I/O][fast])
as a Rust crate.

## Backends

The crate is backend-agnostic through the [`VecFs`] trait:

- [`NfsVecFs`] — an NFSv4.1 implementation on top of [libntirpc], batching
  many operations (lookups, readdirs, getattrs, opens, reads, ...) into few
  large compounds.
- [`DummyVecFs`] — a `std::fs`-backed implementation so the same API also
  works on non-NFS filesystems (and is handy for tests).

## Example

```rust,no_run
use vnfs::{VecFs, AttrMask};

fn main() -> vnfs::VfResult<()> {
    let mut fs = vnfs::nfs::NfsVecFs::connect("127.0.0.1")?;

    // Attributes to fetch for each entry (all supported fields).
    let masks = AttrMask::all();

    // List a directory's children in one batched round trip.
    let entries = fs.listdir(std::path::Path::new("/export"), masks, 0, false)?;
    for e in entries {
        let path = e.file.path().expect("path-backed entry");
        println!("{} (ftype={})", path.display(), e.ftype);
    }
    Ok(())
}
```

## License

Licensed under either of

- Apache License, Version 2.0
- MIT license

at your option.

[fast]: https://www.usenix.org/conference/fast17/technical-sessions/presentation/chen
[rfcv4_1]: https://datatracker.ietf.org/doc/html/rfc5661
[libntirpc]: https://github.com/nfs-ganesha/ntirpc
[`VecFs`]: https://docs.rs/vnfs/latest/vnfs/trait.VecFs.html
[`NfsVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.NfsVecFs.html
[`DummyVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.DummyVecFs.html
