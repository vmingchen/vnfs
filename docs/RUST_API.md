# Rust API architecture

The Rust-native VFSI application API is intentionally separated from backend
implementation contracts and the historical POSIX/C compatibility surface.
This is the current architecture and contract guide, not a release-specific
migration log. For runnable application examples, start with the
[crate README](../crates/vnfs/README.md) and [canonical examples](../crates/vnfs/examples).

## Application execution

### Option naming

Value-style options and resource/recovery policies use
`with_<property>(value)` for fluent setters and `<property>()` for existing
getters. For example, `ReadOptions::new().with_max_total_bytes(limit)` sets the
override, while `options.max_total_bytes()` returns it. `None` means inherit
the client's budget, not unlimited; `max_total_bytes_or(default)` resolves it.
This is a naming convention, not a change to defaults, zero-value rules,
resource limits, or packed layouts. Existing getters keep their return types.
Dedicated builders such as `OpenOptions` and `NfsBuilder` retain builder-style
names such as `.read(true)` and `.root("/export")`. Python keyword names are
unchanged.

### Referral support

Direct NFS and SMB clients do not follow referrals. SMB connection setup rejects
shares advertising DFS membership before file operations, and automatic DFS
resolution is disabled (there is no `dfs_enabled` option). Connect directly to
the destination server/share instead. A server merely advertising DFS capability
does not prevent access to its ordinary, non-DFS shares.

NFSv4 referrals are discovered lazily: `NFS4ERR_MOVED` (which can also indicate
filesystem migration) returns `ErrorKind::Unsupported` with a descriptive
message. SMB `STATUS_PATH_NOT_COVERED` errors receive the same classification.
Both retain the original protocol status and available request index. These are
semantic errors, not transport failures, and do not authorize automatic replay.
Earlier operations or chunks may already have completed; a vector is not atomic.
Connect directly to the destination NFS server/export to access a referred tree.

### Application traits

`vfsi-core::api` owns `Vfsi`, `VfsiExt`, `OpenFd`, and their portable
requests, results, options, and traversal helpers. It depends on no backend,
RPC library, or async runtime. `vnfs` re-exports these same types; its concrete
NFS and mounted clients implement the core contract. `vfsi-sync` implements
that contract for its generic backend client and owns optimized execution
machinery. Application read/write types are under `vfsi-core::api`, separate
from the legacy backend operation types at the core crate root.

`Vfsi` owns options-aware native vector execution. `VfsiExt` is blanket implemented
for every `Vfsi` and supplies composed workflows and conventional scalar conveniences (`open`, `attrs`,
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
`ListDirOptions`. Shallow is the default; `.with_recursive(true)` enables descent.
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

Custom backend implementers use `vfsi-sync` for `FsClient`, `OpenFile`,
`FsDir`, and `backend::{HandleBackend, VectorBackend}`, plus the corresponding
protocol crate. Construction/extraction of backend owners and protocol
conversion belong there, not on application handles, attributes, flags,
results, or errors. See [Backend implementation](BACKENDS.md) for execution
hooks, binding boundaries, and remaining compatibility aliases. Pre-1.0 Rust
source APIs may change without retaining old application imports.

## Application and backend boundary

### Namespace and route selection

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
per-operation mount lookups to the resulting direct NFS client.

`Auto` shares a filesystem client across equivalent endpoint/export/version/
credential mount aliases. Each opened `AutoFile` retains its selected client
and route. Cross-client link and rename return `EXDEV`; cross-client native
COPY returns unsupported so helpers can stream through the resolved clients.
Operations do not switch to kernel I/O after resolution. Unsupported mounts,
symlinks, and nested-mount tree roots can select the POSIX route during
initial discovery.

### Application contracts and backend types

`vnfs` exposes the application API through its `files`, `directory`, `nfs`,
`posix`, `uring`, and `mounted` modules, subject to the selected backend features. Native
`HandleBackend` and `VectorBackend` contracts, `FileOperand`, `Fd`, `VfAttrs`,
raw libc flags, and NFS protocol modules live in the corresponding `vfsi-*`
implementation crates; they are not republished by `vnfs`. Application code
can start with:

```rust
use vnfs::prelude::*;
```

Application-owned file handles never expose their backend descriptors. Vector
requests made through `NfsClient` verify that every file belongs to that same
client, preventing accidental cross-session descriptor use.

`Nfs::builder(host)` returns an application `NfsBuilder`; `connect()` creates the
concrete `NfsClient`. `NfsVecFs` and `NfsClientBuilder` remain in `vfsi-nfs`
for backend embedding. `VfsiExt::open_options` is the shared file-open builder
and mirrors `std::fs::OpenOptions`. File handles expose lifecycle operations;
I/O runs through `Vfsi` vectors. Use `fs.std_io(&file)` for an explicit
`std::io::{Read, Write, Seek}` adapter with an independent cursor. Its concrete
type is private; use `Seek::stream_position` to query the cursor. Prepare
vector requests without issuing I/O using
`ReadOp::range(&file, offset, length)`, `ReadOp::into(&file, offset, &mut buffer)`,
and `WriteOp::at(&file, offset, data)`. `read_files` performs bounded
path-based vector reads without remote OPEN/CLOSE phases; `write_files` batches
OPEN, WRITE, and CLOSE phases across files. `read_files` inherits a 16 MiB
aggregate allocation limit by default. `close_files` consumes a group of
handles and uses the vector backend rather than serializing one close per
dropped handle.

Real application ports also need metadata-rich traversal and namespace
operations without constructing `VfAttrs` or calling backend traits directly.
`Attributes` selects only needed attributes; `Attrs` reports optional
fields such as size, permissions, allocated blocks, device ID, full mode,
and named-attribute presence as `Option`, so missing values are not confused
with zero. Use `attrs_with_options` with
`AttrsOptions::with_follow_symlinks(false)` for a no-follow query, and `vlistdirs`
to visit pages for several directory operands, with
`ListDirOptions::with_recursive(true)` for bounded recursive trees. Use the
`VfsiExt::read_dirs_with_options` collector only when retained listings are
needed. `DirectoryListing` carries paths and already-fetched entry metadata.
`vcopy` and `vremove` perform ordered batches without promising transactionality.

### Object IDs, open instances, and mount routes

An **object ID**, an **open instance**, and an **operation target** answer
different questions: which object is this, which open owns its state and
cleanup, and how should an operation address it?

The tables distinguish exact protocol mappings from analogies. GCS (Google
Cloud Storage) is a conceptual comparison: vNFS has no GCS backend, and its
numeric `FsObjId` does not encode a GCS resource key.

#### Object identity and attributes

Application imports are in `vnfs::directory`; native types are in `vfsi_core`.
`vnfs::directory::Attributes` is the application name for `AttrMask`.

| vNFS object or field | Meaning and scope | Linux equivalent | GCS equivalent / analogy | NFSv4 equivalent |
|---|---|---|---|---|
| `FsObjId`; `Attrs::fs_obj_id()` | `(fs_id: u128, file_id: u64)` identifies an underlying object within one backend/server identity namespace. Not an open instance or a content version. | `(st_dev, st_ino)`: filesystem/device plus inode number. | Closest version-qualified resource key: `(bucket, object name, generation)`. Unlike an inode ID, this selects a data version; there is no direct numeric mapping. | `(FATTR4_FSID, FATTR4_FILEID)` within the server namespace, not the opaque filehandle. |
| `fs_id`; `Attrs::fs_id()`; `AttrMask::FS_ID` | Filesystem component of the object ID. Not a mount ID or a special device's number. | `st_dev`, not `st_rdev`. | Bucket is the nearest namespace boundary, not a filesystem ID. | `FATTR4_FSID`; vNFS packs its major/minor components into `u128`. |
| `file_id`; `Attrs::file_id()` | File number, meaningful only with its filesystem and backend namespace. | `st_ino`. | No inode-number equivalent; object name identifies a resource within a bucket, with generation selecting a version. | `FATTR4_FILEID`. |
| `Attrs`; native `VfAttrs` | Returned metadata, including which requested fields are available. An attribute snapshot is not an owning handle. | Results of `stat`, `lstat`, or `fstat`. | Object metadata response. | Attributes returned by `GETATTR` or `READDIR`. |
| `Attrs::change_id()` | Optional change token, separate from object identity. | No universal portable inode change token. `ctime` is not an object ID. | `generation` identifies a data version; `metageneration` tracks metadata changes within that generation. Neither is an open handle. | `FATTR4_CHANGE`, not `FATTR4_FILEID`. |
| `Attrs::device_id()` | Device represented by a special file; not the filesystem containing it. | `st_rdev` for a block/character device. | No direct equivalent. | `FATTR4_RAWDEV`, not FSID. |

Linux field meanings are documented in
[inode(7)](https://man7.org/linux/man-pages/man7/inode.7.html).
The NFS mappings follow
[RFC 8881, section 5](https://www.rfc-editor.org/rfc/rfc8881.html#section-5).
GCS distinguishes [objects](https://docs.cloud.google.com/storage/docs/objects)
from their [generation and metageneration metadata](https://docs.cloud.google.com/storage/docs/metadata#generation-number).

#### Open instances, operands, and routing

Application code imports `OpenFd` and `Target` from `vnfs::files`, `DirHandle`
from `vnfs::directory`, and concrete owned wrappers from the selected backend
module, such as `vnfs::nfs::NfsFile`. Native operands and protocol records
belong to backend implementation layers. Their inclusion in this reference
does not make them the preferred application API.

| vNFS type | Role and ownership | Linux equivalent | GCS equivalent / analogy | NFSv4 equivalent |
|---|---|---|---|---|
| `OpenFd`; `vfsi_sync::OpenFile<F>`; wrappers such as `NfsFile`, `PosixFile`, `UringFile`, `AutoFile` | Portable owned-open contract, generic implementation, and application wrappers. Own cleanup of one open instance. | An owning wrapper around an fd referencing an open file description (`struct file`). | Client-side reader/writer state; no general POSIX-style server OPEN descriptor. | Client-owned open resource backed by a filehandle and OPEN state. |
| `Fd` (`vfsi_core`) | Backend-assigned numeric file descriptor, represented as a C `int`. Scoped to its owning backend; the number does not own cleanup. | Numeric process fd is the nearest analogy, but a vNFS `Fd` is not necessarily an OS fd. | Client-side handle-table index, if an adapter provides one. | Client descriptor-table slot, not a wire field. |
| `Target<'a, F>` | Borrowed application operation target: a path or an open file reference. Does not transfer cleanup ownership. | Path operand or borrowed fd. | Object resource key or borrowed client reader/writer. | Resolved to path lookup or the open instance's retained reference. |
| `FileOperand`; `FileOperand::Fd(fd)` (`vfsi_core`) | Native backend operand: path, numeric descriptor, or special directory target. Cloning it neither duplicates an open nor owns cleanup. | Path/fd/current-directory operand. | Resource key or client handle-table lookup. | Client resolves it to a protocol object reference and, when needed, open state. |
| `vfsi_nfs::client::NfsFh` | Opaque bytes addressing an NFS object independently of OPEN state. Cloning copies the reference, not an open. | Closest analogy is an exportfs filehandle, not an fd or inode number. | Version-qualified object resource key is the nearest analogy. | `nfs_fh4`, used by operations such as `PUTFH`. |
| Private `NfsOpenState` | Filehandle, `stateid4`, cursor, append policy, and optional retained open policy. Not a public portable identifier. | Backend bookkeeping around an open file description. | Client stream/upload bookkeeping; not an object generation. | OPEN state plus client bookkeeping. A stateid is not an object ID or a globally unique open-instance ID. |
| `vfsi_sync::backend::OpenObjectRef` | Borrowed scheduler grouping reference: `Id(FsObjId)` or `Opaque(&[u8])`. Describes object aliasing, not OPEN state. | Comparison of the opened objects' identities. | A canonical, version-qualified key could serve this role; no implementation exists here. | Complete FSID/FILEID or a suitable opaque filehandle reference; never a stateid. |
| `DirHandle`; `vfsi_sync::FsDir<F>`; directory wrappers | Owned directory resource for handle-rooted operations; its saved pathname is diagnostic. | Owning directory fd. | No general POSIX directory-fd equivalent; a listing prefix is not an owned directory handle. | Retained directory filehandle/client resource, not regular-file OPEN state. |
| `AutoRoute`, `NfsMount`, `MountInfo`, `NfsMountSession` | Mount discovery and route/connection selection. Not an object's identity. | Mount ID and mount namespace select a path route. | Configured bucket/endpoint/prefix is the nearest routing analogy. | Server/export/session selection; server FSID is distinct from a Linux client mount ID. |

Linux distinguishes a descriptor from its
[open file description](https://man7.org/linux/man-pages/man2/open.2.html).
NFS separately defines [filehandles](https://www.rfc-editor.org/rfc/rfc8881.html#section-4)
and [stateids](https://www.rfc-editor.org/rfc/rfc8881.html#section-8.2).
The GCS open/descriptor entries are client-side analogies, not protocol mappings.

### Identity, ownership, and recovery

Request `Attributes::identity()` before comparing objects.
`Attrs::fs_obj_id()` returns `None` unless both components are available;
missing values are unknown, not zero. The local backend reports
`st_dev`/`st_ino`, and NFS reports FSID/FILEID. The current SMB backend cannot
supply the complete pair and returns no `FsObjId`.

Compare IDs only within a shared backend/server identity namespace. For paths
within one client/router, `Vfsi::same_object_namespace(left, right)` checks
this boundary; it is not an object-equality or existence check. It does not
establish comparability between arbitrary independent clients. Equal
`file_id` values alone prove nothing, and IDs can be reused after deletion.
`FsObjId` is not a permanent globally unique identifier. Linux's view of a
mounted NFS file and a direct NFS client's numeric IDs need not match.

Two independent opens of one object can have equal `FsObjId` values but
separate state and cleanup lifetimes. On Linux, `dup()` instead shares the
same open file description. Copying a vNFS `Fd` or `FileOperand` does not
perform a `dup()` or extend the owning handle's lifetime; descriptor slots
can be reused after close. An open resource is not a snapshot of its contents.

Owned-handle operations continue to address the opened resource after rename,
unlink, or path replacement; the saved path is diagnostic. NFS reconnect uses
batched `OPEN(CLAIM_FH)` on retained `NfsFh` references, with independent owners
for each open instance. Stale or expired references fail rather than reopen
a replacement pathname. Create/truncate effects are never replayed. A
mount-derived client also verifies its pinned remote root identity and
credentials, retaining read-only policy without rediscovering the kernel mount.

Python reader restoration is different: it creates a new open instance by
name after replacing a session. Metadata exposes optional `fs_id` alongside
`fileid`; restoration requires the complete pair and fails closed when either
is missing or the object differs. Rename/unlink can therefore make restoration
fail. Neither ctime nor a change token substitutes for this identity check.

For NFS, equal filehandles from one server identify the same object, but unequal
handles need not identify different objects. Scheduler grouping must establish
canonical identity or conservatively assume possible aliasing. Filehandles
can be persistent or volatile; neither a filehandle nor a stateid is a portable
open-instance identifier.

A GCS generation selects an object version; metageneration is meaningful only
with its corresponding generation. A future adapter must define its identity
semantics rather than hash a resource name into `file_id` and assume
collision-free identity. See GCS
[request preconditions](https://docs.cloud.google.com/storage/docs/request-preconditions).
Neither mount discovery nor object-ID equality adds cache coherence or
transactional guarantees.

Implementation references:

- [Core identities and native operands](../crates/vfsi-core/src/types.rs),
  [borrowed targets](../crates/vfsi-core/src/api/mod.rs), and
  [owned file/directory contracts](../crates/vfsi-core/src/api/handles.rs).
- [Sync owned resources](../crates/vfsi-sync/src/client/handles.rs) and
  [backend object-reference contract](../crates/vfsi-sync/src/backend/mod.rs).
- [NFS opaque filehandle](../crates/vfsi-nfs/src/client.rs),
  [open-state record](../crates/vfsi-nfs/src/nfs.rs), and
  [object grouping](../crates/vfsi-nfs/src/nfs/handle_backend.rs).
- The [backend compatibility appendix](BACKENDS.md#identity-and-open-descriptor-names)
  lists old names, compatibility aliases, and source migration requirements.

Implementation-specific layouts, allocation choices, and scheduling tradeoffs
are documented in [Rust performance notes](RUST_PERFORMANCE.md). They are not
portable API or ABI guarantees.

## Durability and failure rules

### Strict vectors, not transactions

Result-bearing vectors return values in input order on success, or one error
without a partial value vector. Unit-returning mutations use the same strict
error policy. Directory visitors additionally report cancellation/completion as
described above. A strict error is not atomicity: earlier or concurrently
dispatched operations may already have taken effect. Commit/rollback belongs
to a future transactional facet, not VFSI.

For example, typed OPEN has the conventional Rust result shape:

```rust,no_run
use vnfs::{files::OpenOp, Result, Vfsi};

fn open_group<C: Vfsi>(fs: &C, requests: &[OpenOp]) -> Result<Vec<C::File>> {
    fs.vopen(requests)
}
```

On OPEN failure, the collector attempts cleanup of every confirmed successful
handle, including successes after a failed item on concurrent backends. It
selects the lowest request-position failure among reported outcomes, not the
first completion in wall-clock time. The original error remains primary when
cleanup fails; failed cleanup retains ownership for later draining. Cleanup
does not undo creation/truncation, and a lost reply can leave unconfirmed effects.
Borrowed read buffers can likewise contain partial progress when the call fails.

### Attribution and retry safety

The input position is the authoritative vector index. `Error::index()` returns
`Some(n)` for a failure attributable to request `n`, or `None` when attribution
is unknown. Never interpret `None` as zero. Backend wire operation numbers,
split compounds, and chunk-local offsets must be translated to logical request
positions. An index does not describe completed bytes; a failure within one
request can follow successful chunks of that same request.

NFS COMPOUND executes operations in order and stops at its first failure.
Splitting one vector across compounds does not add rollback, and concurrent
backends may complete later requests before reporting a strict error. Mutations
need operation-specific reconciliation or established idempotence before retry;
status codes alone do not prove that nothing happened. Potentially mutating
opens such as exclusive creation are not blindly replayed after a lost reply.
Native partial outcomes and fault-testing requirements are described in the
[backend guide](BACKENDS.md#strict-results-and-resource-ownership).

### Durability and cleanup

The standard-I/O adapter's `flush` delegates to `vfsync` with `SyncMode::Data`;
scalar `sync_data` and `sync_all` select the corresponding mode. Current NFS
writes request `FILE_SYNC4`, so its synchronization hook validates the live
descriptor without issuing a separate COMMIT. SMB issues `FLUSH`; the local
backend calls the corresponding `std::fs::File` synchronization method.

A lost transport response leaves dispatched mutations indeterminate; they
are not replayed automatically.
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
`ReadOptions::with_max_total_bytes(NonZeroUsize)` override must be nonzero;
New options inherit the client default. Native backend whole-file collection uses
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

Directory operations share the same default metadata selection, with or without
explicit default options: mode, size, link count, file ID, and filesystem ID.
Select UID/GID, timestamps, change IDs, or other additional fields explicitly
with ListDirOptions::with_fields to request their backend and decoding work.

Allocating directory APIs are bounded for the same reason. `VfsiExt::read_dir`
uses finite entry and combined-path-byte defaults; `read_dir_with_options` and
`ListDirOptions` select tighter limits or explicitly opt into unlimited
collection. `read_dirs_with_options` applies these limits across the entire
returned vector. Use `read_dirs_with_options(&[root], ListDirOptions::new().with_recursive(true))`
for recursive collection with the default depth limit. The result contains
one tree per input root. NFS multi-directory listing
delivers each bounded READDIR page before requesting continuation pages, so
early-stop callbacks no longer retain the whole remote listing. Applications
needing to consume one directory incrementally can use `listdir`
with `ListDirOptions`; set `with_recursive(true)` to visit a recursive root. The
options-aware helper respects the supplied traversal scope, metadata, and budgets.
`listdir` replaces the former shallow, recursive, options-aware, and event-walk
entry points. Its callback receives `WalkEvent` and returns `WalkControl`.
Use `with_enter_leave(true)` for lifecycle events and `with_sort_by_name(true)` for sorted
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

For recursive removal, `VfsiExt::remove_dir_all` is fail-fast and
`remove_dir_all_with_options`, `remove_dir_contents_with_options`, and
`Vfsi::vremove(paths, mode, options)` expose `RemoveOptions` at the application layer.
Scalar contents removal uses `remove_dir_contents(path)` or
`remove_dir_contents_with_options(path, options)`; multiple roots use `vremove`
with `RemoveMode::Contents`. These calls resolve the current pathname and do not
retain a directory's identity across separate calls or renames. Use trusted
ancestor paths, not a diagnostic name from an old handle.

The core `Vfsi::vopen_dirs` and `vremove_dir_contents` operations remain for
explicit retained-directory vectors. They require genuine directory descriptors;
path-only tokens return `Unsupported` rather than imply handle safety. The NFS
path remover uses a retained handle internally and closes it after the call;
the scalar extension does not open or collect directory contents itself.
The NFS remover processes
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
