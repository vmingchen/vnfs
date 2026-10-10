# vnfs

[![crates.io](https://img.shields.io/crates/v/vnfs.svg)](https://crates.io/crates/vnfs)
[![docs.rs](https://docs.rs/vnfs/badge.svg)](https://docs.rs/vnfs)
[![CI](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml/badge.svg)](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Many NFS files. Fewer network round trips.**

`vnfs` is a synchronous Rust client for NFSv4.1/4.2. Submit independent file
operations together, and the backend packs them into NFS COMPOUND requests
within the server's negotiated operation and byte limits. It is especially
useful for small files and directory metadata, where latency dominates.
Connect directly to a server—no kernel mount required.

## Try it

```toml
[dependencies]
vnfs = "0.0.20"
```

On Ubuntu 24.04 or newer, install the Linux native build prerequisites:

```console
sudo apt-get install build-essential cmake clang libclang-dev pkg-config liburcu-dev
```

Write two files, then read them together. Their parent directory must exist;
`write_files` replaces existing contents.

```rust,no_run
use vnfs::prelude::*;

fn main() -> vnfs::Result<()> {
    let fs = Nfs::builder("nfs.example.com")
        .root("/export/application")
        .connect()?;

    fs.write_files(&[
        ("/file-1", b"hello".as_slice()),
        ("/file-2", b"world".as_slice()),
    ])?;
    let results = fs.vread([
        ReadOp::whole("/file-1"),
        ReadOp::whole("/file-2"),
    ], Default::default())?;
    assert_eq!(results[0].data(), Some(b"hello".as_slice()));
    assert_eq!(results[1].data(), Some(b"world".as_slice()));
    Ok(())
}
```

Both reads can share **one COMPOUND round trip** when negotiated limits permit,
without remote OPEN/CLOSE phases. Writes use batched OPEN, complete WRITE, and
CLOSE phases—not one RPC. Larger batches split automatically; successful
results retain input order. Paths are relative to the configured remote root,
which is not a security sandbox.

## Small-file benchmark

For 20 independent 4 KiB files on NFS-Ganesha 15.3 (NFSv4.2), with approximately
1 ms added RTT using loopback `netem`, medians of 30 trials were:

| Workload | Vector backend | Kernel NFS + `std::fs` | Speedup |
| --- | ---: | ---: | ---: |
| Cold write | 38.14 ms | 144.39 ms | 3.79× |
| Cold read | 2.41 ms | 80.65 ms | 33.49× |
| Warm write | 40.86 ms | 156.46 ms | 3.83× |
| Warm read | 1.88 ms | 55.12 ms | 29.28× |

These measurements use the **native backend**, not the `write_files` helper
above: each vector completed in one COMPOUND. Connection setup is excluded;
client order alternates, cold trials use fresh paths, and warm trials reuse
files after an untimed warm-up. vNFS retains no file data between these calls.
Results depend on server limits, cache policy, network, and workload—not a
universal speedup. See the [benchmark driver][benchmark] to reproduce them.

## Choose your workflow

- **Small files:** `vread` and `write_files` batch independent work.
- **Repeated I/O:** `vopen`, `ReadOp::range`/`ReadOp::into`, and `vwrite` use owned
  handles and optional caller-provided buffers.
- **Directories:** `vlistdirs` pages listings with selected `Attributes`, avoiding
  a separate stat per entry. `ListDirOptions::recursive(true)` visits trees.
- **Large files:** `vstream` bounds memory; read pools hide RTT with independent
  sessions. See [streaming and resource limits][operations].

Import `vnfs::prelude::*`: `Vfsi` supplies vector operations, and `VfsiExt`
adds scalar conveniences without replacing the vector engine. Start with the
[compiled examples][examples] or the [API overview][api].

Already mounted on Linux? `Nfs::from_mount("/mnt/data")?` discovers a supported
NFSv4 TCP AUTH_SYS connection. `Posix` always uses the kernel; opt-in `Auto`
can route suitable mounts directly. **Direct clients do not share or invalidate
kernel caches.** See [mount discovery and routing][operations] before mixing them.

## Before production

The API is beta and may change before 1.0. Linux and Rust 1.88+ are supported.
Native builds compile pinned, packaged ntirpc source; no system libntirpc or
build-time source download is required. NFS and server-side COPY are enabled
by default; SMB has a separate `vfsi-smb` package.

Vector operations are **not transactions**: errors can follow completed work.
Owned reads default to a shared 16 MiB data budget; stream larger files. AUTH_SYS
is the default for trusted networks. Async applications should use bounded
blocking workers; there is no async API, RPC-over-TLS, or delegation support.

- [Standard Rust I/O and handle lifecycle][scalar]
- [Failure, partial progress, and recovery][failure]
- [Optional Kerberos authentication][auth]
- [Streaming, limits, and mount routing][operations]

The vector API builds on the FAST'17 paper
[vNFS: Maximizing NFS Performance with Compounds and Vectorized I/O][fast].

## License

MIT OR Apache-2.0, at your option.

[api]: https://docs.rs/vnfs/latest/vnfs/
[examples]: https://docs.rs/vnfs/latest/vnfs/examples/
[scalar]: https://docs.rs/vnfs/latest/vnfs/guides/standard_io/
[failure]: https://docs.rs/vnfs/latest/vnfs/guides/failure_recovery/
[auth]: https://docs.rs/vnfs/latest/vnfs/guides/authentication/
[operations]: https://docs.rs/vnfs/latest/vnfs/guides/operations/
[benchmark]: https://github.com/vmingchen/vnfs/blob/main/crates/vnfs/examples/small_files_benchmark.rs
[fast]: https://www.usenix.org/conference/fast17/technical-sessions/presentation/chen
