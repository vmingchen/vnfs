# vnfs

A vectorized NFSv4.1/4.2 client library written in Rust.

> **Production status:** `vnfs` is beta, synchronous, and currently supports
> Linux with NFSv4.1/4.2 over TCP, AUTH_SYS by default, and optional Kerberos
> RPCSEC_GSS authentication. It is a good fit
> for controlled environments where vectorized small-file performance matters;
> assess the authentication, server-compatibility, and recovery constraints
> below before adopting it for production.

AUTH_SYS carries the calling process's numeric UID/GID without cryptographic
peer identity, integrity, or privacy. Use it only on a trusted network with
server export policy that treats those credentials appropriately. For
untrusted networks, enable the opt-in `rpcsec-gss` feature described below.
Production builders can also set `require_secure_authentication(true)` to
fail closed instead of accidentally connecting with AUTH_SYS.

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

NFS and NFSv4.2 server-side COPY are enabled by default. The dummy backend is
an opt-in test/development feature, and RPCSEC_GSS is intentionally not
enabled by default.
Applications that only need interface types can disable default features:

```toml
vnfs = { version = "0.0.11", default-features = false }
```

## Secure authentication (optional)

Enable Kerberos-backed RPCSEC_GSS explicitly:

```toml
vnfs = { version = "0.0.11", features = ["rpcsec-gss"] }
```

The client uses the process's default GSS credential cache (normally populated
with `kinit`) and does not accept or retain passwords. Integrity protection is
the recommended baseline.

```rust,no_run
use vnfs::{NfsAuthentication, NfsConnectOptions, NfsVecFs, RpcsecGssProtection};

fn main() -> vnfs::VfResult<()> {
    let fs = NfsVecFs::connect_with_options(
        "nfs.example.com",
        NfsConnectOptions {
            authentication: NfsAuthentication::RpcsecGss {
                // None derives the GSS host-based name nfs@nfs.example.com.
                service_principal: None,
                protection: RpcsecGssProtection::Integrity,
            },
            ..NfsConnectOptions::default()
        },
    )?;
    drop(fs);
    Ok(())
}
```

`Authentication` and `Integrity` correspond to server export security flavors
`krb5` and `krb5i`. The server must enable the
matching flavor and possess a service key for the selected principal. An
explicit host-based service name can be supplied when DNS canonicalization or
the export's service identity differs from `nfs@<host>`. Automatic reconnects
reuse the same authentication configuration and obtain fresh credentials from
the current process cache. RPCSEC_GSS privacy (`krb5p`) is not exposed yet
because the supported libntirpc 6.x client cannot reliably encode privacy
payloads; the API does not silently downgrade it to a weaker mode.

## Rust-native example

Write two independent files, then read them back, using one vector call for
each phase:

```rust,no_run
use vnfs::prelude::*;

fn main() -> std::io::Result<()> {
    let backend = NfsVecFs::builder("nfs.example.com")
        .root("/export/application")
        .connect()
        .map_err(std::io::Error::from)?;
    let client = FsClient::new(backend);
    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let files = client.open_many(&[
        OpenRequest::new("/file-1", flags),
        OpenRequest::new("/file-2", flags),
    ])?;

    client.write_many(&[
        files[0].write_at(0, b"hello"),
        files[1].write_at(0, b"world"),
    ])?;

    let contents = client.read_many(&[
        files[0].read_at(0, 5),
        files[1].read_at(0, 5),
    ])?;
    assert_eq!(contents[0].data, b"hello");
    assert_eq!(contents[1].data, b"world");
    Ok(())
}
```

For these two small files, `open_many`, `write_many`, and `read_many` each put
both independent operations into one NFSv4 COMPOUND and therefore one network
round trip per phase. A scalar POSIX-style loop hides this opportunity and pays
latency for each file operation. Larger vectors are packed into as few
compounds as the server's negotiated operation, request, and response-size
limits allow; oversized vectors are split automatically.

The same model applies to `openv`, `writev`, `getattrsv`, `listdirv`,
`renamev`, `removev`, and the other vector methods. This is especially useful
for metadata-heavy workloads and for many small, independent I/O operations,
where network latency dominates transfer time.

## Idiomatic scalar I/O

`FsClient` is cheaply cloneable and its owned `FsFile` handles can coexist or
move to worker threads. A handle implements `Read`, `Write`, and `Seek` and
closes its remote descriptor on drop. Call `close()` explicitly when a close
error must be observed, and `sync_data()`/`sync_all()` when durability errors
must be observed before close.

```rust,no_run
use std::io::{Read, Seek, SeekFrom};
use vnfs::{FsClient, NfsVecFs, OpenFlags, OpenRequest};

fn main() -> std::io::Result<()> {
    let fs = NfsVecFs::builder("nfs.example.com")
        .connect()
        .map_err(std::io::Error::from)?;
    let client = FsClient::new(fs);
    let mut file = client.open(OpenRequest::new("/file-1", OpenFlags::READ))?;
    file.seek(SeekFrom::Start(0))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    file.close()?;
    Ok(())
}
```

One backend connection serializes access to its stateful NFS session, while
`FsClient::read_many` and `write_many` preserve useful compound batching.
Create a bounded pool of clients when parallel network requests are required;
use one vector cohort per worker. Async applications should run these
synchronous workers with their runtime's blocking-task API.

## Failure and recovery semantics

An NFS COMPOUND is ordered but **not transactional**. If operation `i` fails,
the server stops processing that compound: the prefix before `i` may already
have succeeded and the suffix was not executed. The outcome-aware vector
methods return `BatchOutcome<T>` with `Success`, `Completed`, `Failed`,
`NotAttempted`, and `Indeterminate` states. `Completed` means a compatibility
decoder proved success but did not retain the returned value. Reconcile an
`Indeterminate` mutation before retrying it. The fail-fast methods remain in
`VecFs` for compatibility.

Low-level compound, RPC, and session construction is isolated under
`vnfs::legacy`; it is not part of the recommended application API.

Lost replies to create, write, rename, copy, remove, and other mutations are
reported as ambiguous and are never replayed automatically; replay could
duplicate an append or repeat another side effect. Side-effect-free reads and
metadata queries reconnect after transport, expired-client, stale-state, or
dead-session failures, reopen all live path-backed descriptors in one vector,
preserve their descriptor numbers and offsets, and retry once. Reconnect
attempts use a bounded exponential backoff configurable with
`NfsRecoveryPolicy`; automatic recovery can be disabled with
`set_auto_reconnect(false)`.

Recovery cannot reopen an unlinked or renamed file by its old path, and it
cannot restore a descriptor whose permissions or identity changed while the
server was unavailable. Streaming callback APIs are not replayed because a
callback may already have observed a prefix. Treat an error from those APIs as
partial progress and restart at an application-defined checkpoint.

## Platform and build requirements

The supported native target is Linux. The minimum supported Rust version is
1.88. Normal builds use the system `libntirpc` (4.3 or newer) and do not clone
or download native source from `build.rs`. On Ubuntu 24.04 or newer, install:

```console
sudo apt-get install clang libclang-dev pkg-config libntirpc-dev \
  libkrb5-dev libgssglue-dev liburcu-dev
```

After Cargo dependencies have been fetched, the native build can run without
network access. docs.rs uses checked-in FFI declarations and does not require
the native development packages. NFS servers must expose an NFSv4 pseudo-root
reachable by the supplied host name. Kerberos RPCSEC_GSS requires the opt-in
Cargo feature, a valid default credential cache, and matching server
configuration. There is currently no RPC-over-TLS, callback/delegation, or
asynchronous API.

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
[benchmark]: https://github.com/vmingchen/vnfs/blob/main/crates/vnfs/examples/small_files_benchmark.rs
[`VecFs`]: https://docs.rs/vnfs/latest/vnfs/trait.VecFs.html
[`NfsVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.NfsVecFs.html
[`DummyVecFs`]: https://docs.rs/vnfs/latest/vnfs/struct.DummyVecFs.html
