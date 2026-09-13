# vnfs

A vectorized [NFSv4.1][rfcv4_1] client library written in Rust.

NFSv4 supports *COMPOUND* requests: one RPC can carry an ordered sequence of
file operations. A conventional POSIX-style loop hides that capability behind
one-file-at-a-time calls, so latency grows with the number of files. `vnfs`
instead exposes a vectorized `VecFs` API (the idea from the FAST'17 paper
[vNFS: Maximizing NFS Performance with Compounds and Vectorized I/O][fast])
as a Rust crate. The NFS backend packs each vector into compounds up to the
server's negotiated operation and message-size limits, then returns results in
input order.

## Package boundary

The `vnfs` crate is the NFS-focused Rust compatibility package in the wider
VFSI project. It provides [`NfsVecFs`], the shared [`VecFs`] interfaces and
types, and [`DummyVecFs`] for local testing. New protocol backends are
published as separate `vfsi-*` crates so each backend has an independent
dependency and release boundary.

NFS, the dummy backend, and NFSv4.2 server-side COPY are enabled by default.
Applications that only need interface types can disable default features:

```toml
vnfs = { version = "0.0.10", default-features = false }
```

## Example

Read the first 4 KiB of 32 independent files as one vectorized operation:

```rust,no_run
use vnfs::{NfsVecFs, ReadOp, VecFs, VfOffset};

fn main() -> vnfs::VfResult<()> {
    let mut fs = NfsVecFs::connect("nfs.example.com")?;
    let paths: Vec<String> = (0..32)
        .map(|shard| format!("/dataset/shard-{shard:02}.json"))
        .collect();
    let reads: Vec<ReadOp> = paths
        .iter()
        .map(|path| ReadOp::from_path(path, VfOffset::At(0), 4096))
        .collect();

    // One API call gives the backend the whole batch. NfsVecFs emits as few
    // NFSv4 COMPOUND RPCs as the negotiated server limits allow.
    let results = fs.readv(&reads)?;

    for (path, result) in paths.iter().zip(results) {
        println!("{path}: {} bytes (eof={})", result.data.len(), result.eof);
    }
    Ok(())
}
```

With scalar calls, reading 32 paths requires a succession of path lookup,
open, read, and close exchanges for each file. `readv` exposes all 32 reads at
once, allowing `NfsVecFs` to place many independent operation chains into each
NFSv4 COMPOUND. The number of latency-bearing RPC round trips therefore scales
with the number of compound chunks instead of directly with the number of
files. The exact packing depends on the server's `ca_maxoperations`, request
size, and response-size limits; oversized vectors are split automatically.

The same model applies to `openv`, `writev`, `getattrsv`, `listdirv`,
`renamev`, `removev`, and the other vector methods. This is especially useful
for metadata-heavy workloads and for many small, independent I/O operations,
where network latency dominates transfer time.

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
