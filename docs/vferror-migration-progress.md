# Standard I/O error migration progress

Baseline: 70bea6a. The implementation stages below record local verification.
Coordinated publication is recorded at the end. The complete design and seven-stage
scope in vferror-simplification.md remain authoritative.

## Reviewed local commits

1. f68e5c3 — characterize source lifetime, fan-out, progress, attribution and
   allocations; preserve the historical comparison and its exact measured source.
2. d8440a1 — move selected causes/handles and cleanup diagnostics without
   redundant clones. Core/native checks passed; the required NFS library run
   executed 268 tests and listed 10 ignored cases, which are not coverage.
3. 974935b — private standard-error payloads, shared mapping tables, minimal
   native/provenance inspection, explicit fan-out and context reuse. Reviewed;
   focused tests, smoke checks and normal commit hooks passed.

## Reviewed atomic cutover

The core now uses owned std::io::Error causes, OpError without arbitrary Clone/Eq,
and IndexedError for lossy multi-input collection. Scalar helpers explicitly
discard attribution. StdIo moves the remaining cause after processing confirmed
progress, preserving interruption nonreplay.

Focused current verification executed 56 tests: allocations 16, error_contract 8,
indexed_errors 5, standard_errors 15, vector_results 12; no ignored cases.
Core all-features compilation also passes.

The smaller native buffered-result slots initially caused one extra healthy
allocation. Read staging now reuses final output for buffered/mixed reads and
retains compact 16-byte bounds for owned-only ranges. Existing allocation-count
limits remain unchanged; a new requested-byte regression test catches enlarged
owned-only staging. Review identified the compact-bounds issue; its fix was
re-reviewed with no findings.

Production default-feature libraries now compile across the native and protocol
backends, the C binding, and the vnfs facade with uring enabled. The detached
Python native production adapter compiles with all features.

The complete vfsi-core all-features run now passes 162 unit/integration/property
tests and all 60 doctests. Existing semantic cases were retained while obsolete
custom-error Clone/Eq/index assertions were adapted to standard causes, explicit
IndexedError wrappers, source identity and native inspection.

The detached native Python all-features run passes 73 tests, including three new
binding regressions; one existing manual sorting-performance control remains
ignored and is not counted as coverage. The two new bulk-listing attribution
tests were also checked with a reversible control: both fail with explicit input
indices removed and pass after the fix is restored.

Targeted review found and fixed two additional attribution gaps: strict NFS
READDIR collection must retain its original IndexedError instead of round-tripping
through RpcError, and scalar-backed Python bulk listing must retain the known
directory input position. Re-review found no further issues within that scope.
The four new NFS mapping/source regressions execute successfully; the full
required NFSv4.2 all-feature library run passes 272 tests, with ten existing
ignored controls excluded from coverage. Component review found no further
issues, including source/fan-out ownership, progress, recovery and cleanup.

The complete vfsi-sync all-features run now passes 165 unit/integration/property
tests plus seven doctests: 19 library tests, all 139 native-client tests, six
closed-handle regressions, and the public backend-contract test. Recurring sync
faults retain explicit group ownership; one-shot causes are moved. Standard-I/O
tests retain prefix/cursor/nonreplay checks, and certainty/progress remain checked
at the rich vector boundary, including full-prefix interruption cases.

A second stream review found that cohort validation was losing known item
indices. Native cohort validators now return IndexedError; scalar/per-slot
callers explicitly consume the wrapper, while strict streams and the copy
experiment remap it through their actual submission maps. A new later-wave test
covers malformed file/offset/length/zero-progress results and cardinality, with
no callback delivery for the invalid wave and no replay or subsequent dispatch.
The test fails if validation attribution is discarded and passes with the fix
restored. Targeted re-review found no further issues.

Every root-workspace target now compiles with all features enabled. Existing
Rust test callers, shared fixtures and runnable examples use separate selected
indices instead of cause indices. Per-input injected failures now occupy their
own outcome slots; malformed-index fixtures that no longer have a representable
cause are covered with malformed outcome counts, retaining nonreplay and cleanup
assertions. Scalar application seams discard helper attribution explicitly.

Additional current verification:
- vfsi-local: 107 tests passed, none ignored; component review found no issues.
- vfsi-uring: 32 library and 14 integration tests passed, none ignored. All three
  required exact kernel outcome checks pass. Component review found no issues.
- vfsi-smb: 76 library and 45 integration tests reported passed, with two and one
  existing ignored controls, respectively. The required local share was enabled;
  the optional restart/DFS controls remain omitted. Six focused provenance/
  cleanup cases were independently executed in review, with no findings.
- C binding: 79 library tests passed, six existing ignored controls. Review found
  two adapter-local descriptor loops dropping their known input index; new
  tests reproduce both defects before the fixes and cover slots zero and one.
  Targeted re-review found no further issues. A separate regression preserves
  the selected Unknown item's indeterminate category in the aggregate result.
- vnfs: 33 library tests passed, 16 ignored; TreeBuilder's ten integration tests
  pass. The six initially failing facade targets now pass 113 tests with no
  ignored cases, including all 50 transfer tests. All 140 doctests pass.
- The detached native Python adapter still passes 73 tests, one existing manual
  performance control ignored. The thin nfs4fs/vsmb native entry packages compile
  and run zero-test targets; this is build verification, not Python coverage.
- The active representation prototype passes all five tests using its owned
  fixture, and all detached fuzz binaries compile. Frozen historical source and
  measurements are unchanged; these prototype checks are not integrated timing.

The original Auto-versus-Posix allocation guard exposed excess routed request
reservation after the representation change. Routed reads now reserve the known
valid input count, reusing the existing count rather than adding another pass.
The original allocation-call and requested-byte assertions pass unchanged.
Focused review found no ownership, batching or healthy-I/O regression.

Two new transfer regressions cover backend-error precedence and lowest-wave-slot
callback selection in both callback completion orders, with original-root/child
attribution, opaque-source identity/release, unchanged source bytes and complete
handle cleanup. Both selection defects are caught by reversible controls;
restoring the implementation passes all four recursive tests. Re-review found
no further issues. A cfg-only non-Linux context-construction defect was also
corrected by inspection; no non-Linux build verification is claimed.

The complete standard server-independent runner (scripts/test-rust.sh) and
required scripts/test-ci-local.sh smoke gate pass. Workspace default all-target
compilation, strict default/all-feature all-target Clippy, and strict detached
native adapter Clippy pass. Frozen comparison source hashes are unchanged.

The required full NFS backend selections pass 364 tests each with minor versions
1 and 2 selected, excluding ten existing ignored library controls. The public
NFS facade passes 155 tests under each selected default, with two ignored;
additional live recovery/transfer/TreeBuilder/ordered-directory targets pass.

Python packages were rebuilt in an owned temporary environment, reusing installed
dependencies without modifying the shared environment. Import assertions verify
every tested extension and package comes from vnfs02. Shared fsspec passes 91
tests; nfs4fs passes 2,042 (544 fixture-dependent skips excluded from coverage);
vsmb's selected client target passes three (one skipped), and vsmbfs's package
target passes four. Mount discovery now checks the exact errno namespace/code,
path, category and absence of scalar indexing instead of old custom wording;
the semantic assertion change was re-reviewed without findings.

The independently compiled C ABI-v3 client and optional-symbol controls pass,
including real published vfsi-c 0.3.11, missing-library/POSIX fallbacks and the
current library. Required C NFSv4.1/v4.2 outcome controls pass for lost/uncached
replies, short reads, confirmed prefixes and later-wave uncertainty.

Final read-only reviews cover core/sync/native backends, NFS/SMB, and facade/C/
Python/docs scopes. No remaining actionable findings were reported. Declaration
audit of changed existing Rust files finds 1,424 original versus 1,440 current
test declarations, with ten intentional renames accounted for; the new indexed
error test target adds five more. This audit supplements executed tests.

This records the reviewed stage-4 source cutover, not completion of the full
seven-stage plan. No push, publication, active port repin or mirror promotion
has occurred.

## Stage 5: binding and numeric-code cleanup

The atomic cutover is local commit `cd25783`. Follow-up cleanup removes the
portable synthetic RPC/unsupported constants; the historical unsupported value
stays private to the Python boundary, matching the C boundary. Local syscall
failures move their original standard errors without converting through a
synthetic numeric fallback. Containment EACCES policy remains unchanged.

Read-only review of all four source changes found no issues. Local tests pass
107 cases; the detached native Python target passes 73 (one existing manual
performance control ignored). Strict workspace all-feature/all-target and
native Python Clippy, C unit tests, independently compiled ABI-v3 client and
smoke checks pass. The rebuilt C SDK passes current/published-old/missing-library
fallbacks and required NFSv4.1/v4.2 outcome controls. Rebuilt Python extensions
pass 2,042 nfs4fs tests; 544 fixture-dependent skips are not coverage.

No new syscall, allocation or execution-order change is introduced by this
cleanup. Non-Linux execution remains unverified.

## Stage 6: integrated evidence and a healthy NFS allocation fix

Binding cleanup is local commit `f68f5ec`. The integrated
[actual-client report](benchmarks/error-integration/README.md) compares immutable
content snapshots with identical independent VecResult semantics. It archives
all 27 scenarios × nine randomized pairs, separate Rust allocation/peak-live
controls, exact source/compiler/binary provenance, syscall/ring counters and
NFSv4.1/v4.2 request-byte and SMB wire controls.

A measured extra healthy NFS allocation led to lazy DescriptorReadPlan failure
slots. Required live regression coverage verifies zero healthy failure-slot
capacity, sparse EBADF/overflow NotAttempted inputs, both healthy siblings,
wire owners and cleanup. Both minor-version descriptor targets pass seven tests.
Read-only network review found no issues.

An exact-capacity conversion experiment was rejected: requested bytes decreased
but zero-byte SMB peak-live memory increased by 1,296 bytes. That code and its
uncommitted experimental test were removed together; original tests remain.
The report preserves its source diff and controls. Refreshing owned archive
input timestamps exposed and corrected stale shared-Cargo-cache reuse in a
negative control; final source snapshots were explicitly rebuilt without
clearing or reconfiguring build caches/profiles/toolchains.

The benchmark review found a lossy-error parity omission; selected index/kind/
native status/transport are now asserted and compared, with a reproducing
negative control. Four additional wire-parser tests cover compound/coalesced
messages and interim STATUS_PENDING replies; all seven runner/parser tests pass.
Catches for incomplete/dropped packet captures prevent false wire parity.
Primary requests and terminal responses match, while scheduler-dependent pending
responses are recorded separately.

Final review independently verified source/binary hashes, all 243 paired rows,
intervals, allocation parity and control cardinalities. A packaging finding was
fixed by explicitly staging both required lockfiles; every frozen manifest entry
is staged with matching hashes. No review findings remain. The generated patch
alone has an exact-path whitespace attribute because its context spaces are data.

Stage-6 source and evidence are local commit `0b1ef93`.

## Stage 7: guide and disposable adoption

The public guide describes standard causes, native/provenance inspection,
explicit IndexedError extraction and non-Clone ownership. Historical published
VecResult instructions are clearly superseded where they describe VfError.
Stale deleted-accessor instructions found in review were corrected.

The [adoption record](migrations/std-error-adoption.md) records exact source
revisions and source-only patches for disposable coreutils/findutils snapshots.
Both patches apply to matching originals and reproduce the tested source.
Coreutils passes 440 library tests (one existing ignored control), rm/rmdir
compilation, the multicall build and two round-robin CLI regressions. Findutils
passes all 286 library tests and actual activation/metadata/pruning/side-effect/
permission-error parity controls. Git and rsync actual builds and C consumer
suites pass against the rebuilt SDK; rsync runs all 11 fixture/fallback tests.
Active checkouts/pins under ~/gh are unchanged. Re-review found no issues.

## Final current-source verification

After refreshing only owned current-source input timestamps, strict workspace
all-feature/all-target Clippy, the complete standard Rust runner and smoke gate
pass. The initial final descriptor run had one STALE_CLIENTID setup failure
before read planning. The exact failed case passed separately under both minor
versions, then the unchanged parallel seven-test selection passed under both.
The initial failure is retained in the work logs rather than reported as green;
no session setup, retry or service behavior was changed.

All three required real-ring controls pass. Isolated Python extensions were
rebuilt again from current vnfs02: fsspec passes 91 tests; nfs4fs 2,042 with
544 fixture-dependent skips; vsmb client three with one skip; vsmbfs four with
161 fixture-dependent skips. Skips and thin zero-test entry packages remain
excluded from coverage. Benchmark/profile Clippy and all seven observer/runner
regressions pass. Staged source/hash/lockfile inventory and whitespace checks
pass. Existing and new historical source manifests remain byte-identical.

## Follow-up read optimization and future release gates

The seven-stage implementation is local through commit 91d9798. The earlier
performance question concerned both healthy allocation growth and POSIX latency.
The [follow-up report](benchmarks/error-integration/read-optimization-2026-11-14.md)
preserves the earlier results and records a new complete 27 × nine-pair run.

Profiling located extra successful-read work in PosixIo::read_owned. A forced
fresh-result allocation showed no clear latency win and was discarded. Dense
local reads now avoid eager placeholder/index storage until preflight rejects an
input. Single-chunk SMB reads move the already-owned decoded payload rather than
allocating and copying it. The measured 64 × 4 KiB healthy cohorts now save one
allocation for POSIX/ring and 63 net allocations for SMB; NFS remains equal.
All five have lower peak-live Rust memory and zero retained delta. Independent
syscall, NFS request-byte, SMB wire and real ring counters retain work parity.

Healthy POSIX still has a roughly 3% paired median cost in the new run, with a
wide interval including parity and near-2× outliers in both variants. This is
investigated and explicitly retained, not declared eliminated. The no-extra-
healthy-allocation/work gate is met for the measured scenarios; timing and
native/opaque failure costs remain documented limitations of the chosen API.

New tests preserve dense/sparse/all-invalid mapping, malformed cardinality,
zero/positive length, positional/current offsets and cleanup across 60 local
combinations. The SMB live regression checks empty/short/EOF/exact-chunk and
multi-chunk results. Neither original coverage nor failure-prefix/recovery
controls were removed. Read-only native and network reviews found no issues.

Current-source follow-up verification passes strict workspace all-feature/all-target
Clippy, the complete standard Rust runner, required live SMB library/integration
tests (77 and 45 passed; two and one existing ignored controls excluded), all
three required real-ring controls, and the smoke runner. The independently
rebuilt C SDK passes ABI-v3 and current/published-old/missing-library fallback
checks plus NFSv4.1/v4.2 outcome conformance. All seven benchmark observer tests
pass. The independent evidence review recomputed every timing interval/table
and checked source/binary/trace hashes and work parity, with no findings.
These checks are local verification, not public CI.

Optional SMB restart/DFS and non-Linux runtime coverage remain omitted, as
reported. Future publication/promotion requires separately authorized same-main
CI and all four public-package port/ABI gates; local checks are not public CI.
Existing server fixtures, host mounts/services, shared environments and build
caches/profiles remain unchanged. The deferred NFS bitmap decoder stays excluded.


## Coordinated publication — 2026-11-15

The source commits were merged into main. The P2 category-preservation fix is
2d271b1; release metadata and CI-specific documentation corrections culminate in
c6c8fd6. Exact-commit canonical CI and all four-port/ABI checks passed before
publication. [Publication evidence](migrations/std-error-publication-2026-11-15.json)
records the registry versions/checksums, release workflows and actual port SHAs.

Published Rust: vnfs 0.0.36; vfsi-core/sync/local/POSIX/io_uring/NFS/SMB 0.3.0;
vfsi-c 0.3.20, preserving C ABI v3. Python: nfs4fs 0.3.16 and vsmb 0.1.10.
Unchanged native system bindings and pure-Python helpers retain their versions.

All four ports now pin the published artifacts, with explicit standard-error
migration in the Rust applications. Their local tests, required copy fallback
controls, published SDK parity tests and read-only reviews pass. Port CI is
tracked on the exact pushed commits. The compatibility registry now points at
those commits and applies no transitional source patches. The previously
recorded performance costs and omitted-platform limits remain applicable.
