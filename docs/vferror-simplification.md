# Replace VfError with std::io::Error after VecResult

Status: implemented, reviewed and published in vnfs 0.0.36 / vfsi-* 0.3.0.
See [publication evidence](migrations/std-error-publication-2026-11-15.json).
All seven stages were implemented and reviewed locally. Follow-up read
optimizations remove the measured extra healthy allocation; remaining timing
costs are documented in the
[follow-up report](benchmarks/error-integration/read-optimization-2026-11-14.md).
Future mirror promotion still requires its same-commit gates. See the
[migration progress](vferror-migration-progress.md).
Date: 2026-11-14.
Baseline: vnfs02 at 70bea6a.
Decision: owned std::io::Error is the public cause; lossy helpers use IndexedError.

## Objective and explicit decisions

Remove VfError rather than retaining a smaller custom public cause type.
Prefer the familiar standard I/O error API while retaining exact native facts,
source diagnostics, per-input execution certainty and confirmed progress.

The Rust API is not stable. Constructor signatures, error aliases, helper return
types, Clone/Eq bounds and diagnostic behavior may change. Do not keep VfError as
a compatibility alias, newtype, deprecated wrapper, or boxed payload inside
io::Error. Retire its index field, ErrorContext representation and public
constructor/accessor machinery.

The target aliases are:

```rust
pub type VecResult<T> = std::io::Result<Vec<OpResult<T>>>;
pub type OpResult<T> = Result<T, OpError>;
```

The public return contracts are:

| Surface | Return type | Attribution and execution details |
| --- | --- | --- |
| Canonical vector operation | VecResult<T> | Slot position identifies each input; OpError retains certainty/progress |
| Scalar operation or helper | std::io::Result<T> | No input index |
| Lossy multi-input helper | Result<T, IndexedError> | Optional selected input index; no execution-state or progress guarantee |

Use ErrorKind for ordinary category matching. Callers needing exact native status
or transport provenance use the small inspection functions described below;
callers needing execution certainty or confirmed progress inspect OpError before
extracting its standard error. Do not infer safe retries from ErrorKind alone.

For example, `Vfsi::vread` returns `VecResult<ReadResult>`, expanding to
`Result<Vec<Result<ReadResult, OpError>>, std::io::Error>`. The outer standard
error describes whole-call rejection; each accepted input has its own success or
`OpError` in the vector. This change does not impose sequential execution or stop
healthy siblings when another input fails.

The selected design uses `std::io::Error` directly, not the minimal index-free
`VfError` prototype from the comparison. `ErrorKind` remains a category rather
than a replacement for the complete error: native status, original source and
execution details must survive in their designated owners below.

Existing conventional application Result<T>/Error reexports may refer directly
to std::io::Result<T>/std::io::Error where useful. They must not conceal another
custom cause type. Remove the VfError-specific name and its reexports.

Keep OpError for certainty/progress and one separate IndexedError for selected
input attribution after lossy collection. Do not introduce BatchError, Stop,
ExecutionProgress, a generic error hierarchy, temporary *_outcomes APIs, ordered
execution, or a public error-extension trait reproducing the old VfError API.

## Evidence behind the decision

The [measured comparison](benchmarks/error-representations/README.md) and
[raw results](benchmarks/error-representations/results-2026-11-14.json) compare
isolated prototypes on aarch64/Rust 1.93.1. These are not integrated migration
results or stable layout promises.

| Representation | Cause | OpError | Void-result slot | Read-result slot |
| --- | ---: | ---: | ---: | ---: |
| Indexed VfError baseline | 32 B | 48 B | 48 B | 48 B |
| Minimal index-free VfError prototype | 16 B | 32 B | 32 B | 40 B |
| Owned io::Error prototype | 8 B | 24 B | 24 B | 40 B |

The healthy POSIX/io_uring controls were near parity. A 64-input synthetic
all-NFS-failure cohort took about 1.56 microseconds with the minimal cause versus
4.16 microseconds with owned io::Error. Bare errno/category errors allocated
nothing in both prototypes. Typed NFS/SMB payloads required two allocations
totaling 32 bytes with io::Error on that compiler, versus none with the minimal
custom cause.

This is an accepted tradeoff for a simpler public API, not a universal performance
win. The new native-failure allocation cost must appear in performance evidence.
Neither the small cause size nor ErrorKind conversion guarantees zero total
error-path allocation.

The Arc-backed alternative reduced cloning/fan-out cost for an already-created
shared cause, but added an allocation when creating each independent error.
It is not the default public representation chosen here.

## Responsibilities and target types

| Information | Owner |
| --- | --- |
| Portable category | io::Error::kind() / std::io::ErrorKind |
| Actual OS errno | io::Error::raw_os_error() for an unwrapped OS error |
| Exact NFS/SMB status and namespace | Small private typed payload |
| Transport provenance | Private typed payload, independent of ErrorKind |
| Operation/path/message/original source | Private diagnostic payload/context |
| NotAttempted / Failed / Unknown | OpError |
| Confirmed read/write/stream progress | OpError |
| Identity of an accepted input | Position in VecResult |
| Selected input after lossy collection | IndexedError.index |
| Backend wire position | Submission/response metadata, not logical cause attribution |

```rust
pub struct OpError {
    cause: std::io::Error,
    state: FailureState,
    progress: Option<Box<IoProgress>>,
}

pub struct IndexedError {
    index: Option<usize>,
    cause: std::io::Error,
}
```

Both wrappers own their cause. cause() borrows &io::Error; into_cause() moves the
io::Error; into_parts() moves the fields without reconstructing the cause or
cloning an owned read prefix. Do not replace these fields globally with
Arc<io::Error> to preserve old Clone derives.

Keep existing FailureState and IoProgress semantics. Their independent Clone,
Copy or equality support need not be removed merely because causes change.
OpError and IndexedError do not require Clone/PartialEq/Eq. Tests and callers
compare semantic fields or match outcomes instead of requiring equality/cloning
of arbitrary errors.

### Private detail storage and the small inspection surface

Use small typed payloads suitable for io::Error::new, with safe owned Send+Sync
source storage:
- A compact native-status payload for NFS/SMB failures.
- A transport payload marking provenance, optionally retaining a source/message.
- A contextual payload only where operation/path/message/source diagnostics
  actually need to be added.
- A shared-source payload only for explicitly shared failure groups.

Do not force bare protocol statuses to allocate a large maximum-size diagnostic
struct. Store rich context inline within its already-boxed payload where practical
rather than adding a redundant context box.

Retain StatusCode and TransportKind as small typed views. Provide the minimal
read-only inspection functions needed by Rust callers and bindings, such as
native_status(&io::Error) -> Option<StatusCode> and
transport_kind(&io::Error) -> Option<TransportKind>. Keep construction, context
attachment and additional binding diagnostics in the existing hidden internal
support layer. Do not add a public replacement cause type or broad extension trait.

Resolve details through direct OS errno and library-owned known payload/wrapper
types. Do not parse messages or walk arbitrary foreign source chains without
bounds looking for status/attribution. Foreign errors with unknown payloads remain
valid io::Errors; absence of recognized metadata means unknown, not invented data.

Remove the old public ErrorDomain if its only role is a derived view now covered
by StatusCode, TransportKind and ErrorKind. Bindings may derive their own wire
domain tags. Remove VfError::raw_code, to_errno, context/index methods and bespoke
Display/equality implementations; move needed errno/wire translation into the
existing adapter/internal support code without duplicating full mapping tables.

### Construction and conversion rules

| Existing cause construction | Target |
| --- | --- |
| VfError::from_io(index, error) | Move the original io::Error; add context only when needed |
| VfError::errno(index, real_os_errno) | io::Error::from_raw_os_error(real_os_errno) |
| VfError::from_kind(index, kind) | io::Error::from(kind) |
| VfError::unsupported(index) | io::Error::from(ErrorKind::Unsupported) |
| VfError::nfs / smb / from_status | Private typed native factory, mapped to the correct ErrorKind |
| VfError::transport / transport_with_kind | Private typed transport factory |
| VfError::invalid_reply | InvalidData with explicit InvalidReply provenance when required |
| VfError::with_context | Private context attachment preserving kind/native facts/source |
| VfError::index / with_index / map_index | Slots, explicit maps or IndexedError; never an io::Error payload |

Do not pass NFS status, NTSTATUS, or compatibility sentinel values to
from_raw_os_error. Map the category and retain the exact native value separately.
ErrorKind alone is not enough to preserve native details or classify transport
provenance. A local TimedOut error is not automatically an RPC transport failure.

Context attachment must preserve the original kind, source and recognized native
details. A known existing contextual payload should be updated/reused rather
than accumulating an unbounded chain of context wrappers. Wrapping an OS error
may make the outer raw_os_error() return None; the typed inspection helper must
still expose the original errno from a recognized library-owned wrapper.

Library-owned payload/context fields contain no logical input index and must
not embed IndexedError to smuggle attribution back into the cause. Preserve
legitimate original source diagnostics without mining them for input positions.

## Cloning, sharing and ownership

io::Error is not Clone, and an arbitrary payload cannot be safely reconstructed
from kind(), Display text or raw_os_error() alone.

Remove the blanket cloning assumption instead of adding a clone_io_error shim:
1. Move a single owned error into its single outcome, outer rejection, pending
   std_io error, or selected IndexedError wherever possible.
2. When a backend knows a Copy native status/recipe, it may construct independent
   causes for the affected slots. That is deliberate construction, not general
   cloning; retain exact status and provenance.
3. When one opaque/contextual source genuinely belongs to several outcomes,
   share the original source explicitly inside a private shared-source payload.
   Each public slot still owns an io::Error with the correct category and source
   linkage. Never stringify, truncate, or replace the original source with kind().
4. Keep sharing scoped to that failure group. Do not add a global cause cache,
   interner, custom public SharedError type, or Arc around every independent error.
5. Drop all source owners at the appropriate cleanup boundary; sharing must not
   retain handles, sessions or unrelated payload buffers indefinitely.

A SharedCause(Arc<io::Error>) payload inside each owned io::Error still incurs
per-wrapper boxing. The direct Arc cause benchmark did not measure this exact
owned-error representation. Do not advertise its zero-per-clone allocation
result as a property of this selected design. Measure actual integrated fan-out,
including source allocation, per-slot wrappers and consuming conversions.

Fault scripts, cached failures and delayed callbacks also need ownership
decisions: store an explicit native recipe or shared original where appropriate.
Do not impose Clone on arbitrary public errors just to simplify test injection.
This migration must not add replays or extra I/O to regenerate an error.

## Outer rejection, scalar helpers and lossy collection

At a canonical VecResult boundary:
- Err(io::Error) rejects the whole call before dispatch and has no input index.
- Ok(outcomes) provides one outcome per accepted input in input order.
- After dispatch, failures belong in affected slots with proper FailureState.

A whole-call shape/options/aggregate-budget rejection may explain the bad
constraint and useful path context, but has no programmatic offending-input
index. This accepted API change does not justify inventing zero or changing
rejection into execution.

io::Error used outside VecResult may describe scalar or helper-wide post-dispatch
failures. The type, ErrorKind and lack of an index are never retry guidance.

Scalar helpers return std::io::Result<T>. If they reuse a collection helper, they
explicitly discard its attribution with IndexedError::into_cause.
StdIo processes OpError certainty and confirmed progress before moving the
remaining cause into its pending-error state or returning it. Avoid reboxing an
already-correct io::Error simply to convert it to standard I/O.

Lossy multi-input helpers return Result<T, IndexedError>, for example read_batch,
open_batch, getattrs_all and other multi-input *_all/stream/listdir collectors.
The wrapper has at(index, cause), unattributed(cause), index(), cause(),
into_cause(), into_parts(), map_index() and only additional context forwarding
actually needed by helpers. None is unattributed; Some(0) is a known first input.
Display renders attribution; Error::source exposes its contained io::Error.

Do not add an implicit From<IndexedError> for io::Error. Attribution loss must be
explicit. IndexedError carries no FailureState or IoProgress: callers requiring
those details must use VecResult.

Collector policy:
1. Outer rejection becomes IndexedError::unattributed(cause).
2. Malformed cardinality becomes an unattributed InvalidReply error. It may be
   detected after dispatch; it does not mean that nothing ran.
3. Otherwise select the first failed slot in input order and move its cause into
   IndexedError::at(index, cause), deliberately dropping other execution details.
4. Close every successfully opened sibling, or retain failed cleanup ownership,
   before returning an open_batch error. Preserve the primary error.

Multi-input helpers outside VfsiExt must not hide indices in causes:
- Transfer copy_items/move_items and cohort path mapping use IndexedError for
  original source/root attribution.
- TreeBuilder saved/declaration errors use IndexedError; root-wide errors are
  unattributed.
- Truly single-root helpers return io::Result<T> with useful path/operation text.

## Repository inventory and attribution audit

Primary changes:
- crates/vfsi-core/src/error.rs: replace VfError with private payload/factory/
  inspection support; retain only needed status/provenance views.
- crates/vfsi-core/src/outcome.rs: owned io::Error causes and move-based methods.
- crates/vfsi-core/src/internal.rs: collectors, subgroup errors and fan-out.
- crates/vfsi-core/src/api: aliases, helper signatures, budgets and std_io boundary.
- crates/vfsi-sync/src/client and backend/helpers: ownership and slot remapping.
- crates/vfsi-local, vfsi-posix, vfsi-uring: preserve direct syscall errors.
- crates/vfsi-nfs/src/client, completion and recovery: typed native/transport
  conversion, submission maps and nonreplay handling.
- crates/vfsi-smb/src: exact NTSTATUS, command validation and deferred ownership.
- crates/vnfs/src/auto and helpers: root/route/declaration attribution.
- bindings/c/src/lib.rs and io_outcomes.rs: explicit-index result conversion.
- adapters/vfsi-python/src/lib.rs: exception class/errno/native metadata conversion.
- Fault/test support, examples, fuzz targets, benchmarks and detached adapters.
- Actual Rust port callers under ~/gh; use disposable source adaptations.

Classify each existing index before removal:
1. Accepted logical input: vector position.
2. Selected lossy error: IndexedError, remapped once to original position.
3. Whole-call rejection: no input attribution at the canonical interface.
4. Wire position: backend metadata mapped through the actual submission plan.
5. Adapter error: explicit slot, wrapper, validation-loop index or ErrorMap key.

Keep low-level RpcError and its wire positions where independently needed.
Convert it to io::Error at the portable boundary without making the position part
of the portable cause contract. Remove indexed VfError/RPC bridge helpers only
after mapping callers explicitly; do not reinterpret wire offsets as input indices.

## Implementation sequence: seven reviewable commits

Keep every implementation commit buildable with its focused tests and review.
No intermediate commit is a publication target. Preserve existing unrelated work
and test coverage; no temporary public *_io_error aliases or legacy VfError shim.

### 1. Characterize semantic facts and ownership requirements

Reuse the existing comparison as the representation decision, not proof of an
integrated migration. Add/extend error_contract, vector_results and allocation
coverage for:
- Direct OS errno/category and exact NFS/SMB status/provenance.
- Original opaque sources, context attachment and category preservation.
- Independent outcomes, progress, uncertainty, cardinality and cleanup.
- Slot/wrapper attribution, index zero/unknown and remapping.
- Move semantics, source lifetime and group fan-out without arbitrary Clone.

Inventory Clone/Eq requirements and all shared-error broadcasts, cached failures,
pending errors and fault scripts. Decide ownership for each before changing the
public type. Record baseline layouts/allocations; update assertions to semantic
fields rather than weakening tests when Clone/Eq disappear.

Archive the measured experimental source/binary hashes and raw results before
updating any benchmark that references the removed VfError API. Preserve old
evidence rather than silently making its source links describe another build.

### 2. Prepare slot mapping and owned-error propagation

Refactor attribution to explicit submission/route/root maps and adapter arguments,
while the old representation still exists internally. Avoid changing execution
order, independent dispatch, retries, successful-prefix interpretation or budgets.

Remove unnecessary error cloning where ownership can be moved already. Prepare
explicit known-native recipes or scoped shared-source handling for real fan-out.
Inspect eager error construction and avoid new success-path allocations.

Exit gate: mapping and lifetime tests pass; no canonical path requires logical
attribution stored in a cause, and every remaining error broadcast has a design.

### 3. Implement private standard-error construction and inspection

Add the final small typed payloads/factories and minimal read-only inspection
functions for io::Error. Reuse the existing status-to-kind mapping; do not
maintain two copies of the full table during transition.

Support direct OS/category errors, typed protocol/transport causes, reusable
context attachment and explicit sharing of opaque group failures. Construction
and context remain index-free. Keep old type compatibility code private and
temporary only where needed for buildable transition; never wrap VfError as the
new standard payload.

Test exact native domains, context/source downcasting, unknown foreign payloads,
wrapping behavior, source lifetime and Send+Sync requirements. Do not require
unsafe trait implementations to force unsuitable sources into payloads.

Exit gate: new standard causes preserve the required facts independently of the
old VfError constructors and pass allocation/fan-out tests.

### 4. Cut public causes and lossy helpers over atomically

Switch VecResult, OpError, scalar aliases and backend causes to owned io::Error.
Introduce the one final IndexedError and migrate all lossy multi-input helper
returns plus their root/declaration callers. Move errors through parts/conversion
methods without implicit attribution loss.

Update workspace/detached sources, facade reexports, macros, tests, fault hooks,
examples, fuzz targets, StdIo and the initial C/Python conversion paths together.
This commit must both compile and preserve the tested semantic facts; temporarily
flattening causes to strings just to compile is not acceptable.

Delete VfError, its aliases/constructors/index APIs, clear_input_index and the
obsolete From<VfError> for io::Error conversion. Remove public cause-dependent
Clone/Eq derives and update actual callers. Retire ErrorDomain if redundant.

Exit gate: no live code depends on a custom VfError or an implicit error clone;
scalar/outer causes are standard errors and selected helper attribution lives
only in IndexedError. Existing tests are adapted, not removed.

### 5. Finish bindings and remove legacy numeric-code assumptions

Audit former raw_code and old-accessor call sites against kind(), raw_os_error()
and the small native/provenance inspection functions. Removed-symbol call sites
must already compile in step 4; do not defer those fixes to this commit.
Kind/provenance alone
does not authorize retry or copy fallback; preserve FailureState decisions.

Keep legacy numeric encoding in C/Python boundary code and retire core synthetic
error constants. Never pass those constants to from_raw_os_error. Use explicit
typed markers/translation for private adapter cases such as same-file errors,
rather than manufacturing native errno.

C ABI v3 binary layouts/optional symbols remain separate from Rust source
stability and need no unrelated redesign. Use the existing unknown-index
representation for unattributed library rejections. Adapter-local validation
and accepted item slots can still supply known indices explicitly.

Python conversion receives an explicit optional index; ErrorMap uses its key.
Preserve the underlying exception category, errno/native domain/code, useful
filename/source diagnostics and exact confirmed-close ownership metadata.
Old outer indices/message text/helper signatures need not remain compatible.

Use one internal semantic mapping source and existing adapter conversion seams,
not a new public cross-language error framework or duplicated full status tables.

Exit gate: bindings derive facts from standard errors plus explicit attribution;
legacy optional-symbol fallback and nonreplay behavior remain correct.

### 6. Validate the integrated design and measured costs

Run actual Vfsi/adapter integration and performance controls, not only the isolated
prototype. Use immutable baseline/final sources with the same VecResult semantics;
do not compare an old aggregate/sequential API with the new one.

Cover healthy/mixed/failure-heavy POSIX/io_uring, void results, selected lossy
errors, known native fan-out and opaque shared-source fan-out. Include NFS 4.1/4.2
and SMB live error/recovery controls. Compare work, bytes, statuses, prefixes,
syscalls/RPCs, SQEs/CQEs/waves and cleanup owners.

Use release builds, actual recorded Cargo compiler identity, unchanged target/
profile caches, warm-up and stable fixture identity. Run at least seven randomized
pairs; keep raw samples and intervals. Separate allocation/traced controls from
uninstrumented timing. Record allocation calls/bytes and peak-live memory;
RSS is only supplementary.

Acceptance:
- No extra healthy-path allocation, I/O, serialization or submission waves.
- Simple kind/direct OS errors do not acquire unnecessary payload boxes.
- Native/custom errors may pay the measured std::io::Error boxing cost; report
  it rather than applying the old zero-allocation VfError requirement.
- No redundant boxing/context layers beyond the chosen payload design.
- IndexedError wrapping/parts conversion adds no allocation just for attribution.
- Explicit fan-out preserves arbitrary sources; its actual owned-wrapper cost
  is measured, not borrowed from the different direct-Arc prototype.
- Layout bounds use measured target values; do not promise all Result slots shrink.
- Investigate repeatable regressions beyond accepted error-path costs and retain
  failure correctness even where the old representation was faster.

Have the final integrated diff reviewed; fix legitimate findings with reproducing
coverage and re-review.

### 7. Document the break and prepare release gates

Document std::io::Error as the cause, ErrorKind as portable category, optional
native/provenance inspection, removal of VfError/Clone/Eq assumptions, explicit
fan-out, IndexedError helper returns and unindexed outer rejection.

Adapt Rust ports in disposable checkouts using temporary path dependencies.
Exercise actual C consumers and detached Python packages. Keep active port
checkouts/pins unchanged until adoption is separately authorized.

Use coordinated versions consistent with this accepted source-breaking API
change and existing release tooling. Record exact reviewed revisions, semantic
tests, public API examples and performance evidence. Do not publish intermediates
or invent package versions.

Writing/implementing this plan does not authorize push, publication, PR creation,
port repinning or mirror promotion. Eventual authorized promotion requires CI and
all four port/ABI checks on the same final main commit.

## Error-handling acceptance matrix

| Scenario | Required result |
| --- | --- |
| Whole-call shape/options/budget rejection | Outer io::Error; no input dispatched or embedded index |
| Bare OS error | Original errno and portable category; no pointless reboxing |
| NFS/SMB status | Exact namespace/code in typed payload and correct ErrorKind |
| Local timeout versus RPC timeout | Category may match; transport provenance remains distinct |
| Opaque foreign cause | Original object/source retained; never stringify to clone |
| One error shared across slots | Explicit ownership/shared-source policy, independent slot states |
| Middle failure with healthy siblings | Same positions; both healthy sides execute |
| Item cause/context | No logical index or IndexedError hidden in payload |
| Short I/O then failure | Exact prefix and certainty retained in OpError |
| Lost/uncached reply | Submitted Unknown, unsent NotAttempted; no replay/fallback |
| Failed OPEN/CLOSE cleanup | Resources closed or retained for deferred cleanup |
| Lossy helper selects slot N | IndexedError(Some(N), io::Error); no prefix/retry claim |
| Lossy helper sees rejection | IndexedError(None, io::Error) |
| Helper discovers malformed reply shape | InvalidReply cause; lack of index does not mean no dispatch |
| Scalar helper unwraps selection | Explicit into_cause; useful diagnostics preserved |
| Adapter item error | Index comes from slot/map/wrapper/validation, never cause mining |
| Standard-I/O pending failure | Move original cause after processing confirmed progress |
| IndexedError source chain | Wrapper exposes io::Error and its retained original diagnostics |

## Verification commands and execution policy

Use established caches/profiles; do not delete coverage or omit relevant features.
These are implementation gates, not claims that migration tests have run.

- Core: cargo test -p vfsi-core --test error_contract --test vector_results --test allocations
- Native: cargo test -p vfsi-sync --features test-faults --test native_client
- Local/ring: explicit affected library/contract targets with fault features;
  ci/test-uring-outcomes.sh must execute its actual kernel rows.
- Bindings: cargo test -p vfsi-c --lib; detached native adapters through the
  existing all-features runner; Python tests must import vnfs02's modified source
  instead of main's installed package.
- NFS/SMB: required live fixtures, deterministic faults and real-wire lost/
  uncached-reply cases. Zero, ignored or missing-fixture cases are not coverage.
- Final broad gate: ./scripts/test-rust.sh; ./scripts/test-ci-local.sh smoke;
  formatting, applicable all-target/all-feature Clippy, doctests, detached
  packages, C header/ABI/optional-symbol checks and four disposable port checks.

Use fresh owned mount/network namespaces if the old host NFS mount is
unresponsive. Do not change host mounts or restart services for this migration.

## Completion criteria

- Public outer/scalar causes and OpError/IndexedError causes are owned io::Error.
- No custom VfError definition, compatibility alias, payload or constructor remains
  in active library/API code; historical measurement artifacts remain immutable.
- Standard payloads/context contain no library-managed logical input attribution.
- OpError owns certainty/progress; slots and IndexedError own attribution.
- Native status, transport provenance, context and opaque source facts remain
  inspectable without message parsing.
- Arbitrary Clone/Eq is not required; each shared failure has an explicit owner
  and measured, correct fan-out policy.
- ErrorKind/native construction and small inspection support replace the old
  bespoke public error API; binding wire codes stay at adapter boundaries.
- Actual integration tests and performance gates pass with accepted boxing
  costs reported honestly and no new healthy-path overhead.
- Intentional API changes are documented; implementation completion is tracked
  separately from this design. This plan grants no publication or port-repinning
  authorization.
