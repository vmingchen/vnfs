# Rust allocation and layout audit

This focused audit starts from commit `d21a87a` and examines representative
Rust hot paths: core byte paths and operation types; sync read/write dispatch,
completion and traversal helpers; NFS compounds, planning, metadata, identity
resolution, path I/O and worker state; Auto routing/handles; local/Posix and
io_uring I/O; SMB vector dispatch; and allocation boundaries in the C binding.
It is not a workload profile or an exhaustive line-by-line review.

## Implemented changes

| Area | Previous cost | Change and evidence |
|---|---|---|
| Auto route state (`crates/vnfs/src/auto/routing.rs`) | Every clone copied mount paths, endpoint and credentials along with the client view. The connection snapshot enlarged every route and retained handle. | Share the snapshot with `Arc`. On the VM's 64-bit build, the route occupies 8 bytes instead of 168, and `AutoFile` occupies 120 instead of 280. A regression compares the new layouts with the former inline layouts. |
| Merged NFS reads (`crates/vfsi-nfs/src/client.rs`, `nfs/io.rs`) | Wire bytes passed through a temporary owned vector, then completed vectors were cloned into public results. | Copy reply slices directly into the accumulating buffer and transfer each completed vector using `Option::take`. Regression assertions preserve the original pointer and capacity in returned results. Growing result buffers can still reallocate. |
| Attribute decoding (`crates/vfsi-nfs/src/nfs/metadata.rs`) | Owner/group XDR fields were copied into temporary vectors before ID resolution. | Borrow slices from the live reply. Check all required XDR padding and lengths; regressions cover borrowed pointers, truncated fields, numeric IDs and field alignment. |
| Identity cache (`crates/vfsi-nfs/src/identity.rs`) | Every nonnumeric lookup allocated an owned tuple key, even on a cache hit. | Split owner/group maps so lookups can borrow `str`. Allocate keys only on misses; preserve positive/negative results, update positions and the shared 1024-entry FIFO bound. The second map adds bookkeeping and can retain extra bucket capacity, while the combined entry count remains bounded. |
| Compound result extraction (`crates/vfsi-nfs/src/planner.rs`, `client.rs`) | LOOKUP used a linear range search per completed item; merged READ/WRITE tested membership by scanning the completed list per item. | Borrow the completed execution prefix directly. Item-range extraction is linear instead of quadratic; decoding individual wire operations still costs their count. Tests include sparse, nonmonotonic caller IDs, sequence failure, item failure and trailing failure. |
| Byte paths (`crates/vfsi-core/src/path.rs`, NFS `client.rs`) | Splitting built an intermediate component vector; joining could grow its output multiple times; NFS planning owned a vector for each component. | Split with a reverse separator search, reserve joined length once, and borrow components during compound construction. Chunked resolution retains only a vector of borrowed slices. Tests preserve repeated separators, dot components, NUL and non-UTF-8 bytes. |
| Saved-handle navigation (NFS `client.rs`) | A RESTOREFH was emitted before choosing a root walk that discarded it. The reported count omitted that operation. | Emit RESTOREFH only when walking from the saved directory. A regression first reproduced 5 emitted operations versus 4 reported, then verifies both parent-navigation methods report the actual count. |

The Auto snapshot requires one shared allocation per connection and
reference-count updates when cloning routes. Changing limits uses copy-on-write when older handles/routes
retain a snapshot. It keeps the separate route identity, ownership, credentials
and cleanup behavior; existing handles remain valid. Public request/result and
string/byte-container APIs stay compatible. No new unsafe allocation or packed
representation is introduced.

These are layout and local-copy/allocation improvements. No end-to-end
throughput or latency increase is claimed without workload measurements.

## Follow-up candidates

| Priority | Location | Evidence and next step |
|---|---|---|
| High | `crates/vfsi-sync/src/client/write.rs`: `vwrite_all_mapped_native` | Pairwise same-path conflict construction retains up to quadratic dependency edges. Each successive wave rescans dependencies of every pending item; a fully conflicting batch can make readiness checking cubic. First profile large same-path batches, then use successor lists and outstanding dependency counts; consider an interval index for conflict construction. Preserve whole-batch validation, short-write repair, append semantics and input-order conflicts. |
| Medium | `crates/vfsi-sync/src/backend/helpers/removal.rs`: default recursive removal | The callback clones and retains complete `VfAttrs` records by directory, while classification uses only file type and operand. Retain those two fields instead, then consider bounded pages rather than whole breadth-first levels. Verify default-backend removal ordering and failure behavior, including SMB. |
| Medium | `crates/vfsi-nfs/src/nfs/open.rs`, `namespace.rs`, `directory.rs` | Batched low-level requests repeatedly clone `NfsFh` payloads and names to construct owned protocol request vectors. Add private borrowed request adapters where their lifetimes end at synchronous dispatch, while keeping the existing public owned request types. Measure before considering `Arc` for every opaque filehandle: it changes allocation/refcount costs. |
| Medium | `crates/vfsi-sync/src/backend/helpers/read.rs`: `vstream_impl_default` | Each streaming wave creates fresh index and request vectors. Reuse cleared, bounded-capacity vectors where ownership allows it. Check cancellation, error remapping, memory limits and fairness across files. |
| Low | Sync dispatch error-context/remapping code | Several `map_or(error.clone(), ...)` calls eagerly clone messages/paths on errors. Replace selectively with ownership-preserving matches. This affects exceptional paths and should follow hot-path work. |

The io_uring backend already reuses bounded scratch and scheduling vectors and
can submit final owned-read buffers directly. Preserve its buffer/descriptor
lifetimes and completion draining. SMB request payload vectors are owned by
asynchronous protocol messages; shrinking their headers alone would not remove
the ownership copy. Public `Vec`, `String` and native path APIs are not general
replacement targets merely because their values are often read-only.

## Validation scope

Seven new deterministic regressions cover layout, borrowed storage, ownership
transfer, byte-path rules, identity-cache bounds and completed-prefix extraction.
The existing live Auto limits test additionally checks snapshot sharing before a
limit change, copy-on-write afterward, and stable backend identity across views.
Validation completed:

- New tests first: `./scripts/test-rust.sh --package PACKAGE --lib --filter
  allocation_` for `vfsi-core`, `vfsi-nfs`, and `vnfs`. All seven new tests
  passed; the NFS filter also runs one existing allocation-budget regression.
- Affected libraries: `./scripts/test-rust.sh --package PACKAGE --lib` for
  those same packages: 22 core, 97 NFS, and 31 vnfs tests passed. Eleven
  fixture tests remain ignored in these default library selections.
- Live NFS with the existing `127.0.0.1:2049` fixture: the `vfsi-nfs` NFS
  integration target (26 tests), vnfs `path_` tests (15), and vnfs
  `non_utf8_filenames_roundtrip` (1), on minor versions 1 and 2. These
  selections passed 84 test runs. One extra deep-path case and a retry of
  the initially failing connection case also passed.
- Five explicitly enabled Auto mount tests passed using `/mnt/nfs`:
  direct routing, changed limits with cached connections/open handles,
  public traversal budgets, directory error attribution, and cross-route
  write preflight. The changed-limits test exercises copy-on-write snapshots.
- `./scripts/test-ci-local.sh smoke` passed in 11.16 seconds.
- `./scripts/test-ci-local.sh check` passed in 7.90 seconds: formatting and
  all-target/all-feature Clippy for the workspace and detached Python adapters.
- `git diff --check` passed.

The first NFS 4.2 backend run had one STALE_CLIENTID connection-setup failure
before file I/O (25 tests passed). The same case passed alone and the complete
26-test target passed on rerun; the transient was not reproduced or fixed.
Logs for the final smoke/check runs are at
`/tmp/vnfs-rust-optimization-{smoke,check}.log` on the VM.

No access limitation occurred. The existing fixture and mount configuration
were reused. Full CI, other mount-specific fixture cases, live SMB and real
io_uring validation were not run. No end-to-end performance benchmark was run.
A smoke or focused run is not full CI or a performance benchmark.
