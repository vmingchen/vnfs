# Streaming, resource limits, and mount routing

## Bound allocations

Owned reads inherit a 16 MiB aggregate payload budget. `ReadOptions` permits a
nonzero per-call override; `None` inherits the client policy, not unlimited.
Range lengths are checked before dispatch; whole-file reads fail on overflow.
Limits bound logical data, not RPC envelopes, allocator overhead, or all process
memory. Caller-managed buffers and read pools have separate memory policies.

```rust,no_run
use vnfs::prelude::*;
use std::num::NonZeroUsize;
# fn example(fs: &vnfs::NfsClient) -> vnfs::Result<()> {
let results = fs.vread(
    [ReadOp::whole("/file-1"), ReadOp::whole("/file-2")],
    ReadOptions::new().max_total_bytes(NonZeroUsize::new(32 * 1024 * 1024)),
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
use std::num::NonZeroUsize;
# fn example(fs: &vnfs::NfsClient) -> vnfs::Result<()> {
let mut bytes_seen = 0u64;
fs.vstream(
    &["/dataset/large.bin"],
    StreamOptions::new().chunk_size(NonZeroUsize::new(4 * 1024 * 1024).unwrap()),
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
`listdir` visits entries; enable `recursive(true).enter_leave(true)` for
depth-first Enter/Entry/Leave events and prune with `SkipSubtree` on Enter.
`Stop` ends the entire traversal. Sorting and lifecycle ordering require
bounded directory buffers; plain entry traversal retains native paging and
batching without guaranteeing depth-first order. Callbacks may reenter the
client; errors/stops can leave events undelivered. Concurrent mutation can
invalidate continuation cookies.

## Existing mounts

On Linux, `Nfs::from_mount` discovers an absolute directory under an ordinary
NFSv4.1/4.2 TCP AUTH_SYS mount, verifies remote identity, preserves read-only
restrictions, and creates a direct connection. `NfsBuilder::from_mount` permits
tuning before connecting. Unsupported security or ambiguous mappings fail;
discovered configurations remain pinned to the mount.

`Nfs::discover_mount` inspects without connecting. `helpers::NfsMountSession`
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
