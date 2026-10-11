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

The allocation and layout changes above have no end-to-end measurements in
this audit. The write scheduler follow-up below was benchmarked separately.

## Write dependency scheduling follow-up

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

Logs and command/result/timing records are on the VM under
`/tmp/vnfs-write-scheduler-*.log` and
`/tmp/vnfs-write-scheduler-{focused,live,checks}-results.json`.
The configured NFS fixture was reused without service changes. No access
limitation occurred. Full CI, remaining live suites, live SMB and real io_uring
were not run for this follow-up. Benchmarks and correctness tests ran separately.

## Follow-up candidates

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

## Allocation and layout validation scope

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
