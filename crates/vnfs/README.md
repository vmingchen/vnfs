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
vnfs = { version = "0.0.11", default-features = false }
```

## Example

Write two independent files, then read them back, using one vector call for
each phase:

```rust,no_run
use vnfs::{NfsVecFs, ReadOp, VecFs, VfOffset, WriteOp};

fn main() -> vnfs::VfResult<()> {
    let mut fs = NfsVecFs::connect("nfs.example.com")?;

    fs.writev(&[
        WriteOp::from_path("/file-1", VfOffset::At(0), b"hello".to_vec())
            .with_creation()
            .with_truncate(),
        WriteOp::from_path("/file-2", VfOffset::At(0), b"world".to_vec())
            .with_creation()
            .with_truncate(),
    ])?;

    let files = fs.readv(&[
        ReadOp::from_path("/file-1", VfOffset::At(0), 5),
        ReadOp::from_path("/file-2", VfOffset::At(0), 5),
    ])?;
    assert_eq!(files[0].data, b"hello");
    assert_eq!(files[1].data, b"world");
    Ok(())
}
```

For these two small files, `writev` puts both create/write chains into one
NFSv4 COMPOUND and therefore one network round trip. The known-length `readv`
does the same for both lookup/open/read chains. A scalar POSIX-style loop hides
this opportunity and pays latency for each file operation. Larger vectors are
packed into as few compounds as the server's negotiated operation, request,
and response-size limits allow; oversized vectors are split automatically.

The same model applies to `openv`, `writev`, `getattrsv`, `listdirv`,
`renamev`, `removev`, and the other vector methods. This is especially useful
for metadata-heavy workloads and for many small, independent I/O operations,
where network latency dominates transfer time.

## Small-file benchmark

The repository includes a [Rust benchmark driver][benchmark] that compares
`NfsVecFs::writev` and `NfsVecFs::readv` with scalar `std::fs::write` and
`std::fs::read` calls through a Linux kernel NFS mount. Both paths reach the
same NFS-Ganesha 15.3 NFSv4.2 export. Linux `netem` added 500 microseconds to
each loopback traversal, producing approximately 1 ms of added network RTT.

For 20 independent 4 KiB files, the following are medians of 30 trials. Client
connection setup is excluded, client order alternates each trial, and cold
trials use fresh paths:

| Cold paths | `vnfs` vector API | kernel NFS + `std::fs` | Speedup | `vnfs` RPCs |
| --- | ---: | ---: | ---: | ---: |
| Write 20 files | 38.14 ms | 144.39 ms | 3.79x | 1 |
| Read 20 files | 2.41 ms | 80.65 ms | 33.49x | 1 |

Repeating the benchmark over the exact same files after an untimed warm-up
produced:

| Warm paths | `vnfs` vector API | kernel NFS + `std::fs` | Speedup | `vnfs` RPCs |
| --- | ---: | ---: | ---: | ---: |
| Write 20 files | 40.86 ms | 156.46 ms | 3.83x | 1 |
| Read 20 files | 1.88 ms | 55.12 ms | 29.28x | 1 |

The warm read improves for both clients, but it does not erase the per-file
open, validation, and state-management cost of scalar kernel-NFS access.
`vnfs` does not retain file data across these calls: its advantage here comes
from expressing all 20 independent operations together and carrying them in
one COMPOUND RPC. Results will vary with server limits, workload, and network.

Run the same measurement against an NFS export mounted at `/mnt/nfs`:

```console
cargo run --release -p vnfs --example small_files_benchmark -- \
  --host 127.0.0.1 --direct-root /export --mount-root /mnt/nfs \
  --files 20 --bytes 4096 --rounds 30

# Reuse and prewarm the same paths.
cargo run --release -p vnfs --example small_files_benchmark -- \
  --host 127.0.0.1 --direct-root /export --mount-root /mnt/nfs \
  --files 20 --bytes 4096 --rounds 30 --reuse-paths
```

## License

Licensed under either of

- Apache License, Version 2.0
- MIT license

at your option.

[fast]: https://www.usenix.org/conference/fast17/technical-sessions/presentation/chen
[rfcv4_1]: https://datatracker.ietf.org/doc/html/rfc5661
[libntirpc]: https://github.com/nfs-ganesha/ntirpc
[benchmark]: https://github.com/vmingchen/vnfs/blob/main/crates/vnfs/examples/small_files_benchmark.rs
[`VecFs`]: https://docs.rs/vnfs/latest/vnfs/trait.VecFs.html
[`NfsVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.NfsVecFs.html
[`DummyVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.DummyVecFs.html
