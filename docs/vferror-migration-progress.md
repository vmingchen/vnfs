# Standard I/O error migration progress

Baseline: 70bea6a. All changes remain local. The complete design and seven-stage
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

## Remaining full-scope gates

- Finish the stage-5 numeric-code/binding conformance audit with explicit indices,
  ABI v3/optional-symbol compatibility and confirmed-close ownership checks.
- Run stage-6 full local integrations and paired actual-Vfsi performance controls,
  including peak-live memory, live NFS/SMB, real ring work counts and fan-out costs.
- Finish stage-7 documentation and four disposable port checks. Do not repin
  active ports, push, publish, open PRs or promote mirrors.

Existing server fixtures/build caches remain unchanged. The deferred NFS bitmap
decoder finding remains excluded.
