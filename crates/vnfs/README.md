# vnfs

[![crates.io](https://img.shields.io/crates/v/vnfs.svg)](https://crates.io/crates/vnfs)
[![docs.rs](https://docs.rs/vnfs/badge.svg)](https://docs.rs/vnfs)
[![CI](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml/badge.svg)](https://github.com/vmingchen/vnfs/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Turn many independent NFS operations into a few network round trips.**

`vnfs` is a native Rust client for NFSv4.1 and NFSv4.2. Its vectorized API
opens, reads, writes, stats, renames, and removes many files together, allowing
the NFS backend to encode the work as protocol-native COMPOUND requests. This
is especially effective for small-file and metadata-heavy workloads where
network latency costs more than transferring the data itself.

NFSv4 supports *COMPOUND* requests: one RPC can carry an ordered sequence of
file operations. A conventional POSIX-style loop hides that capability behind
one-file-at-a-time calls, so latency grows with the number of files. `vnfs`
instead exposes vectorized file operations (the idea from the FAST'17 paper
[vNFS: Maximizing NFS Performance with Compounds and Vectorized I/O][fast])
as a Rust crate. The NFS backend packs each vector into compounds up to the
server's negotiated operation and message-size limits, then returns results in
input order.

## Try it

`vnfs` runs on Linux and connects directly to an NFSv4 server—no kernel mount
is required. Add the crate:

```toml
[dependencies]
vnfs = "0.0.19"
```

On Ubuntu 24.04 or newer, install the native build dependencies once:

```console
sudo apt-get install build-essential cmake clang libclang-dev pkg-config liburcu-dev
# Optional rpcsec-gss feature:
sudo apt-get install libkrb5-dev
```

## Rust-native example

For complete, compiled workflows, see the
[canonical examples](examples/README.md) and the
[task-oriented API documentation](https://docs.rs/vnfs/latest/vnfs/).
Start with `vread`/`write_files` for small files, `vopen`/`vread` with `ReadOp::into` for
repeated positional I/O, or `vstream` for bounded large-file reads.
The portable traits are owned by `vfsi-core` and re-exported by `vnfs`.
`Vfsi` contains vectorized operations; `VfsiExt` adds scalar operations and
convenience workflows, preserving their native backend execution.

Single-target `VfsiExt` helpers use conventional names such as `open`, `write`,
`metadata`, and `read_dir`. Prefer vector operations for independent
work on multiple files or directories: `vopen`, `vread`, `write_files`, and
`vlistdirs` let the backend batch requests.
`vgetattrs` batches metadata with selected fields and explicit
final-symlink behavior through `AttrsOptions`.

The API is grouped into `nfs`, `files`, `directory`, `error`, and `helpers`;
common application types are also available at the crate root.

Write two independent files, then read them back. The convenience methods
batch each phase across both files:

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

`write_files` performs vector OPEN, WRITE, and CLOSE phases. `vread`
reads directly by path, so small files can share one READ COMPOUND without
remote OPEN/CLOSE phases when the server's negotiated limits permit.
`write_files` retries short
writes through `vwrite`; it is not transactional, so a failed call may have
modified a prefix of files. `vread` limits the combined returned data to
16 MiB by default; use `ReadOptions::max_total_bytes` to adjust the limit, or stream
large files. A scalar POSIX-style loop pays latency for each file operation.
Larger vectors are packed into as few
compounds as the server's negotiated operation, request, and response-size
limits allow; oversized vectors are split automatically.

Already have the directory mounted on Linux? Discover its connection:

```rust,no_run
use vnfs::Vfsi;
let fs = vnfs::Nfs::from_mount("/mnt/data/git/some/tree")?;
let files = fs.vread([
    vnfs::ReadOp::whole("/file-1"),
    vnfs::ReadOp::whole("/file-2"),
], Default::default())?;
# Ok::<(), vnfs::Error>(())
```

The mount path must be absolute; the selected directory becomes the remote root.
Discovery supports ordinary
NFSv4.1/4.2 TCP AUTH_SYS mounts, verifies the remote directory identity, and
preserves read-only restrictions. Unsupported security and ambiguous mount
mappings fail explicitly. `NfsBuilder::from_mount(path)?` allows timeout and
other tuning before connecting. The configuration remains pinned to the
discovered mount; every operation uses the direct NFS client with its own
caches and state. Mount discovery requires no per-file probing during I/O.

The same model applies to `vopen`, `vread`, `vwrite`, and high-level
`vlistdirs`, `vcopy`, and `vremove`. For tools such as
`ls`, `du`, and `find`, `Attributes` chooses which attributes a directory
listing fetches, and each `DirectoryListing` includes metadata for its entries
without a separate stat call per file. For example:

```rust,no_run
use vnfs::{ControlFlow, Vfsi, Attributes, Nfs, ListDirOptions};

fn main() -> vnfs::Result<()> {
    let fs = Nfs::connect("nfs.example.com")?;
    let directories = ["/export/a", "/export/b"];
    fs.vlistdirs(&directories,
        ListDirOptions::new().fields(Attributes::MODE | Attributes::SIZE | Attributes::BLOCKS),
        |index, page| {
            println!("{}: {} entries in this page (input {index})", page.path.display(), page.entries.len());
            Ok(ControlFlow::Continue(()))
        })?;
    Ok(())
}
```

The NFS backend batches directory lookups and READDIR pages into compounds;
large listings continue page by page. The aggregate entry and path-byte
limits bound traversal work. Pages are delivered incrementally without retaining
an entire listing. Set `ListDirOptions::recursive(true)` to visit a tree.
For collected results, `VfsiExt::read_dirs_with_options` builds on `vlistdirs`;
`VfsiExt::read_dir_with_options` handles one directory.

Owned `NfsClient::vread` results inherit the client's 16 MiB aggregate budget
when called with `ReadOptions::default()` (or `Default::default()`).
Use `ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(bytes))` to override it,
or `vread` with `ReadOp::into` to supply your own buffers.
Range lengths are checked before dispatch; whole
files are collected within the remaining budget and fail rather than truncate.

## Use existing Linux mounts

If paths already live under Linux mounts, `Auto` accepts ordinary mounted
paths. For an unambiguous read-write NFSv4.1/4.2 mount with `sec=sys`, it
discovers the server/export, verifies the remote root, and reuses a direct
NFS connection so adjacent operations on that mount can form COMPOUNDs.
Local paths, SMB/CIFS mounts, Kerberos mounts, and mounts it cannot safely
identify continue through the kernel. The chosen route is observable on each
opened file:

`Auto` opts into a **separate NFS client**, not a transparent kernel-cache
acceleration. Direct reads/writes do not share or invalidate kernel caches.
Use `Mounted` when the same objects are also accessed through the kernel and
you need its cache/coherency semantics, including access through aliases.

```rust,no_run
# #[cfg(all(feature = "auto", target_os = "linux"))]
# fn main() -> vnfs::Result<()> {
use vnfs::{Auto, Vfsi, OpenFlags, OpenOp};

    let fs = Auto::new("/")?;
    let paths = ["/mnt/nfs/file-1", "/mnt/nfs/file-2"];
    let requests = paths.map(|p| OpenOp::new(p, OpenFlags::READ));
    let mut files = fs.vopen(&requests)?;
    println!("route: {:?}", files[0].route());
    let contents = fs.vread([
        vnfs::ReadOp::range(&files[0], 0, 5),
        vnfs::ReadOp::range(&files[1], 0, 5),
    ], Default::default())?;
    assert_eq!(contents.len(), 2);
    fs.vclose(&mut files)?;
    Ok(())
}
# #[cfg(not(all(feature = "auto", target_os = "linux")))]
# fn main() {}
```

Use `Mounted::new("/")?` to **always** use the kernel client, or
`Nfs::builder(server).root(export).connect()?` for a fully explicit direct
connection. `Auto` selects a backend before dispatch; it never replays a
possibly completed write through a different backend. Open handles remain
pinned to their selected backend. `Auto::with_limits(ResourceLimits::new().max_read_bytes(bytes))`
configures the same client resource policy as `NfsBuilder::limits`;
file `Read` calls
are chunked to 1 MiB.

Mixing direct and kernel I/O can expose stale reads or delayed kernel writes;
even selecting a different pathname can reach the same underlying object.
Open-handle route pinning does not coordinate those caches. `Auto` captures
AUTH_SYS credentials per direct connection and rejects use of a direct file
after that thread's filesystem identity changes. Prefer `Mounted` where exact
kernel mount semantics or warm page-cache hits are more important than
cross-file vectorization.

`Auto::route_for(path)` reports a candidate route, not a promise about every
operation on that path. For example, `vopen` leaves final symlinks and
ambiguous create-if-missing paths on the kernel route; `CREATE_NEW` requests
can take the direct path without following a pre-existing symlink. The
`AutoFile::route()` value is the definitive choice for an open handle.

## Small-file benchmark

The repository includes a [Rust benchmark driver][benchmark] that compares
the native `VectorBackend::vwrite_impl` and `VectorBackend::vread_impl` operations on
`NfsVecFs` with scalar `std::fs::write` and
`std::fs::read` calls through a Linux kernel NFS mount. Both paths reach the
same NFS-Ganesha 15.3 NFSv4.2 export. Linux `netem` added 500 microseconds to
each loopback traversal, producing approximately 1 ms of added network RTT.

For 20 independent 4 KiB files, the following are medians of 30 trials. Vfsi
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

## Is vnfs a fit?

`vnfs` is a strong fit when a Linux service touches many independent NFS files
and can express that work in batches. It also provides explicit adapters for
familiar scalar `Read`, `Write`, and `Seek` support when only part of a data path benefits from
vectorization.

The crate is currently beta and synchronous. It uses NFSv4.1/4.2 over TCP,
defaults to AUTH_SYS on trusted networks, and optionally supports Kerberos
RPCSEC_GSS authentication. Review the failure, authentication, and platform
notes below before production deployment. Async applications should call it
from bounded blocking workers.

## Idiomatic scalar I/O

`NfsClient` is cheaply cloneable and its owned `NfsFile` handles can coexist or
move to worker threads. Handles expose lifecycle operations; file I/O and
metadata go through `Vfsi`. Call `close()` explicitly to observe cleanup errors,
and `fs.sync_data(&file)` or `fs.sync_all(&file)` to observe durability errors.

Use an explicit `fs.std_io(&file)` adapter for generic `std::io` code. It borrows
the client and handle, maintains its own cursor starting at zero, and delegates
reads, writes, metadata and synchronization to the vector engine. Collecting
reads enforce the client's payload budget. The adapter's concrete type is private;
use `Seek::stream_position` to query its cursor. All clients share the
`VfsiExt::open_options` builder, whose `open` and `vopen` methods use `Vfsi::vopen`.

```rust,no_run
use std::io::Read;
use vnfs::prelude::*;

fn main() -> std::io::Result<()> {
    let fs = Nfs::connect("nfs.example.com")?;
    let file = fs.open("/file-1")?;
    let mut contents = Vec::new();
    fs.std_io(&file).read_to_end(&mut contents)?;
    file.close()?;
    Ok(())
}
```

Vector operations preserve structured errors. Standard-I/O adapters convert
these to `std::io::Error` for interoperability. Sparse metadata getters such as
`Attrs::len()` and `permissions()` return `None` when the backend did not return
the corresponding field; missing values never imply zero size or mode zero.

`VfsiExt::listdir(root, ListDirOptions::new(), callback)` invokes a callback
with one `WalkEventKind::Entry` per child without retaining the full listing
on paging backends. Use `ListDirOptions` to adjust the default
entry and path-byte limits, including `ListDirOptions::unlimited()` for very
large directories. The client fetches bounded READDIR pages and releases its
backend lock before invoking the callback, so the callback may use the same
client or drop another of its files. Concurrent directory mutation can change
the listing or invalidate its continuation cookie; iteration is not a snapshot.

## Large-file streaming and tuning

For a large file, stream bounded chunks instead of collecting the complete
file in a `Vec`. The default chunk size is 1 MiB; tune it for the server,
network RTT, and consumer. The backend still obeys negotiated NFS limits, so
the callback may receive smaller chunks. Unlike the directory visitor, the
file-stream callback runs without the backend lock and may call the same client.

```rust,no_run
use vnfs::prelude::*;

fn main() -> vnfs::Result<()> {
    let fs = Nfs::connect("nfs.example.com")?;
    let mut bytes_seen = 0u64;
    fs.vstream(
        &["/dataset/large.bin"],
        StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(4 * 1024 * 1024).unwrap()),
        |index, offset, chunk| {
            assert_eq!(index, 0);
            assert_eq!(offset, bytes_seen);
            // Consume/process this chunk here; do not retain it to keep memory bounded.
            bytes_seen += chunk.len() as u64;
            Ok(std::ops::ControlFlow::Continue(()))
        },
    )?;
    println!("read {bytes_seen} bytes");
    Ok(())
}
```

Benchmark a real export while sweeping chunk sizes; the driver counts bytes
without retaining them, reports median throughput and chunk count, and compares
single-session reads against a persistent pool of independent NFS sessions.
Pool setup is reported separately from read time. The pool bounds outstanding
chunks by `max_buffered_bytes`, delivers chunks in file order, and does not
provide snapshot consistency if another client modifies the file while it is
being read:

```console
cargo run --release -p vnfs --example large_file_read_benchmark --features nfs -- \
  --host 127.0.0.1 --root /export --path /large.bin \
  --chunk-sizes 65536,262144,1048576,4194304 --rounds 7 --warmups 2
```

Add `--minor-version 1` or `--minor-version 2` to pin an NFS version instead
of using the client's default negotiation.

Use the same file and server when comparing chunk sizes. For network-latency
experiments, add controlled RTT with `tc netem` on the client/server path and
record the applied delay; report cold and warm runs separately. The best size
depends on negotiated server limits, latency, throughput, and callback work.
This synchronous client serializes operations on a connection; use
`connect_read_pool` when the server and network can benefit from multiple
independent sessions. `NfsReadPoolOptions` defaults to four workers, 1 MiB
chunks, at most eight outstanding ranges, and a 16 MiB buffer budget. A pool
keeps its sessions alive across streams, but opens and closes a file on each
worker for every stream. Tune worker count and chunk size against measured
throughput: more sessions can increase server load and are not always faster.

On an isolated Linux test host, this helper applies 500 microseconds of
egress delay to loopback (about 1 ms added RTT), runs the sweep, and removes
the qdisc on exit. It refuses to replace an existing loopback qdisc and only
accepts loopback server addresses:

```console
sudo apt install iproute2
dd if=/dev/zero of=/srv/vnfs-ci/.vnfs-large-read-benchmark bs=1M count=32
./ci/benchmark-large-read-pipeline.sh 127.0.0.1 / /.vnfs-large-read-benchmark 500us 5 1
rm /srv/vnfs-ci/.vnfs-large-read-benchmark
```

```rust,no_run
use vnfs::prelude::*;

fn main() -> vnfs::Result<()> {
    let mut pool = Nfs::builder("nfs.example.com").root("/export")
        .connect_read_pool(
            vnfs::NfsReadPoolOptions::new()
                .worker_count(4)
                .chunk_size(1024 * 1024)
                .max_in_flight(8)
                .max_buffered_bytes(16 * 1024 * 1024),
        )?;
    pool.read_stream("/dataset/large.bin", |offset, chunk| {
        // Consume chunks in order; false cancels after this chunk.
        println!("received {} bytes at {offset}", chunk.len());
        Ok(std::ops::ControlFlow::Continue(()))
    })?;
    Ok(())
}
```

One backend connection serializes access to its stateful NFS session, while
`NfsClient::vread` and `vwrite` preserve useful compound batching.
Clone a client to share that same connection; use `Nfs::builder(host).connect_pool(4)?`
when separate vector cohorts need parallel network requests. `next_client()`
distributes cohorts round-robin across independent sessions; it does not split
one vector call. Async applications should run these synchronous clients with
their runtime's blocking-task API.

## Failure and recovery semantics

The application surface uses `vnfs::Result`, `Error`, `ErrorKind`, `FileType`,
and owned file handles. `ReadOp::whole`, `ReadOp::range`, and `ReadOp::into` construct
read operations without I/O; the consuming `vread` accepts arrays, vectors,
or iterators plus `ReadOptions`. Default options inherit the client's aggregate
budget; explicit options override it.
`WriteOp::at(&file, offset, data)` prepares a portable positional write without
copying the payload or importing backend crates. `vwrite` with
`WriteOptions::default()` reports short writes. Select
`WriteOptions::new().write_all(true)` to complete successful short writes without
replaying failed or ambiguous requests. Completion does not guarantee durability. Vector
results are in request order and carry data/count, resolved offset, and EOF or
durability information; they do not expose backend descriptors. `vread` with `ReadOp::into`
returns one `ReadResult` per operation with offset, byte count, EOF, and
optional owned data. `data` is `None` for caller buffers, whose borrows end
when the call returns—even on error.
For reusable vector-only helpers, use a `Vfsi` bound. Use `VfsiExt`
(or `vnfs::prelude::*`) for convenience methods such as `read_files`, `write_files`,
scalar opens, metadata wrappers, and default-option directory/streaming operations. `VfsiExt` is
blanket-implemented for every `Vfsi` and preserves batching and resource limits.
Custom backends implement only `Vfsi`; `VfsiExt` derives its methods from those vector primitives.
Use `FileHandle` for generic handle operations
instead of backend traits. The same generic code can use a direct `NfsClient`,
`Mounted`, or `Auto`. These traits delegate to the native vector implementations
without boxing, additional copies, or scalar-loop fallbacks.

```rust,no_run
use vnfs::VfsiExt;
fn read_inputs<C: vnfs::Vfsi>(fs: &C) -> vnfs::Result<Vec<Vec<u8>>> {
    Ok(fs.vread([vnfs::ReadOp::whole("/file-1"), vnfs::ReadOp::whole("/file-2")], Default::default())?.into_iter().map(|r| r.into_data().unwrap()).collect())
}
```

`vread` accepts whole-file paths, positional handle ranges, or a mix. Whole-file
paths use the optimized backend algorithm without a scalar OPEN/CLOSE loop.
Range requests keep their opened object across renames and preserve its cursor.
Complete files fail rather than truncate when the shared budget is exceeded. Configure client-wide
defaults with `NfsBuilder::limits(ResourceLimits)`
or `Auto::with_limits(ResourceLimits)`. The default maximum owned read payload
is 16 MiB, shared by scalar whole-file reads and aggregate vector reads. Explicit
per-call options override these defaults. Limits bound logical data/path bytes,
not allocator rounding, result metadata, RPC envelopes, or all process memory.
Read pools have their own explicit concurrency/buffer options.
Configure resource policies through builders such as
`ResourceLimits::new().max_read_bytes(1024).max_walk_depth(8)` and inspect
values through accessors. Zero read, entry, path-byte, and depth limits remain
valid. `stream_chunk_bytes` takes `NonZeroUsize` to ensure forward progress.
`OpenOp`, `RemoveOptions`, and `NfsRecoveryPolicy` also keep their fields private;
use their constructors, builders, and accessors.

Use `SetAttrsOp` with `vsetattrs` for metadata changes, including mixed path and
handle targets. The singular `truncate`, `chmod`, and `chown` helpers delegate
to that engine. Recursive collection uses
`read_dirs_with_options(&[root], ListDirOptions::new().recursive(true))`, returning
one tree per root; `listdir` provides incremental traversal. All portable reads
return `ReadResult`, with `data()` for owned reads and `read()` for buffered reads.

`max_read_bytes` is a collection/batch policy, not a cap on all reads or
process memory. The explicit `fs.std_io(&file)` adapter bounds collecting reads
using the client's read budget. Repeated reads into caller-managed buffers remain caller-managed.
Retain the adapter to preserve its cursor across calls; each new
`fs.std_io(&file)` starts at zero. Use `adapter.read_to_end(&mut buffer)` to
collect from its current cursor with the client's bound. At the limit it may consume one extra EOF-probe byte;
failure does not roll back the cursor, and `read_to_end` retains the bytes already
appended to its caller's buffer.
Changing Auto limits also updates cached connections; existing Auto handles
use the current policy for grouped reads. Directory path-byte limits apply to
the public Auto paths, including mount prefixes.

Cloning a client shares one synchronized connection; it does not parallelize
RPCs. Use `connect_pool` for independent application workers, or
`connect_read_pool` for ordered read-ahead on a stable large file.
`read_stream` returns `StreamCompletion::Complete` at EOF or
`Stopped { next_offset }` after the callback requests a stop. A pooled stop
waits for outstanding reads to finish and then closes the worker handles.
Neither streaming path promises a snapshot of a concurrently modified file.

`listdir(root, ListDirOptions::new().recursive(true).enter_leave(true), callback)`
adds depth-first `Enter`, `Entry`, and `Leave` events. Use `.fields(...)`
for selective metadata and `.sort_by_name(true)` for sorted siblings.
Return `WalkControl::SkipSubtree` from `Enter` to avoid reading that directory;
`Stop` ends the entire traversal. A pruned directory still receives `Leave`.
Errors or stopping can leave the remaining events undelivered. Never replay
side-effectful callbacks through another backend after a partial traversal.
The root is depth zero and counts toward the aggregate entry/path budget.
Lifecycle traversal is lazy between directories and retains bounded directory
buffers for depth-first ordering. Sorting also requires buffering a complete
bounded directory. `visit_dirs_ordered` remains available for custom sibling
ordering, descent admission, and callbacks receiving complete listings.
Buffered recursive/lifecycle `listdir` reopens directory paths without following
symlinks, including replaced ancestors. Backends unable to enforce this return
`Unsupported`. Ordinary shallow listing retains its existing path behavior;
`ListDirOptions::follow_symlinks(false)` requests strict no-follow directory opens.

`Nfs::discover_mount(path)` inspects a supported Linux mount without opening
a network connection. Its opaque result exposes the host, export root,
mountpoint and canonical local directory for grouping application operands.
Use `Nfs::from_mount` to connect; discovery does not establish cache coherence.

After building the application ports and C adapter, run
`VFSI_PORTS_ROOT=/path/to/ports VFSI_LIBRARY=/path/to/libvfsi_c.so bash scripts/test-port-listing-overflow.sh`
from the development repository to check exact-limit and overflow behavior
with a test-only legacy adapter, including rsync deletion safety. The fixture
streams synthetic entries without creating 200,001 files on disk.

`listdir(root, ListDirOptions::new().recursive(true), callback)` incrementally
delivers child Entry events without retaining the whole tree; callbacks may
reenter the client. It does not follow entry symlinks.
Backends without native paging may retain one bounded directory snapshot.
The visitor finishes that directory's pages before descending into children,
so snapshots cannot accumulate across ancestors.
The scalar visitor accepts `Ok(WalkControl::Continue)`, `SkipSubtree`, or
`Stop`; enable `enter_leave(true)` and skip on Enter to prune before child I/O.
Stopping ends the entire traversal, not one subtree. It returns
`TraversalCompletion::Complete` or `Stopped`. Callback
errors propagate, and even breaking on the last entry reports `Stopped`.
Child directories retain native anchored cursors and batching through `vlistdirs`; plain entry mode
does not impose depth-first callback order. `NfsDir::try_close` retains ownership after a failed close,
as `NfsFile::try_close` does. Dropping either handle may block on the client lock
and a network cleanup operation, and discards cleanup errors. Close explicitly
when errors matter. `Write::flush` requests backend durability (`sync_data`),
not merely flushing a user-space buffer, and may cost a network round trip.

`diagnostics::take_and_reset` drains **process-wide** counters; diagnostic
consumers must coordinate. The independently sampled fields are not an atomic
snapshot under concurrent activity. `compound_bytes` is `None` unless exact
byte telemetry was enabled with `VNFS_STATS=1` before the first compound.

An NFS COMPOUND is ordered but **not transactional**. If operation `i` fails,
the server stops processing that compound: the prefix before `i` may already
have succeeded and the suffix was not executed. Public vector methods return
all values on success or one `Error` on failure; they never promise
rollback. A transport failure may have an unknown index and ambiguous effects,
which callers must reconcile before retrying a mutation.
`Error::index()` returns `Option<usize>` and identifies a logical request, not
a guaranteed completion boundary. That request may itself have completed some
chunks before a later semantic failure. Error status alone therefore provides
no retry-safety or "not applied" guarantee. `Error::kind()` supplies a portable
category; `status()` and `domain()` retain native protocol details. Standard
`io::Error` adapters retain the original `Error` as their source.
`transport_kind()` distinguishes known timeout, connection, invalid-reply,
and authentication failures. Unknown causes remain `Other`; these categories
describe provenance, not whether retrying is safe. `err_no()` is a raw
compatibility value, not a portable errno for every backend.
`close_files` consumes its handles and attempts best-effort cleanup on failure.
`vclose(&mut files)` preserves them after an error so the caller can
reconcile uncertain close status explicitly; a confirmed successful call
disarms every handle.
An armed handle after a failed close only retains local cleanup ownership:
it does not prove that the server still considers the handle open.

Low-level backend construction belongs to the separate `vfsi-*` crates;
`vnfs` does not republish protocol construction or backend extraction.

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

### Recursive removal and path-entry races

`remove_dir_all` and `remove_dir_contents` take paths. Between the caller
naming a path and the backend starting work, a concurrent actor can replace a
path component with a symbolic link, so a privileged process may remove an
unintended tree. This is the entry-point TOCTOU described by
[RUSTSEC-2023-0018](https://rustsec.org/advisories/RUSTSEC-2023-0018.html); it
cannot be fixed inside a path-taking library and must be handled by the caller.

The NFS backend addresses directories by filehandle and issues `REMOVE` and
`READDIR` relative to held handles, preventing a swapped intermediate symlink
from redirecting its walk. Generic/path-only backends do not provide this
guarantee; do not treat `VfDir::Path` as a secure directory handle.

For privileged or attacker-influenced paths, root the removal at an
already-open directory instead of a path:

```rust,no_run
use vnfs::VfsiExt;
# fn example(fs: &vnfs::NfsClient) -> vnfs::Result<()> {
let mut dir = fs.open_dir_handle("/attacker/controlled")?;
fs.remove_dir_contents_handle(&dir)?; // rooted at the resolved directory handle
dir.try_close()?;       // Retains cleanup ownership if explicit close fails
# Ok(())
# }
```

`open_dir_handle` rejects backends without a genuine handle and does not follow
a final symlink. `remove_dir_contents_handle` empties that retained directory
while preserving its root, through `Vfsi::vremove_dir_contents`. Use
`Vfsi::vopen_dirs` to open multiple directories. Import `DirHandle` for generic
handle lifecycle methods.
`remove_dir_all_with_options`, `remove_dir_contents_with_options`, and
`Vfsi::vremove(paths, mode, options)` accept a `RemoveOptions` value choosing best-effort vs
fail-fast removal (`continue_on_error`, defaulting to fail-fast), a vector batch-size cap
(`batch`), and the retry count for transient per-entry statuses (`retries`).
These tuning options are implemented by the NFS backend. Backends using the
generic remover reject non-default options rather than silently ignoring them.

## Platform and build requirements

The supported native target is Linux. The minimum supported Rust version is
1.88. Normal builds compile the pinned, packaged ntirpc source into a static
archive; no system libntirpc is required and `build.rs` does not clone
or download native source. After Cargo dependencies have been
fetched, the native build can run without network access. docs.rs uses
checked-in FFI declarations and does not require the native development
packages. NFS servers must expose an NFSv4 pseudo-root reachable by the
supplied host name. Kerberos RPCSEC_GSS requires the opt-in Cargo feature, a
valid default credential cache, and matching server configuration. There is
currently no RPC-over-TLS, callback/delegation, or asynchronous API.

## Package boundary

The `vnfs` crate is the NFS-focused application package in the wider VFSI
project. Backend implementers depend directly on
`vfsi-core`, `vfsi-sync`, and `vfsi-nfs`. New protocol backends are
published as separate `vfsi-*` crates so each backend has an independent
dependency and release boundary.

NFS and NFSv4.2 server-side COPY are enabled by default. The dummy backend is
an opt-in test/development feature, and RPCSEC_GSS is intentionally not
enabled by default.
Applications that only need interface types can disable default features:

```toml
vnfs = { version = "0.0.15", default-features = false }
```

## Secure authentication (optional)

AUTH_SYS carries the calling process's numeric UID/GID without cryptographic
peer identity, integrity, or privacy. Use it only on a trusted network with
server export policy that treats those credentials appropriately. AUTH_SYS is
the compatibility default; selecting `NfsAuthentication::RpcsecGss` explicitly
requests Kerberos-backed authentication and never downgrades to AUTH_SYS.

Enable Kerberos-backed RPCSEC_GSS explicitly:

```toml
vnfs = { version = "0.0.15", features = ["rpcsec-gss"] }
```

The client uses the process's default GSS credential cache (normally populated
with `kinit`) and does not accept or retain passwords. Integrity protection is
the recommended baseline.

```rust,no_run
# #[cfg(feature = "rpcsec-gss")]
# fn main() -> vnfs::Result<()> {
use vnfs::{Nfs, NfsAuthentication, RpcsecGssProtection};

    let fs = Nfs::builder("nfs.example.com")
        .root("/export/application")
        .auth(NfsAuthentication::RpcsecGss {
            // None derives the GSS host-based name nfs@nfs.example.com.
            service_principal: None,
            protection: RpcsecGssProtection::Integrity,
        })
        .connect()?;
    drop(fs);
    Ok(())
}
# #[cfg(not(feature = "rpcsec-gss"))]
# fn main() {}
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

## License

Licensed under either of

- Apache License, Version 2.0
- MIT license

at your option.

[fast]: https://www.usenix.org/conference/fast17/technical-sessions/presentation/chen
[benchmark]: https://github.com/vmingchen/vnfs/blob/main/crates/vnfs/examples/small_files_benchmark.rs
