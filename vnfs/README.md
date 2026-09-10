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

## Backends

The crate is backend-agnostic through the [`VecFs`] trait:

- [`NfsVecFs`] — an NFSv4.1 implementation on top of [libntirpc], batching
  many operations (lookups, readdirs, getattrs, opens, reads, ...) into few
  large compounds.
- [`DummyVecFs`] — a `std::fs`-backed implementation so the same API also
  works on non-NFS filesystems (and is handy for tests).

All backends are enabled by default. The SMB backend uses the async [`smb2`]
client internally, so its synchronous `VecFs` facade owns a [Tokio] runtime.
Tokio is optional and is not part of an NFS-only build:

```toml
vnfs = { version = "0.0.7", default-features = false, features = ["nfs", "server-copy"] }
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
[`smb2`]: https://crates.io/crates/smb2
[Tokio]: https://tokio.rs/
[`VecFs`]: https://docs.rs/vnfs/latest/vnfs/trait.VecFs.html
[`NfsVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.NfsVecFs.html
[`DummyVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.DummyVecFs.html
