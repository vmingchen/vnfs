# Streaming, resource limits, and mount routing

## Bound allocations

Owned reads inherit a 16 MiB aggregate payload budget. `ReadOptions` permits a
nonzero per-call override; `None` inherits the client policy, not unlimited.
Range lengths are checked before dispatch; whole-file reads fail on overflow.
Limits bound logical data, not RPC envelopes, allocator overhead, or all process
memory. Caller-managed buffers and read pools have separate memory policies.

```rust,no_run
use vnfs::prelude::*;
use vnfs::files::{ReadOp, ReadOptions, StreamOptions};
use vnfs::directory::{Attributes, ControlFlow, ListDirOptions};
use std::num::NonZeroUsize;
# fn example(fs: &vnfs::nfs::NfsClient) -> vnfs::Result<()> {
let results = fs.vread(
    [ReadOp::whole("/file-1"), ReadOp::whole("/file-2")],
    ReadOptions::new().with_max_total_bytes(NonZeroUsize::new(32 * 1024 * 1024)),
)?;
# let _ = results;
# Ok(())
# }
```

Configure defaults with `NfsBuilder::limits(ResourceLimits)` or
`Auto::with_limits(ResourceLimits)`. Directory entry/path-byte limits bound
listings and traversal separately; `ListDirOptions::unlimited()` opts out.
`Attributes` chooses fields READDIR obtains without per-entry stats. Sparse
getters return `None` when a field was not returned.

## Large files and parallelism

`vstream` delivers bounded chunks (1 MiB by default), possibly smaller according
to negotiated limits. Its callback may reenter the client. Consume rather than
retain chunks to keep memory bounded; reads do not promise snapshot consistency.

```rust,no_run
use vnfs::prelude::*;
use vnfs::files::{ReadOp, ReadOptions, StreamOptions};
use vnfs::directory::{Attributes, ControlFlow, ListDirOptions};
use std::num::NonZeroUsize;
# fn example(fs: &vnfs::nfs::NfsClient) -> vnfs::Result<()> {
let mut bytes_seen = 0u64;
fs.vstream(
    &["/dataset/large.bin"],
    StreamOptions::new().with_chunk_size(NonZeroUsize::new(4 * 1024 * 1024).unwrap()),
    |index, offset, chunk| {
        assert_eq!(index, 0);
        assert_eq!(offset, bytes_seen);
        bytes_seen += chunk.len() as u64;
        Ok(ControlFlow::Continue(()))
    },
)?;
# Ok(())
# }
```

Clones share one serialized connection. `connect_pool` creates independent
sessions for parallel vector cohorts, distributed by `next_client`; it does
not split a vector across sessions. `connect_read_pool` pipelines ranges from
one file in order. Defaults are four workers, 1 MiB chunks, eight outstanding
ranges, and a 16 MiB buffer budget. Sessions persist across streams; worker
handles open/close per stream. Stopping waits for outstanding reads and closes
handles. More sessions can increase server load.

Tune chunk sizes, workers, and buffer caps against the same file/server, with
cold and warm runs reported separately. The repository's
[large-file benchmark](https://github.com/vmingchen/vnfs/blob/main/crates/vnfs/examples/large_file_read_benchmark.rs)
compares single-session and pooled reads and reports setup separately.

## Directory callbacks

`vlistdirs` delivers bounded pages; `read_dirs_with_options` collects them.
`listdir` visits entries; enable `with_recursive(true).with_enter_leave(true)` for
depth-first Enter/Entry/Leave events and prune with `SkipSubtree` on Enter.
`Stop` ends the entire traversal. Sorting and lifecycle ordering require
bounded directory buffers; plain entry traversal retains native paging and
batching without guaranteeing depth-first order. Callbacks may reenter the
client; errors/stops can leave events undelivered. Concurrent mutation can
invalidate continuation cookies.

## Filesystem object identity and open instances

`file_id()` alone is scoped to a filesystem and can repeat on another
filesystem. Request `Attributes::identity()` and use `Attrs::fs_obj_id()` to compare
`(fs_id, file_id)` where the backend supplies both. `fs_id()` reports the
filesystem component; it is independent of the mount used to reach the object. The local backend
reports (`st_dev`, `st_ino`); NFS reports its filesystem ID and file ID. These
values are scoped to a backend identity namespace, not globally unique across
unrelated servers or clients, and can be reused after deletion. A backend that cannot report both parts returns
no `FsObjId`.

`OpenFd` is the owned File Descriptor handle to one open instance. The
generic concrete type is `vfsi_sync::OpenFile<F>`; `Fd` is a numeric backend
slot, and `FileOperand::Fd` references that slot without owning cleanup. Opening the same
object twice produces the same `FsObjId` and separate handles; closing one does
not close the other. Handle operations continue to address the opened object after
rename or unlink. A saved pathname is for diagnostics only.

Mount IDs and mount paths select a route; they are not file identities.
`Auto` resolves a path to an eligible underlying filesystem before dispatch,
then pins the chosen client on each opened handle. `Nfs::from_mount` resolves
the mounted path to a direct NFS client when the client is created.

## Existing mounts

On Linux, `Nfs::from_mount` discovers an absolute directory under an ordinary
NFSv4.1/4.2 TCP AUTH_SYS mount, verifies remote identity, preserves read-only
restrictions, and creates a direct connection. `NfsBuilder::from_mount` permits
tuning before connecting. Unsupported security or ambiguous mappings fail;
after connection, discovered configurations pin the remote filesystem root,
credentials, and read-only policy. Reconnect and new sessions made from a
resolved configuration do not consult the original kernel mount or discovery
path. `Auto` shares clients across equivalent endpoint/export/version/credential
mount aliases. Cross-client links and renames return `EXDEV`; native COPY returns
unsupported so helpers can stream through the resolved clients. These
operations never switch to kernel I/O after resolution.

`Nfs::discover_mount` inspects without connecting. `nfs::mount::NfsMountSession`
combines a connection with mapping host-local operands to remote paths.
Neither discovery nor connection establishes coherence with kernel caches.

`Posix` always uses the kernel. Opt-in `Auto` groups suitable read-write
AUTH_SYS NFS mounts for direct I/O and leaves local, SMB, Kerberos, and unsafe
mappings on the kernel route. Final symlinks and ambiguous create-if-missing
opens remain on the kernel route; supported exclusive creates may go direct.
`route_for` is a candidate; the opened file's `route()` is definitive. Handles
remain pinned, and ambiguous writes are never replayed through another
backend. Direct credentials are captured per connection; direct handle use
after a filesystem identity change is rejected.

**Direct NFS clients do not share or invalidate kernel caches.** Aliases can
reach the same objects. Prefer `Posix` for exact kernel semantics or warm
page-cache reuse; coordinate caches explicitly when mixing direct and kernel I/O.

## Diagnostics

`diagnostics::take_and_reset` drains process-wide counters, not per-client
statistics. Coordinate consumers; sampled fields are not an atomic snapshot.
Exact compound byte telemetry requires `VNFS_STATS=1` before the first compound.
Count actual RPCs when assessing batching—not API calls.
