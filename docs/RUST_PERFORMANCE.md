# Rust performance notes and audits

Current implementation notes are separate from the dated audit and benchmark
records below. Layout observations are target-specific, not ABI guarantees;
historical test counts do not describe the current checkout. Benchmark results
describe their recorded host and workloads, not universal performance guarantees.

## Current implementation notes

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

Complete positional writes keep successor lists and outstanding dependency
counts for conflicting requests on the same underlying object. Local clients
use cached device/inode pairs. NFS uses retained handle equality when every
request shares one handle; otherwise it batches FSID/FILEID queries once per
live descriptor and caches only complete object identity. Unequal NFS handles
may still denote one object (RFC 8881 section 9.11). Separate opens and
hard-link/renamed aliases therefore participate in the same ordering. Cached
identities are invalidated on descriptor reuse and reconnect.
Backends without object references conservatively order potentially conflicting
requests across the whole batch; paths never establish independence. A request
releases its successors only after every byte is acknowledged; short writes
remain in subsequent ready waves. Ready waves retain input order, and their
index buffers are reused. Empty requests still undergo whole-batch validation
and are complete without dispatch. Append ordering, error indices, and failure
replay rules remain unchanged. Small object cohorts use direct conflict
comparisons: dense ones switch after 16 requests, sparse ones after 256. Larger
cohorts index disjoint byte intervals by their
latest writer, retaining transitive ordering without redundant edges. Append
is an object-wide barrier; unknown identities still rebuild one conservative
group. Interval updates and graph construction take O(n log n) time and O(n)
space per object, rather than constructing every pairwise conflict. The
first successor is stored inline to avoid a single-edge allocation; graph arrays
are prepared before conflict comparisons for batches of two or more requests.
Scalar writes need no graph arrays. The [scheduler benchmark](write-scheduler-benchmark.md)
records release comparisons, sparse-case tradeoffs and measurement limits.

### Write-frontier comparison — 2026-10-18

Compared with `9ae4df0` on the ARM64 Linux VM, using matched release builds,
nine alternating CPU-pinned pairs per case and 100 ms target samples. All 35
cases retained identical backend dispatch counts. Fourteen were classified as
improved, 21 within noise/small-change bounds, and none regressed under the
runner's bootstrap/5% criteria. Dense 1,024-request sink scheduling improved
14.7x; 1,024-request scatter improved 2.0x; buffered POSIX overlap at 512
requests improved 2.7x. Sparse 64-request chains remained flat. These are batch
latency comparisons, not NFS-throughput claims.

An initial universal 16-request cutoff regressed sparse chains and mixed
workloads. Retaining linear comparison longer for sparse groups removed those
regressions in the full rerun. Raw reports/build provenance are retained under
`target/write-scheduler-frontier-tuned`; reproduce with
`scripts/benchmark-write-scheduler.py --baseline 9ae4df0 --samples 9 --target-ms 100`.

The [allocation and layout audit below](#allocation-and-layout-audit--2026-10-11) records measured
layouts, implementation evidence, tradeoffs, and remaining optimization candidates.

## Allocation and layout audit — 2026-10-11

This focused audit starts from commit `d21a87a` and examines representative
Rust hot paths: core byte paths and operation types; sync read/write dispatch,
completion and traversal helpers; NFS compounds, planning, metadata, identity
resolution, path I/O and worker state; Auto routing/handles; local/Posix and
io_uring I/O; SMB vector dispatch; and allocation boundaries in the C binding.
It is not a workload profile or an exhaustive line-by-line review.

### Implemented changes

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

The allocation and layout changes above have no end-to-end measurements in
this audit. The write scheduler follow-up below was benchmarked separately.

## Write dependency scheduling follow-up — 2026-10-11

This follow-up starts from commit `4b9ff87`. In
`crates/vfsi-sync/src/client/write.rs`, complete positional writes previously
rescanned each pending request's earlier conflicts on every wave. For a fully
conflicting batch, increasingly long completed prefixes made readiness checking
cubic even when each request succeeded in one backend call.

The scheduler now stores successor lists and outstanding dependency counts.
Each fully acknowledged request releases its outgoing edges once. Short writes
remain ready and keep their successors blocked until completion. Newly ready
requests join the next wave; the wave is sorted only when needed to preserve
input order. The two ready-index vectors reuse their capacities, completed
successor vectors are freed, and construction-only path records are dropped
before dispatch. Empty requests retain whole-batch validation and are considered
complete from the start, so they do not create blocking edges. Each successor
list keeps its first index inline and allocates an edge vector only for additional
successors. Graph arrays are allocated before the pairwise loop for batches of
two or more requests; scalar writes need neither array. The append flag is read
once per request before comparing earlier ranges. On this 64-bit build a
successor slot occupies 32 bytes instead of a 24-byte vector header, plus an
eight-byte dependency counter. This increases per-node storage but removes
single-edge allocations, and was retained after measuring the sparse cases.

For `n` requests, `E` dependency edges, and `D` dispatched request occurrences
(including short-write retries), scheduling costs `O(n + E + D log(n + 1))` in the worst
case, including ready-wave sorting. Every dependency edge is visited once after
construction; already ordered waves need only a linear order check. Pairwise
same-path conflict construction remains `O(n²)` in the worst case and graph
storage remains `O(n + E)`. A fully conflicting one-call-per-request batch now
has quadratic total local work, with one-item waves needing no sorting.

A deterministic regression uses 128 one-byte writes to the same offset. The
previous implementation projected requests 366,400 times; the new implementation
projects them 384 times while preserving the same 128 ordered backend calls.
This measures repeated request work, not end-to-end throughput or latency.
Four additional regressions cover mixed ready/short-write waves, completed empty
requests, later-wave error remapping without replay, and comparison with the
previous readiness rule across 64 deterministic batches containing mixed paths,
overlaps, appends, empty payloads and short writes. Existing public APIs,
acknowledged-progress handling, stability aggregation and append offsets are
preserved.

### Scheduler benchmarks

Release comparisons against `4b9ff87` cover 35 sink/POSIX cases with nine
alternating paired samples, followed by six longer confirmations with 21 pairs.
Buffered POSIX overlap batches improved by 8.671× at 256 requests and 20.175×
at 512; the 256-request confirmation measured 8.910×. Sink scheduling improved
by 48.841× at 1,024 fully conflicting writes and 1.131× for the 256-request
mixed short-write case (1.132× in confirmation). Independent POSIX controls
were effectively flat. No final case had a speedup interval entirely below 1.
The initial sparse-graph refinement's scatter regression was fixed before these
runs. See [the benchmark report](write-scheduler-benchmark.md) for all cases,
confidence intervals, raw samples, source hashes, reproduction commands and
measurement limits. No NFS throughput or durable storage improvement is claimed.

### Baseline reuse and release verification

The benchmark runner now records build configuration and compares it before
baseline reuse, rejecting legacy results with missing configuration evidence.
It captures Rust/Cargo flags and versions, wrappers, workspace profiles and
Cargo configuration hashes, and compares the Cargo-reported artifact profiles.
Checks remain enabled under `python3 -O`. Artifact selection uses Cargo's actual
executable path. Four real CLI rejection cases and matching reuse passed;
14 new regressions and all 30 script tests passed. The normal release build and
fresh baseline/optimized builds report `opt_level=3` for all five runtime library
targets and the benchmark executable, with no active code-generation overrides.
The newly verified comparison binary hashes exactly match the original two
benchmark datasets. This fix changes benchmark tooling and evidence only.
See [release verification](write-scheduler-benchmark.md#baseline-reuse-fix-and-release-verification)
for commands, timings, guard cases, profiles and raw records. Smoke and check
were rerun; the 11 ignored fixtures and omitted full/live suites remain outside
that selection.

### Scheduler validation

The new `write_allv_dependency_schedule` filter was run first against the
previous scheduler: three semantic cases passed and the readiness-work bound
failed with 366,400 projections. After the change, all five new tests pass,
including the reference-wave comparison. They were rerun first after each
refinement and report 384 projections for 128 conflicting writes. The final
measured implementation was then validated as follows. Focused selections reuse
`test-faults test-support` for `vfsi-sync` and `posix test-faults` for `vnfs`.
Live selections retain all NFS features and fault coverage.

| Command or selection | Result | Runner wall time |
|---|---|---|
| `./scripts/test-rust.sh --package vfsi-sync --test native_client` | 61 passed | 0.69 s |
| `./scripts/test-rust.sh --package vfsi-sync --lib` | 8 passed | 1.45 s |
| `./scripts/test-rust.sh --package vnfs --test client_vectors` | 5 passed | 4.74 s |
| `./scripts/test-rust.sh --package vnfs --test application_boundary` | 5 passed | 1.81 s |
| `./scripts/test-rust.sh --package vnfs --test public_api` | 18 passed | 3.50 s |
| Live NFS 4.1: `same_file_` | 4 passed | 4.31 s |
| Live NFS 4.1: `descriptor_short_writes_preserve_contiguous_contents` | 1 passed | 0.85 s |
| Live NFS 4.2: `same_file_` | 4 passed | 0.86 s |
| Live NFS 4.2: `descriptor_short_writes_preserve_contiguous_contents` | 1 passed | 0.84 s |
| `./scripts/test-ci-local.sh smoke` | 167 passed; 11 fixture tests ignored | 10.52 s |
| `./scripts/test-ci-local.sh check` | Formatting and Clippy passed | 5.46 s |

For each live row, the command is `./scripts/test-rust.sh --package vfsi-nfs
--test nfs --filter FILTER`, using `VFSI_NFS_SERVER=127.0.0.1:2049`,
`VFSI_NFS_MINOR=1` or `2`, and `RUST_TEST_THREADS=1`. The runner requires the
server and checks that tests actually execute. Ten selected live test runs
passed, preserving batching and compound-count assertions. The 97 focused
tests passed; the five new tests are included in the 61-test native target.
Smoke overlaps public/library selections and does not cover its 11 ignored
fixtures. Check covers the workspace and detached Python adapter formatting
and Clippy jobs, including the new benchmark example. At that validation point, each test-runner
invocation also checked its 16 script regressions. The benchmark runner completed the full
matrix and baseline-reuse confirmation; its Python syntax and `git diff --check`
were checked separately.

At the recorded validation, logs and command/result/timing records were on the VM under
`/tmp/vnfs-write-scheduler-*.log` and
`/tmp/vnfs-write-scheduler-{focused,live,checks}-results.json`.
These `/tmp` paths are ephemeral, not checked-in evidence; the benchmark JSON
records linked above are retained. The configured NFS fixture was reused without service changes. No access
limitation occurred. Full CI, remaining live suites, live SMB and real io_uring
were not run for this follow-up. Benchmarks and correctness tests ran separately.

## Follow-up candidates

Reviewed against the implementation on 2026-10-17. These are profiling/code
inspection leads, not promises of a speedup or measured regressions.

| Priority | Location | Evidence and next step |
|---|---|---|
| High | `crates/vfsi-sync/src/client/write.rs`: conflict construction in `vwrite_all_mapped_native` | Readiness scans have been replaced with successor releases. Pairwise same-path conflict construction still compares up to quadratic pairs and retains up to quadratic edges. Profile large same-path batches before adding an interval index or reducing redundant edges. Preserve whole-batch validation, short-write repair, append semantics and input-order conflicts. |
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

## Allocation and layout validation scope — 2026-10-11

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
Logs for those smoke/check runs were written to
`/tmp/vnfs-rust-optimization-{smoke,check}.log` on the VM; these ephemeral paths
are not a durable validation archive.

No access limitation occurred. The existing fixture and mount configuration
were reused. Full CI, other mount-specific fixture cases, live SMB and real
io_uring validation were not run. No end-to-end performance benchmark was run.
A smoke or focused run is not full CI or a performance benchmark.
