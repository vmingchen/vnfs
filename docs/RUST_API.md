# Rust API architecture

The Rust-native VFSI application API is intentionally separated from backend
implementation contracts and the historical POSIX/C compatibility surface.

## Application boundary and migration

`vfsi-core::api` owns `Vfsi`, `VfsiExt`, `OpenFd`, and their portable
requests, results, options, and traversal helpers. It depends on no backend,
RPC library, or async runtime. `vnfs` re-exports these same types; its concrete
NFS and mounted clients implement the core contract. `vfsi-sync` implements
that contract for its generic backend client and owns optimized execution
machinery. Application read/write types are under `vfsi-core::api`, separate
from the legacy backend operation types at the core crate root.

`Vfsi` owns options-aware native vector execution. `VfsiExt` is blanket implemented
for every `Vfsi` and supplies composed workflows and conventional scalar conveniences (`open`, `metadata`,
`read_dir`, etc.). Generic applications need an
`Vfsi` bound and a `VfsiExt` import; they do not implement extensions separately.

Concrete clients do not duplicate these helpers as inherent methods. Native
open and streaming execution hooks are private; directory collection uses the public page visitor. Method syntax
and explicit `VfsiExt` calls therefore share helper semantics, including error
indices, rather than selecting different implementations based on receiver type.

`VfsiExt::read_dirs_with_options` collects shallow directories or recursive trees
with the same `ListDirOptions` used for visiting. Results are grouped by input:
`results[i]` holds the listings for `paths[i]`. Shallow mode has exactly one
listing per root; recursive mode includes descendants. The allocating helper consumes directory pages from the native vector visitor, retaining batching without copying every entry.
`VfsiExt::read_dirs` supplies the ordinary flat shallow result for convenience.

`Vfsi::vlistdirs` handles both shallow and recursive visits through
`ListDirOptions`. Shallow is the default; `.recursive(true)` enables descent.
Depth 0 lists root children, and depth 1 also lists immediate subdirectories.
Recursive depth limits fail on deeper directories unless intentional truncation
is enabled. Callbacks receive owned `DirectoryListing` pages, including pages for empty
directories, run outside locks, and can cancel the entire vector. First pages
and continuation pages use vector waves; pages may interleave across roots.
Cancellation reports every started root, marking unfinished roots `Stopped`. A directory
may produce several pages; each page retains its parent path. The visitor uses
bounded cohorts (up to 32 directories for NFS) and at most 128 delivered entries
per page, starting with one entry to preserve early cancellation. Fallback
backends consume one directory snapshot before starting another.
`VfsiExt::visit_entries_with_options` adapts pages to borrowed per-entry callbacks. Budgets are shared across roots; unspecified limits inherit the
client policy. Metadata selection is pushed into paged enumeration, without
per-entry stat requests. Recursive traversal does not follow entry symlinks.

Removal uses `vremove(paths, mode, options)`, with explicit
`RemoveMode::Entry`, `Tree`, or `Contents`. Contents mode retains roots and
delegates to native anchored removal, never a list-then-delete extension loop.
The vector call preserves indexed failures and may leave partial mutations.

`NfsClient`, `NfsFile`, `NfsDir`, and their borrowed vector requests are opaque
application handles. `Posix` provides the corresponding local/kernel-backed
handles. Clients do not dereference to backend owners, expose locks, accept raw
backends, or provide backend extraction. Builder operations return application
clients, not protocol implementations. Cloning remains cheap and shares the
connection; files retain their existing ownership, cleanup, and error semantics.

Custom backend implementers depend on `vfsi-sync` for `FsClient`,
`OpenFile`, `FsDir`, and backend traits, plus the corresponding protocol crate. Construct and extract backend owners there
instead of using `connect_backend` or extracting an application client. Protocol
conversion helpers also live in those implementation crates, not on application metadata,
flags, results, or errors. There are no historical root aliases: applications use
`Error`, `Result`, and `FileType`. This is a pre-1.0 Rust source change; the C ABI
is unchanged.

Backend implementers import `vfsi_sync::backend::{HandleBackend, VectorBackend}`.
These contracts describe execution responsibilities; applications use `Vfsi`
and `VfsiExt` on clients instead:

- `HandleBackend` is the minimum owned-handle contract: descriptor I/O, lifecycle,
  synchronization, and handle-level metadata/statistics. Scalar-only backends
  do not need namespace or vector operations.
- `VectorBackend: HandleBackend` adds native vectors, paged directories, namespace/link/
  copy operations, and overridable workflows. Defaults compose shared helpers;
  native overrides retain batching and identity guarantees. It is object-safe
  for C and Python adapters, and has no blanket implementation from scalar I/O.
- `NfsClient` owns and shares a connection; `NfsFile` owns a remote handle without
  borrowing the entire client.
- Concrete NFS and SMB backends expose protocol-only negotiated state directly.
- `OpenOp`, `AttrsOptions`, and `SetAttrsOp` replace raw flags and
  overloaded metadata masks.
- Public `v*` operations return ordered results on success or one `vnfs::Error`
  with an optional input index. They do not promise rollback; an unattributable
  transport failure is never assigned a fabricated request index or presented
  as an ordinary filesystem status.
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
maps `/a` to the NFS-visible `/export/project/a`; `Posix::new("/work")`
maps `/a` to the host path `/work/a`. A mount-derived client rooted at
`/mnt/nfs/project` maps `/a` into that remote project directory. Namespace
rooting is not a race-resistant security sandbox.

`Auto` and mount-derived direct NFS connections are independent clients:
they neither share nor invalidate the kernel NFS client's caches. Mixing
direct and kernel accesses, including through aliases, can expose stale reads
or delayed writes. Use `Posix` when kernel cache coordination is required.

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

The `vnfs` crate root exposes the NFS application API. Native `HandleBackend` and
`VectorBackend` contracts, `FileOperand`, `Fd`, `VfAttrs`, raw libc flags,
and NFS protocol modules live in the corresponding `vfsi-*` crates; they are
not republished by `vnfs`. The former `VecFs` trait has been removed. New
application code can start with:

```rust
use vnfs::prelude::*;
```

The native owned-file API never exposes its backend descriptor. Vector
requests made through `NfsClient` verify that every file belongs to that same
client, preventing accidental cross-session descriptor use.

Application code should connect through `Nfs::builder`, which directly
returns the concrete `NfsClient`. `NfsVecFs` and `NfsClientBuilder`
remain available in `vfsi-nfs` for backend embedding. `VfsiExt::open_options`
is the shared builder for every client and mirrors `std::fs::OpenOptions`. File handles expose lifecycle operations;
I/O runs through `Vfsi` vectors. Use `fs.std_io(&file)` for an explicit
`std::io::{Read, Write, Seek}` adapter with its own cursor. Its concrete type
is private; use `Seek::stream_position` to query the cursor. Prepare vector
requests without issuing I/O using
`ReadOp::range(&file, offset, length)`, `ReadOp::into(&file, offset, &mut buffer)`,
and `WriteOp::at(&file, offset, data)`. `read_files` performs bounded
path-based vector reads without remote OPEN/CLOSE phases; `write_files` batches
OPEN, WRITE, and CLOSE phases across files. `read_files` has a 16 MiB aggregate allocation limit by
default. `close_files` consumes a group
of handles and closes them with the vector backend rather than serializing
one close per dropped handle.

Real application ports also need metadata-rich traversal and namespace
operations without constructing `VfAttrs` or calling backend traits directly.
`Attributes` selects only needed attributes; `Attrs` reports optional
fields such as size, permissions, allocated blocks, device ID, full mode,
and named-attribute presence as `Option` so an absent value is not confused with zero. Use
`attrs_with_options` with `AttrsOptions::follow_symlinks(false)` for a no-follow query,
`vlistdirs` to visit pages for several directory operands, with
`ListDirOptions::recursive(true)` for bounded recursive trees. Use the
`VfsiExt::read_dirs_with_options` collector only when retained listings are needed. `DirectoryListing` carries
paths and already-fetched entry metadata. `vcopy` and `vremove`
perform ordered batches without promising transactionality.

## Object IDs, open instances, and mount routes

`FsObjId` identifies an underlying filesystem object. `OpenFd` is the owned
File Descriptor interface to one open instance; `vfsi_sync::OpenFile<F>` is
its generic concrete implementation. A numeric `Fd` is a backend-assigned
File Descriptor slot. `NfsFh` addresses an NFS object, and `NfsOpenState`
retains the NFS client's state for an open instance. These IDs are scoped to
the backend/server identity namespace. Object IDs may be reused after deletion.

`NfsFh` privately owns `Box<[u8]>`: its length is fixed and callers cannot
modify its opaque bytes. `Clone` still copies the bytes and does not perform
NFS OPEN or CLOSE. On a 64-bit target the stored slice uses 16 bytes, versus
24 bytes for a `Vec<u8>` header, saving 8 bytes per handle. This is a storage
layout observation, not a public ABI guarantee or a reduction in the payload
length. A boxed slice permits mutation in Rust; immutability here comes from
the type's private field and its API.

Private fixed byte storage also uses boxed slices for compound backing
payloads, adaptive compound tag keys, saved/opened path snapshots, generated
OPEN-owner tokens, established client-owner IDs, namespace cache/group keys,
retained directory cursor/removal names, and `TreeBuilder` file contents.
Compound payload allocations remain stable as the outer operation/buffer
lists grow. Vectors remain for growing or reusable I/O/XDR/path buffers and
for public byte-vector interfaces (including connect options and protocol
request/result fields). Those public types retain their ownership and mutation
behavior; bytes are boxed when they enter private fixed storage. Path
normalization borrows immutable components from its input for the duration of the call,
avoiding a separate allocation for every component.

Fixed private UTF-8 text uses `Box<str>` for NFS builder/reconnection endpoints,
Auto's retained mount-spec endpoint, and owner/group identity-cache keys and
their eviction queue. The stored string occupies 16 bytes on a 64-bit target,
versus 24 bytes for `String`; the text allocation remains separate. These
fields retain their contents and value-based equality, and cloning still
copies their text. The fixed RPC initialization diagnostic uses `&'static str`
and requires no retained heap allocation. Public `Into<String>` endpoint
inputs, `AutoRoute`/`MountInfo` server fields, error messages, authentication
options, and text-I/O results/buffers retain their existing `String` types.
Growing string builders and strings transferred into those public types also
keep `String`.

Private Auto routes share one `Arc` connection snapshot rather than cloning
its mount paths, endpoint, credentials, and client policy for every operand,
file, or directory cursor. Changing Auto's limits replaces the cached client
view using copy-on-write and preserves its separate route identity; already-open
handles retain their connection and cleanup ownership. The shared allocation
adds reference-count bookkeeping once per connection. Object sizes are Rust
implementation details, not public ABI guarantees.

NFS path planning borrows component slices while constructing compounds; the
compound owns the wire payloads it needs. It restores a saved handle only for
a relative walk, avoiding a redundant RESTOREFH before a root walk and keeping
the reported operation count equal to the compound built. Byte-path splitting allocates only
the returned parent/name buffers, and joining reserves the final length once.
Attribute decoding borrows padded owner/group fields from the reply, and
identity-cache hits borrow names without creating owned keys. The owner and
group namespaces remain distinct under one shared FIFO capacity of 1024 entries.

Merged NFS reads copy wire bytes directly into the growing result buffer and
transfer that buffer into the owned result. Completed compound items form a
prefix in execution order, so result extraction scans that prefix once instead
of searching for each caller. Failure attribution, fallback, retry policy,
ordering, and public request/result types remain unchanged. These reduce local
allocation/copying and quadratic extraction work; they do not establish an
end-to-end throughput improvement without workload measurements.

The [Rust allocation and layout audit](RUST_PERFORMANCE.md) records measured
layouts, implementation evidence, tradeoffs, and remaining optimization candidates.

| Canonical name | Meaning | Linux, NFS, and GCS equivalent | Previous spelling / compatibility |
|---|---|---|---|
| `FsObjId`; `Attrs::fs_obj_id()` | Composite underlying object ID: `(fs_id, file_id)`. | Linux: (`st_dev`, `st_ino`). NFS: (`FATTR4_FSID`, `FATTR4_FILEID`); opaque filehandle is separate. GCS: `(bucket, object name, generation)` identifies an object version. | `FileIdentity` and `file_identity()` remain compatibility aliases. |
| `fs_id` fields/accessors on `VfAttrs`, `Attrs`, and `FsObjId`; `AttrMask::FS_ID` | Filesystem namespace component, optional in `Attrs` when unavailable. | Linux: `st_dev`. NFS: FSID major/minor packed into the existing `u128`. GCS: bucket is the nearest namespace boundary, but is not a filesystem ID. | `filesystem_id()` and `FILESYSTEM_ID` remain deprecated aliases; migrate the public `VfAttrs.filesystem_id` field to `.fs_id`. |
| `Attrs::file_id()`; `FsObjId::file_id()` | Backend file number, meaningful with filesystem ID and backend namespace. | Linux: `st_ino`. NFS: `FATTR4_FILEID`. GCS has no inode number; object name and generation identify a version within a bucket. | Unchanged. |
| Core `OpenFd` trait; `vfsi_sync::OpenFile<F>` | Owned File Descriptor handle to one open instance and its cleanup obligation. | Linux: a descriptor refers to an open file description (`struct file`); separate `open()` calls create separate descriptions, while `dup()` shares one. NFS: client descriptor maps to filehandle and OPEN state. GCS: client reader/writer state without server-side OPEN. | Core `FileHandle` and sync `FsFile` are deprecated re-exports of the same trait/type. Backend wrappers such as `NfsFile` and `AutoFile` keep their names. |
| `vfsi_nfs::client::NfsFh` | Opaque protocol reference addressing an NFS object independently of OPEN state. | Linux: closest to an exportfs filehandle. NFS: opaque server object reference with server-defined persistence. GCS: object resource key/version is the nearest equivalent. | NFS `client::FileHandle` remains a deprecated alias. The internal `WireFileHandle` alias is removed. |
| `Fd`; `FileOperand::Fd(Fd)` | Numeric backend File Descriptor slot, without cleanup ownership. | Linux: numeric process fd references an open file description. NFS: client slot maps to filehandle plus open state. GCS: any descriptor would be client bookkeeping. | `Fd` is unchanged and means File Descriptor. Unused `HandleId` is removed. Migrate `VfFile::Descriptor` patterns/expressions to `FileOperand::Fd`. |
| `FileOperand` | Native backend operand: path, File Descriptor, current-directory target, or saved-directory sentinel. Cloning does not duplicate an open instance or acquire cleanup ownership. | Linux: path/fd/directory-relative operands. NFS: client resolves operands to protocol references. GCS: analogous key/version or client-handle operands; no GCS adapter exists here. | `VfFile` remains a deprecated alias. Path/sentinel variants retain their behavior. Application code uses `Target` for borrowed paths/handles. |
| Private NFS `NfsOpenState` | Filehandle, stateid, cursor, append policy, and optional reopen recipe for a client open instance. | Linux: analogous client state around an open file description. NFS: OPEN state plus filehandle/client bookkeeping. GCS: stream state without remote OPEN. | Formerly private `OpenFile`; renamed directly without an alias. |
| `AutoRoute`, `NfsMount`, `MountInfo`, `NfsMountSession` | Discovery and selection of the underlying filesystem client/route. | Linux: mount ID and namespace determine path routing. NFS: server FSID differs from client mount ID. GCS has no mount identity. | Unchanged; mount IDs are not part of `FsObjId`. |
| `ResolvePath::{Follow, NoFollow}` | Final-component symlink policy. | Linux: follow or leave the final symlink unresolved. NFS: final-component lookup behavior. GCS has no POSIX symlink semantics by default. | Unchanged. |
| `Attrs::change_id()` | Optional backend change/version token, separate from object identity. | Linux has no universal inode change token. NFS: `FATTR4_CHANGE`. GCS: generation tracks data versions; metageneration tracks metadata versions. | Unchanged. |

The local backend builds `FsObjId` from (`st_dev`, `st_ino`); NFS uses FSID
and FILEID. The current SMB adapter cannot report the complete pair, so it
returns no `FsObjId`. GCS is a conceptual comparison, not an implemented
adapter or a proposed numeric encoding. See [Linux inode fields](https://man7.org/linux/man-pages/man7/inode.7.html),
[Linux open-file descriptions](https://man7.org/linux/man-pages/man2/open.2.html),
[NFSv4.1](https://www.rfc-editor.org/rfc/rfc8881.html), and
[Cloud Storage objects and generations](https://docs.cloud.google.com/storage/docs/objects).

Two opens of one object have equal `FsObjId` values but distinct open state
and cleanup lifetimes. Operations through an open handle continue to address
the opened object after rename or unlink; its saved path is diagnostic. Linux
mount IDs select a path route in `Auto`; after open, `AutoFile` pins the
chosen client/route. A mount-derived NFS connection resolves the mount when
constructed, then uses that filesystem connection for later operations.

The [migration ledger](backend-migration.md#identity-and-open-descriptor-names)
lists source changes and compatibility aliases. The
[rename implementation record](IDENTITY_HANDLE_RENAME_PLAN.md) records scope
and verification.

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

Prefer `try_close()` and `vclose(&mut files)` when cleanup errors matter.
They retain ownership on failure. `is_closed() == false` only means local
cleanup ownership remains, not that a remotely ambiguous close failed to take
effect. Consuming `close`/`close_files` perform best-effort cleanup on error, and
`Drop` queues cleanup while a client remains alive. Use `drain_cleanup` to
observe queued failures; final-owner teardown remains synchronous.

Native client and file methods retain `vnfs::Error`. Only the standard-library
`Read`, `Write`, and `Seek` adapters translate failures to `std::io::Error`.

The asynchronous facet is deliberately deferred to a separate future crate;
the synchronous core does not depend on Tokio.

## Resource-bounded reads

An API that discovers the amount of data itself and returns an owned buffer
must impose a finite default allocation limit and expose an explicit override.
`Vfsi::vread` with whole-file `ReadOp::whole` requests, `VfsiExt::read_files`,
and `VfsiExt::read_to_string` therefore inherit a 16 MiB client budget.
Callers may select another bound with `ReadOptions` on `vread`,
`read_files_with_options`, or `read_to_string_with_options`. A
`ReadOptions::max_total_bytes(Some(NonZeroUsize))` override must be nonzero;
`None` inherits the client default. Native backend whole-file collection uses
the separate `ReadAllOptions` type on `VectorBackend::vread_all_with_options_impl`.
`ResourceLimits` sets client defaults through `NfsBuilder::limits`,
`NfsClient::with_limits`, or `Auto::with_limits`. Existing clones retain their
configured policy. Scalar and vector whole-file reads share the optimized
backend path. `vread` with `ReadOp::into` returns byte counts, offsets and EOF,
with no owned data in those results, and also enforces the aggregate policy
because a backend may use an owned-buffer fallback. The same `ReadOptions`
selects an explicit per-call buffer budget. `Auto`
updates cached connection policies when its limits change and uses its current
policy even for previously opened handles. `Auto` traversal quotas count paths
after translation into the public namespace, including mount prefixes.

`vread` range requests are bounded by their caller-supplied lengths and the
aggregate budget. Applications processing larger or untrusted files can use
`Vfsi::vstream`, `VfsiExt::read_stream`, `fs.std_io(&file)`, or repeated
positional vectors. The adapter uses singleton `vread`, `vwrite`, `vgetattrs`,
and `vfsync` calls against the retained handle; rename does not redirect it.
Each adapter has an independent cursor. Its `read_to_end` and `read_to_string`
limit newly collected bytes to the client's read budget. On overflow,
`read_to_end` retains the appended prefix, while `read_to_string` leaves the
string unchanged. The cursor can advance, including a one-byte EOF probe;
these methods do not roll back the cursor. These limits bound logical payloads,
not process RSS or collection performed by arbitrary standard-I/O wrappers.

Allocating directory APIs are bounded for the same reason. `NfsClient::read_dir`
uses finite entry and combined-path-byte defaults; `read_dir_with_options` and
`ListDirOptions` select tighter limits or explicitly opt into unlimited
collection. `read_dirs_with_options` applies these limits across the entire
returned vector. Use `read_dirs_with_options(&[root], ListDirOptions::new().recursive(true))`
for recursive collection with the default depth limit. The result contains
one tree per input root. NFS multi-directory listing
delivers each bounded READDIR page before requesting continuation pages, so
early-stop callbacks no longer retain the whole remote listing. Applications
needing to consume one directory incrementally can use `listdir`
with `ListDirOptions`; set `recursive(true)` to visit a recursive root. The
options-aware helper respects the supplied traversal scope, metadata, and budgets.
`listdir` replaces the former shallow, recursive, options-aware, and event-walk
entry points. Its callback receives `WalkEvent` and returns `WalkControl`.
Use `enter_leave(true)` for lifecycle events and `sort_by_name(true)` for sorted
siblings; those modes retain bounded directory buffers. The default entry-only
mode stays paged and retains native anchored child cursors and batching. With
lifecycle events enabled, `SkipSubtree` on Enter prevents descent.
`visit_dirs_ordered` still supplies custom ordering and complete listings.
`vgetattrs` accepts paths and retained handles through `AsTarget`, including
mixed vectors of `Target::path(...)` and `Target::file(...)`. Handle queries
retain object identity; unsupported backends return an error instead of
resolving the original path. `vfsync` batches retained handles with
`SyncMode::Data` or `SyncMode::All`; scalar `sync_data` and `sync_all` delegate
to it. Ownership queries must explicitly request `Attributes::UID` and
`Attributes::GID`.
The application visitor starts with one entry, then delivers at most 128 entries per page
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
`Vfsi::vremove(paths, mode, options)` expose `RemoveOptions` at the application layer.
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
Use `try_close`/`vclose` to observe close failures immediately. Dropping the
final backend owner still performs synchronous teardown and can wait on request
timeouts; this is not a cancellation mechanism.

Lifecycle observers are delivered after the backend lock is released. They may
reenter the client. Keep callbacks short and avoid strong observer/client cycles.

`ReadResult` has private fields. Use `offset()`, `read()`, `eof()`, `data()`,
`is_buffered()`, and `into_data()`. Backend implementers construct owned results
with `ReadResult::owned`, which derives the byte count, or use
`ReadResult::buffered` for caller-owned storage. Results never retain buffer borrows.
