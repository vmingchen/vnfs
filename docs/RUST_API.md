# Rust API architecture

The Rust-native VFSI application API is intentionally separated from backend
implementation contracts and the historical POSIX/C compatibility surface.

## Application boundary and migration

`Fs` owns options-aware native vector execution. `FsExt` is blanket implemented
for every `Fs` and supplies default-policy `readv`, `writev`, and `removev`,
composed workflows, and conventional scalar conveniences (`open`, `metadata`,
`read_dir`, etc.). Generic applications need an
`Fs` bound and an `FsExt` import; they do not implement extensions separately.

Concrete clients do not duplicate these helpers as inherent methods. Native
open, collection, walk and streaming execution hooks are private. Method syntax
and explicit `FsExt` calls therefore share helper semantics, including error
indices, rather than selecting different implementations based on receiver type.

`Fs::read_dirs_with_options` collects shallow directories or recursive trees
with the same `VisitOptions` used for visiting. Results are grouped by input:
`results[i]` holds the listings for `paths[i]`. Shallow mode has exactly one
listing per root; recursive mode includes descendants. The allocating path
retains native directory batching, rather than collecting visitor callbacks.
`FsExt::read_dirs` supplies the ordinary flat shallow result for convenience.

`Fs::visit_dirs_with_options` handles both shallow and recursive visits through
`VisitOptions`. Shallow is the default; `.recursive(true)` enables descent.
Depth 0 lists root children, and depth 1 also lists immediate subdirectories.
Recursive depth limits fail on deeper directories unless intentional truncation
is enabled. Callbacks borrow entries, run outside locks, and can cancel the
entire vector. Budgets are shared across roots; unspecified limits inherit the
client policy. Metadata selection is pushed into paged enumeration, without
per-entry stat requests. Recursive traversal does not follow entry symlinks.

Removal uses `removev_with_options(paths, mode, options)`, with explicit
`RemoveMode::Entry`, `Tree`, or `Contents`. Contents mode retains roots and
delegates to native anchored removal, never a list-then-delete extension loop.
The vector call preserves indexed failures and may leave partial mutations.

`NfsClient`, `NfsFile`, `NfsDir`, and their borrowed vector requests are opaque
application handles. `Mounted` provides the corresponding local/kernel-backed
handles. Clients do not dereference to backend owners, expose locks, accept raw
backends, or provide backend extraction. Builder operations return application
clients, not protocol implementations. Cloning remains cheap and shares the
connection; files retain their existing ownership, cleanup, and error semantics.

Custom backend implementers depend on `vfsi-sync` for `FsClient`,
`FsFile`, `FsDir`, and backend traits, plus the corresponding protocol crate. Construct and extract backend owners there
instead of using `connect_backend` or extracting an application client. Protocol
conversion helpers also live in those implementation crates, not on application metadata,
flags, results, or errors. There are no historical root aliases: applications use
`Error`, `Result`, and `FileType`. This is a pre-1.0 Rust source change; the C ABI
is unchanged.

The following backend contracts describe implementation responsibilities;
ordinary applications use the concrete client methods instead:

- `FileSystem` is the descriptor I/O contract. `MetadataFileSystem`,
  `DirectoryFileSystem`, `NamespaceFileSystem`, `LinkFileSystem`, and
  `CopyFileSystem` add focused capabilities; `NativeFileSystem` is their
  convenient aggregate bound.
- `VectorFileSystem` adds optimized ordered batches.
- `NfsClient` owns and shares a connection; `NfsFile` owns a remote handle without
  borrowing the entire client.
- `NfsExtensions` and `SmbExtensions` contain protocol-only negotiated state.
- `OpenRequest`, `MetadataQuery`, and `SetAttributes` replace raw flags and
  overloaded metadata masks.
- Public `*v` methods return all values or one indexed `VfError`. They do not
  promise rollback; a transport failure is never presented as an ordinary
  filesystem status.
- `Capabilities` is a typed bitset. Integer `VF_CAP_*` constants remain for
  source and C ABI compatibility.
- `NfsClientBuilder` configures namespace root, protocol version, timeouts,
  authentication, recovery, observability, and compound limits before
  connecting.
- `NfsVecFs::shutdown` reports close/session teardown errors; `Drop` remains a
  best-effort safety net.

## Application and backend boundary

Application paths are relative to the client's namespace root, including
paths beginning with `/`. For example, `NfsBuilder::root("/export/project")`
maps `/a` to the NFS-visible `/export/project/a`; `Mounted::new("/work")`
maps `/a` to the host path `/work/a`. A mount-derived client rooted at
`/mnt/nfs/project` maps `/a` into that remote project directory. Namespace
rooting is not a race-resistant security sandbox.

`Auto` and mount-derived direct NFS connections are independent clients:
they neither share nor invalidate the kernel NFS client's caches. Mixing
direct and kernel accesses, including through aliases, can expose stale reads
or delayed writes. Use `Mounted` when kernel cache coordination is required.

On Linux, `Nfs::from_mount(path)` constructs the same concrete `NfsClient`
from an existing NFS-mounted directory. `NfsBuilder::from_mount(path)` supports
additional tuning. Shared discovery in `vfsi-nfs::mount` also serves nfs4fs's
`mount=` constructor and the existing `Auto` router. It selects the actual
covering mount ID, pins its TCP endpoint and root, verifies directory identity,
and faithfully reproduces supported AUTH_SYS credentials. Read-only mounts
reject mutations in the backend. Connection root, version, and authentication
cannot be overridden on a mount-derived builder. Ordinary TCP NFSv4.1/4.2
`sec=sys` mounts are supported; other security modes, bind-root mappings, and
roots containing nested mounts return an explicit error. Discovery adds no
per-operation mount lookups to the resulting direct NFS client and does not
share the kernel client's caches.

The `vnfs` crate root exposes the NFS application API. `VecFs`, `VfFile`,
`Fd`, `VfAttrs`, `VfOpenOptions`, raw libc flags, and NFS protocol modules live
only in the corresponding `vfsi-*` crates; they are not republished by `vnfs`. New application
code can start with:

```rust
use vnfs::prelude::*;
```

The native owned-file API never exposes its backend descriptor. Vector
requests made through `NfsClient` verify that every file belongs to that same
client, preventing accidental cross-session descriptor use.

Application code should connect through `Nfs::builder`, which directly
returns the concrete `NfsClient` alias. `NfsVecFs` and `NfsClientBuilder`
remain available in `vfsi-nfs` for backend embedding. `NfsClient::open_options`
mirrors `std::fs::OpenOptions`; direct `read_at` and `write_at` perform
positional I/O, while explicitly named `read_request_at` and
`write_request_at` values compose vector calls. `read_files` performs bounded
path-based vector reads without remote OPEN/CLOSE phases; `write_files` batches
OPEN, WRITE, and CLOSE phases across files. `read_files` has a 16 MiB aggregate allocation limit by
default. `closev` consumes a group
of handles and closes them with the vector backend rather than serializing
one close per dropped handle.

Real application ports also need metadata-rich traversal and namespace
operations without constructing `VfAttrs` or calling `VecFs` directly.
`MetadataFields` selects only needed attributes; `Metadata` reports optional
fields such as allocated blocks, device ID, full mode, and named-attribute
presence as `Option` so an absent value is not confused with zero. Use
`symlink_metadata_with_fields` for a no-follow query,
`read_dirs_with_options` to batch several directory operands, and
`walk_with_options` for a bounded recursive tree. `DirectoryListing` carries
paths and already-fetched entry metadata. `copyv` and `remove_paths`
perform ordered batches without promising transactionality.

## Durability and failure rules

`flush`, `sync_data`, and `sync_all` call the backend durability operation.
NFS requests `FILE_SYNC4`; SMB issues `FLUSH`; the local backend calls the
corresponding `std::fs::File` method.

An NFS semantic compound error stops that compound, but a logical request can
span several compounds and have already made progress. Other backends can also
finish later requests before reporting a strict-vector error. A lost transport
response leaves dispatched mutations indeterminate; they are not replayed.
`vnfs::Error` exposes a portable `kind()`, native status/domain, optional logical
request `index()`, and operation/path context. It does not infer completion
certainty or retry safety from status codes.
`transport_kind()` preserves known timeout, connection, invalid-reply, and
authentication categories. Unclassified causes remain `Other`, without
guessing from message text. `err_no()` is a raw compatibility accessor;
prefer `kind()` and `status()` for interpretation.

Prefer `try_close()` and `try_closev(&mut files)` when cleanup errors matter.
They retain ownership on failure. `is_closed() == false` only means local
cleanup ownership remains, not that a remotely ambiguous close failed to take
effect. Consuming `close`/`closev` perform best-effort cleanup on error, and
`Drop` queues cleanup while a client remains alive. Use `drain_cleanup` to
observe queued failures; final-owner teardown remains synchronous.

Native client and file methods retain `vnfs::Error`. Only the standard-library
`Read`, `Write`, and `Seek` adapters translate failures to `std::io::Error`.

The asynchronous facet is deliberately deferred to a separate future crate;
the synchronous core does not depend on Tokio.

## Resource-bounded reads

An API that discovers the amount of data itself and returns an owned buffer
must impose a finite default allocation limit and expose an explicit override.
`NfsClient::read`, `NfsClient::read_to_string`, and `VecFs::read_allv` therefore
default to `DEFAULT_READ_MAX_BYTES` (16 MiB). Callers may select another bound
with `read_with_limit`, `read_to_string_with_limit`, or `ReadAllOptions`.
`ResourceLimits` sets client defaults through `NfsBuilder::limits`,
`NfsClient::with_limits`, or `Auto::with_limits`. Existing clones retain their
configured policy. Scalar and vector whole-file reads share the optimized
backend path. `readv_into` returns counts, offsets and EOF, and also enforces
the aggregate policy because a backend may use an owned-buffer fallback.
`readv_into_with_limit` provides an explicit per-call buffer budget. Auto
updates cached connection policies when its limits change and uses its current
policy even for previously opened handles. Auto traversal quotas count paths
after translation into the public namespace, including mount prefixes.

Reads whose size is explicit in the request (`readv`, `read_at`, and `pread`)
are bounded by that caller-supplied length. Reads into caller-owned buffers are
bounded by the buffer. Applications processing larger or untrusted files
should stream through `NfsFile`, `Read`, `read_streamv`, or repeated positional
reads instead of raising a whole-file allocation limit without bound.
These limits bound logical payloads, not process RSS or arbitrary
`std::io::Read::read_to_end` calls. For an already-open file, use
`read_to_end_with_limit(max_bytes)`: it reads from the current cursor without
reopening the path. An overflow returns an error and discards the collected
buffer; the cursor can advance, including a one-byte EOF probe. It is not a
cursor-rollback operation.

Allocating directory APIs are bounded for the same reason. `NfsClient::read_dir`
uses finite entry and combined-path-byte defaults; `read_dir_with_options` and
`ReadDirOptions` select tighter limits or explicitly opt into unlimited
collection. `read_dirs_with_options` applies these limits across the entire
returned vector. Recursive `NfsClient::walk_with_options` additionally has a
default depth limit and accepts `WalkOptions`. NFS multi-directory listing
delivers each bounded READDIR page before requesting continuation pages, so
early-stop callbacks no longer retain the whole remote listing. Applications
needing to consume one directory incrementally can use `visit_dir_with_options`
with `VisitOptions`, or `visit_walk_with_options` for a recursive root. These
scalar helpers force the scope indicated by their names while retaining selected
metadata and budget settings. `symlink_metadatav` accepts any `AsRef<Path>` inputs,
including strings and `PathBuf`, consistently with `metadatav`.
The client starts with one entry, then fetches at most 1024 entries per page
and releases its backend lock before invoking the application callback, which
may safely reenter the same client or drop its files. NFS retains a resolved
directory handle and READDIR continuation; the local backend retains its
directory iterator. Other backends take one bounded listing snapshot rather
than re-enumerating for each page. Directory mutation during iteration does
not provide a snapshot and can invalidate continuation or change which entries
are observed.
Directory visitors return `TraversalCompletion::Complete` on exhaustion and
`Stopped` when their callback returns `ControlFlow::Break(())`, even for the
last entry. `ControlFlow::Continue(())` requests another entry; callback
errors propagate. This replaces the previous boolean directory callbacks.

For recursive removal, `NfsClient::remove_dir_all` is fail-fast and
`remove_dir_all_with_options`, `remove_dir_contents_with_options`, and
`remove_paths_with_options` expose `RemoveOptions` at the application layer.
`NfsClient::open_dir_handle` returns an owned `NfsDir` only when the backend has
a genuine directory descriptor; its `remove_contents` methods stay rooted at
that handle and `Drop` queues its cleanup. Backends that only offer path tokens return
`Unsupported` instead of implying handle safety. The NFS remover processes
one bounded READDIR reply at a time (32 KiB by default), advances through the
current pass, and verifies from the beginning after a mutating pass. If a server
invalidates a continuation cookie, it restarts the scan.

The SMB backend exposes `SmbConnectOptions` through
`SmbVecFs::connect_with_options`. Connect setup and ordinary requests have
separate deadlines; the request deadline is also applied to sends, response
waiting, and SMB credit acquisition.

## Lifecycle and read results

File and directory Drop queues cleanup without acquiring the backend mutex or
issuing RPCs while a client remains alive. Later operations drain the queue;
`client.drain_cleanup()` explicitly reports failures and retains failed targets.
Use `try_close`/`try_closev` to observe close failures immediately. Dropping the
final backend owner still performs synchronous teardown and can wait on request
timeouts; this is not a cancellation mechanism.

Lifecycle observers are delivered after the backend lock is released. They may
reenter the client. Keep callbacks short and avoid strong observer/client cycles.

`ReadResult` has private fields. Use `offset()`, `read()`, `eof()`, `data()`,
`is_buffered()`, and `into_data()`. Backend implementers construct owned results
with `ReadResult::owned`, which derives the byte count, or use
`ReadResult::buffered` for caller-owned storage. Results never retain buffer borrows.
