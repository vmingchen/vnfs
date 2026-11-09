# Per-operation results for vectorized Vfsi calls

Status: source migration implemented; coordinated release in progress. The shared
result/error foundation, metadata queries, canonical read/write APIs, handle
lifecycle and namespace vectors, directory paging/visitor APIs and file
streaming are implemented. Local compatibility checks pass. Performance
measurements and release coordination are described below.
The shared return type is **VecResult**. Registry publication and port pins are
coordinated after source review; ecosystem provenance records released artifacts.

## Latest verification checkpoint (2026-11-09)

The operation-family sections below are historical checkpoints; their open-gate
notes describe evidence available at that point, rather than the latest status.

- SMB deferred CLOSE now retains its original share/session and never replays
  an attempted or uncertain close. Live coverage includes identifier reuse,
  session replacement, malformed reply commands, and unsubmitted owners.
  Unsupported create modes fail only their own OPEN input; valid siblings run.
- The complete Python gate passes 2,559 nfs4fs tests (four skips), 35 shared
  fsspec tests, three SMB client packaging tests (one live-fixture skip) and
  four SMB filesystem packaging tests. Required managed Samba coverage also
  passes all 169 adapter tests.
- NFS RPC-count coverage now accounts explicitly for OPEN-adjacent GETFH.
  Twenty short files cross the managed server's 100-op limit: cold writes
  may use four RPCs and warmed adaptive writes three; reads need two data
  waves. This is an acknowledged wire-cost increase over the old identity-
  lossy path. Existing test functions and content checks remain. New coverage
  requires sixteen files to fit one write RPC, bounds cold discovery separately,
  and retains the original metadata/removal scaling checks.
- Four-port compatibility is exercised locally without changing published port
  worktrees. A disposable coreutils adaptation replaces aggregate callers with
  getattrs_all/listdirs_all/removes_all, including du's single-root
  visitor. It passes 440 library tests (one existing ignore), removal checks,
  the multicall build and both opt-in round-robin CLI tests. Findutils needs no
  source changes and passes 286 library tests, its build and CLI parity tests.
  Existing Git and rsync port binaries pass their dynamic-library parity and
  fallback tests against the migrated C library. C ABI v3/optional-symbol
  tests pass; its Rust adapter passes 71 tests (six manual/fixture ignores).
  The disposable Rust port lockfiles resolve this worktree, not crates.io.
- The complete server-independent Rust runner passes, including core/native
  contracts, public API and helper regressions, doctests, C adapter tests and
  the detached Python-native unit suite (59 passes, one manual ignore).
  Strict root/detached formatting and all-feature/all-target Clippy pass.
  The latest broad Rust test phase takes 64 seconds; the required smoke gate takes 28 seconds. Live server checks are
  reported separately rather than implied by this server-independent gate.
- Ten actual crate archives include the new ownership/conformance modules
  and pass an all-features check in a disposable workspace patched to those
  extracted archives (19.92 seconds). Package metadata checks pass. Normal
  prepublication package verification cannot resolve unpublished 0.2.0
  dependencies through cbindgen's nested Cargo metadata process; the staged
  check verifies the actual archive contents without publishing or modifying
  registry configuration. Reverify normally once the cohort is published.
- Optimized paired POSIX/io_uring success and mixed-failure measurements record
  wall time, CPU, allocation calls/bytes, allocator peak-live storage and RSS.
  See [the raw report and reproducible harness](benchmarks/vector-results/README.md).
  io_uring retains identical submissions, completions and waves and uses four
  fewer allocations per cohort. The report also records timing/storage costs;
  it does not claim universal speedup. POSIX mixed failures now perform the
  healthy suffix syscalls that the baseline skipped.

- The std_io adapter acknowledges confirmed positive write prefixes, retains
  the original per-input failure for the next operation, and requires an
  explicit seek to resolve its cursor after that failure is observed. write_all
  also reports a failure after a full acknowledged payload without retry.
  The native-client target passes 130 tests, including three new regression
  tests for prefix acknowledgement and pending-error behavior.
- SMB retained-file operations validate the original tree/session identity
  before dispatch. A real reconnect regression forces FileId reuse and proves
  stale reads/writes/metadata/truncation/flush issue no RPC while current
  siblings succeed. The session-generation check is an atomic load, without
  adding a per-item SDK crypto lock.
- Rust package versions are prepared as a coordinated breaking cohort:
  vfsi-core/sync/local/posix/uring/nfs/smb 0.2.0 and vnfs 0.0.32.
  C/Python follow-up versions retain their existing external conventions.
  [Application migration notes and patches](migrations/README.md) record exact
  port bases, dependency pins and safe COPY fallback policy.

Remaining release gates: review the final uncommitted migration, create reviewed
source and port commits, publish the coordinated packages, and require
same-commit CI plus all four port compatibility jobs before mirror promotion.
Local tests against disposable adaptations do not certify old published Rust
port pins against a breaking API. C ABI v3 and legacy Python conventions stay
unchanged. No versions are published and no port dependency is redirected
persistently. The prepublication checkpoint below precedes coordinated commits and publication.

Remote paired controls are complete against the final source:
NFS and SMB healthy runs retain their RPC/command counts on SDK 0.42.1.
NFS 4.1 and SMB timing intervals span parity; NFS 4.2 shows a small cached-read
improvement in this control. NFS allocation calls decrease 17.1%. Loss-checked SMB
captures include the entire OPEN/CLOSE interval. Mixed remote failures now
execute healthy siblings instead of rejecting the whole vector; their larger
work totals are reported explicitly. See the benchmark report for raw pairs,
memory costs and limitations.

## Prepublication review

[The independent review and fix record](migrations/review-2026-11-09.md) covers
five confirmed findings and their regression tests. All were addressed and
re-reviewed before the coordinated release. Main's SMB SDK 0.42.1 is integrated;
the release checks and remote performance control use that SDK cohort.

## Final local verification commands

The latest source passes the following gates. Reuse the environment and native
libraries documented by the repository runners; require live fixtures rather
than treating skipped live cases as evidence.

| Gate | Selection and result |
|---|---|
| Rust formatting, Clippy and server-independent tests | scripts/test-ci-local.sh check fast; check and fast pass, fast 64 seconds, including detached native adapters and doctests |
| Required smoke | scripts/test-ci-local.sh smoke; passes in 28 seconds |
| Native ownership and standard I/O | vfsi-sync native_client: 130 tests pass |
| NFS protocol / public API | Required minor-2 backend unit/fault, integration and exclusive-symlink targets; public vnfs NFS suite; live cleanup regressions on minor 1 and 2 |
| SMB protocol / reconnect / COPY | Required managed Samba: 69 unit passes/two ignores, 45 integration passes/one optional ignore, 169 Python adapter tests; final exact stale-session regression passes |
| Python NFS and shared compatibility | Python runner: 2,559 nfs4fs passes/four skips, 35 shared fsspec passes, three SMB client packaging passes/one live-fixture skip, four shim passes |
| C / Git / rsync | Rebuilt vfsi-c 0.3.16; ci/ports/check-c-abi.sh, Git and rsync port runners pass; unchanged public headers and ABI v3 |
| Coreutils adaptation | 440 selected library tests/one existing ignore; removal checks, multicall build and both round-robin CLI regressions pass against local vnfs 0.0.32 |
| Findutils adaptation | 286 library tests, build and CLI parity pass against local vnfs 0.0.32 |
| Packages / metadata | Ten actual archive contents checked together with all features; package/ecosystem metadata validators and git diff --check pass |

These are local checks, not all platform CI, sanitizer, Kerberos or remote
release gates. Existing Python skips and manually ignored tests are not passes.
Application source changes are reviewable patches, not silently modified
published port pins.

All twenty canonical vector signatures return VecResult: vstatfs, vgetattrs,
vfsync, vsetattrs, vsymlink, vreadlink, vhardlink, vopen_dirs,
vremove_dir_contents, vopen, vread, vwrite, vclose, vmkdir, vcopy, vremove,
vrename, vlistdirs, vlistdir_pages and vstream. Scalar capabilities/route
discovery remain scalar. No temporary _outcomes family is introduced.

## Implementation status

- Core VecResult/OpResult, OpError, FailureState and IoProgress are implemented
  with portable/native exports. Inner causes have no legacy input index;
  outcome position supplies identity. Error progress is boxed only when present,
  and owned prefixes move between adapters without cloning their payloads.
- Canonical Vfsi::vgetattrs and vstatfs, their native hooks, facades and mounted
  routes preserve independent outcomes on Local, NFS and SMB. Sparse attribute
  masks, final-symlink handling and retained-handle identity remain intact.
  Invalid handles are isolated while valid siblings retain batching.
  VfsiExt::getattrs_all and statfs_all provide explicit all-success collection.
- Canonical Vfsi::vread now returns VecResult directly for whole-file, owned
  range and caller-buffer inputs. Native owned/buffered/whole-file hooks and
  FsClient entrypoints preserve the same contract. No temporary outcomes method
  family or unsupported migration stub is introduced.
- Portable mixed-read assembly and mounted route groups retain known results
  when another phase or route fails. Later independent phases remain eligible.
  Missing replies become Unknown only in the omitted slots; excess replies
  invalidate their affected group. Aggregate request-budget rejection remains
  before dispatch. Confirmed error prefixes charge the returned-data budget.
- Local descriptor reads distinguish preflight rejection from syscall failure.
  io_uring continues later independent waves after ordinary read errors, drains
  published operations and retains its fatal-engine ownership policy. Owned
  reads retain zero scratch copies; borrowed reads keep their existing scratch
  copy policy. Submission/completion/wave assertions remain exercised.
- NFS raw descriptor and merged-path READ/GETATTR drivers preserve successful
  COMPOUND replies, resume only proven unexecuted suffixes, and distinguish lost
  submitted replies from untouched inputs. Paired descriptor completion is shared
  with statistics GETATTR. Short-read repair keeps contiguous prefixes and later
  validated siblings. Native read recovery resumes failed ranges without
  replaying complete data or healthy siblings.
- NFS phased reads consume one rich native OPEN batch, retain acquired
  descriptors across sibling failures and defer unconfirmed temporary closes.
  OPEN preparation now preserves submitted-versus-untouched uncertainty
  directly, without a frontend prefix-replay loop. Full merged READ/WRITE
  compounds capture GETFH immediately after OPEN, so later data or inline CLOSE
  failures retain the exact object for close-only recovery and deferred cleanup.
  Successfully closed owners require no cleanup-vector entries or handle copies.
- SMB metadata and READ vectors preserve join_all concurrency. Raw/compound
  chunk errors retain confirmed bytes, including when CLOSE is missing or fails.
  Selective reconnect recovery retains known siblings and resumes partial reads.
  Temporary resources keep deferred cleanup ownership.
- Native whole-file accumulation is shared and bounded. Successful data and
  error prefixes share one logical payload budget; exact-limit EOF uses a bounded
  one-byte probe. NFS retains its adaptive compound windows. Local path cohorts
  pin successfully opened descriptors through all chunks, continue independent
  siblings and close acquired handles. Measured small-chunk adoption and
  large-file allocation policies remain covered by regression assertions.
  Specialized native whole-file overrides now implement the rich hook directly
  rather than depending on an aggregate stream callback.
- VfsiExt::read_batch invokes the vector once and deliberately collects all
  successes or selects the first error by input order. Short reads remain short.
  Existing scalar/whole-file conveniences use this explicit lossy policy.
  Ordered streaming callbacks consume one completed prefetch vector without
  replaying a healthy prefix after a sibling read failure. StdIo advances only
  confirmed buffered bytes when a read also fails.
- C ABI v3 layouts and existing Python result/exception conventions remain
  unchanged. Their compatibility adapters deliberately select legacy errors.
  Python metadata, ranged, descriptor and whole-file reads consume independent
  native results directly instead of filtering and replaying successful inputs.
- Public conformance covers mixed read forms, missing paths, closed/foreign
  handles, untouched rejected buffers, empty vectors and no-dispatch budget
  rejection on POSIX, mounted routing, io_uring, NFS and SMB. Native tests cover
  malformed replies, owned/buffered prefixes, shrinking cohorts, wire-loss
  boundaries, cleanup and selective recovery.
- Core read allocation-count limits and native backend-storage reuse remain
  unchanged. Mounted routing retains exactly its outcome vector and one backend
  request vector beyond direct dispatch. Its byte bound now measures the richer
  required outcome slots. Paired success/failure wall-time, CPU and memory
  benchmarks remain an open verification gate; batching assertions alone do not
  prove that gate complete.
- Canonical Vfsi::vfsync and native synchronization hooks now return VecResult.
  Invalid/foreign handles retain NotAttempted slots while valid siblings share a
  backend batch. VfsiExt::sync_batch explicitly selects one legacy error after
  all outcomes finish; scalar sync helpers delegate to the vector engine.
  POSIX attempts each syscall, io_uring retains independent waves after ordinary
  sync failures, and SMB submits concurrent FLUSH jobs. NFS preserves its
  FILE_SYNC4 write policy and requires no extra COMMIT round trip for sync.
- Synchronization tests cover success/failure/success, pre-dispatch rejection,
  wire-status failure, reply loss, original identities, no replay and poisoned
  executor reuse. The io_uring publication-loss regression specifically prevents
  untouched synchronization inputs from becoming legacy zero-count successes.
- Canonical Vfsi::vclose and the HandleBackend close hook return VecResult.
  Successful inputs retire their cleanup owners even across sibling failures;
  rejected and uncertain inputs remain armed. Already closed inputs succeed
  without dispatch, and foreign owners do not suppress valid siblings.
  VfsiExt::close_batch selects a legacy error; consuming close_files delegates
  to it. Retained singular try_close uses the same native vector hook.
- NFS descriptor CLOSE uses paired completion decoding: known successes are
  not replayed, and only proven unexecuted suffixes resume after a wire-status
  error. SMB keeps concurrent CLOSE jobs. Pre-dispatch rejection, status error,
  dropped reply, missing/excess results and invalid progress are tested,
  including real servers and public POSIX/mounted/io_uring/NFS/SMB clients.
  C ABI v3 item results retire each known success, including non-prefix
  successes; Python retains its legacy strict close convention.
- Native raw OPEN hooks now return VecResult on Local, NFS and SMB.
  The temporary vopen_outcomes_impl seam is removed; native backends implement
  the canonical hook explicitly. The shared open_raw_batch function provides
  deliberately lossy strict compatibility and cleans distinct valid handles.
  Typed native/public vopen now returns per-input owned handles directly.
  VfsiExt::open_batch deliberately selects one error and closes successful
  siblings in one vector; singular helpers and the existing open builder compose
  that policy. There is no singleton scalar shortcut in public vopen.
- NFS OPEN preserves earlier handles, exact submitted/untouched ranges, read-only
  input rejection, protocol-proven suffixes and independent exclusive-create
  finalization. Non-creating O_TRUNC now performs its required dependent SETATTR
  with the acquired OPEN stateid, preserving other files and existing modes.
  A live reproducer failed before this fix and passes afterward.
  Known symlink rejections repair only their own input, including
  mutating opens whose rejected OPEN made no mutation. Unconfirmed cleanup
  resources remain backend-owned; healthy siblings keep independent open state.
- SMB OPEN keeps concurrent CREATE jobs and its existing alias scheduling
  policy. Both schedules consume the same lazy request/future engine; ordinary
  failures retain later siblings, and fatal serial failures leave their suffix
  unconstructed and NotAttempted. Path/request rejection, wire failures,
  per-input reply loss and local registration failures retain accurate outcomes.
- Native conformance and fault tests cover successful owners on both sides of
  a failed input, post-reply cleanup, deferred registration cleanup and read-only
  policy isolation. Current C/Python signatures and ABI v3 layouts are retained
  through explicit strict collection; no temporary public outcomes API is added.
- Owned OPEN validation preserves missing slots as Unknown, rejects invalid
  descriptors and impossible byte progress, and quarantines malformed excess or
  duplicate resources under exactly one cleanup owner. Real allocator descriptor
  order avoids a hash allocation on the common path. Mounted groups preserve
  successful handles across another route's failure.
- Streaming consumes one OPEN vector, delivers callbacks only before the first
  failed input, and closes every acquired sibling after stop or failure. The
  previous read-only OPEN-prefix replay helper is removed. Tests verify exact
  dispatch counts, cancellation precedence, malformed ownership and cleanup.
- Write executor preparation is implemented: descriptor writes return independent
  OpResult slots, retain confirmed byte prefixes, and no longer turn untouched
  inputs into zero-byte successes. POSIX attempts later independent syscalls;
  io_uring keeps coalescing and bounded waves after ordinary CQE failures while
  fatal submission loss still poisons the engine and retains in-flight resources.
  Regression tests cover short writes, interrupted syscalls, zero progress,
  success/failure/success, submitted uncertainty and untouched suffixes.
  The native backend hook, partial-write client entrypoints and public
  Vfsi::vwrite expose these outcomes directly. Complete-write scheduling retains
  them across dependency waves as described below.
- Local native WRITE preparation now retains independent outcomes across
  descriptor, path, append/cursor and alias-ordering paths. Invalid descriptors
  and offset overflows are isolated without splitting the healthy descriptor
  batch or cloning its payloads. Missing/excess or impossible executor replies
  preserve trustworthy siblings and reject only the affected completions.
  Ordered executors keep their identity-scan bypass. Fallback syscalls share the
  counted-write loop, preserve real short-write prefixes and advance cursors only
  by acknowledged bytes; redundant descriptor cloning and positional metadata
  queries are removed. Path offset arithmetic and cursor-only modes are checked
  before CREATE/TRUNCATE.
  Regressions include real partial pwrite/EFBIG in an isolated child process.
  The canonical native hook now returns this rich engine directly.
- NFS raw descriptor WRITE now uses paired outcomes, validates acknowledged
  counts/commit modes, isolates oversized/overflowing wire inputs and resumes
  only the known unexecuted suffix after a server status failure. The logical
  descriptor accumulator isolates invalid inputs, retains contiguous byte prefixes and cursor progress, continues
  independent later waves after ordinary item failure and stops safely after
  infrastructure loss. Required-server tests verify exact compound counts,
  pre-dispatch rejection, reply loss, adaptive-retry rejection and absence of
  replay. Dispatch tracking keeps post-dispatch infrastructure failures inside
  input slots even when a rejected compound produced no successful prefix.
  The canonical native hook now returns the private rich dispatcher directly;
  the remaining special-stateid/cleanup recovery audit is recorded below.
- NFS merged-path WRITE preparation no longer replays a completed batch when
  its trailing special CLOSE fails. OPEN-adjacent GETFH pins the actual
  temporary object/stateid for close-only recovery and existing deferred ownership,
  including when data fails or the path is subsequently unlinked. Confirmed writes stay completed; only unfinished
  byte suffixes and untouched inputs resume. Continued paths clear create/
  truncate flags, later untouched inputs keep theirs, and shared mutation groups
  preserve alias/truncate ordering. Logical byte counts use usize without u32
  saturation. Healthy cohorts retain their RPC batching; minimal operation
  budgets use a separate CLOSE when trailing CLOSE cannot fit. Required
  server regressions bound transmitted payload, verify deferred cleanup and
  exercise aliases and an eight-operation ceiling.
- NFS merged-path WRITE now retains one outcome per logical input, including
  acknowledged prefixes before later wire failures and short-repair errors.
  Ordinary failures continue independent inputs without replaying mutations.
  Offset overflow and directory-only path rejection isolate their own slots;
  valid siblings keep one healthy compound. Short-repair validation retains only
  a trustworthy contiguous prefix and rejects impossible count/commit replies.
- The NFS private WRITE dispatcher preserves these outcomes across descriptor,
  merged and phased modes. Phased OPEN failures retain healthy descriptors and
  submit their borrowed payloads in one descriptor vector. Temporary public
  descriptors share the read engine's deferred-close ownership helper.
  Symlink rejection yields internally before later mutations; native namespace
  recovery repairs only that input, then resumes the proven untouched suffix.
  A live hard-link alias regression verifies that repair cannot overwrite a
  later write. No temporary public outcomes API or execution-prefix guarantee is
  introduced. Mixed invalid inputs, before-repair faults, append behavior,
  cleanup loss and exact healthy/failure compound counts have required-server
  coverage. Special-stateid compatibility now yields a private continuation at
  the exact failed WRITE or pending size SETATTR. Native repair retains confirmed
  bytes and stability, clears already-executed CREATE/TRUNCATE flags, applies a
  rejected truncate exactly once, and resumes later alias mutations afterward.
  Earlier ordinary/uncertain repair failures cannot be swallowed by a later
  stateid status. Required-server regressions cover both merged modes, sparse
  mapping, payload/RPC byte bounds, pending truncation and later hard-link writes.
  A path whose OPEN already ran is reported Failed when its WRITE remains
  unsubmitted after a later wave stop; it cannot claim the logical input was
  wholly NotAttempted.
- Full NFS READ/WRITE cleanup now distinguishes an inline CLOSE failure from
  failure of the next logical input: only the prior owner is closed or deferred,
  then the untouched input resumes without replaying completed data. Captured
  owners survive malformed execution-map handling as well. A successful CLOSE
  retires older observations of the same shared path owner, including hard-link
  aliases. The OPEN cleanup scanner validates both OPEN and GETFH status before
  reading their success payloads.
  Required-server regressions cover failed READ/WRITE followed by healthy input,
  unlink-before-cleanup, inline close-only recovery, deferred ownership and alias
  retirement. Healthy full cohorts still use one RPC and allocate no cleanup
  owners. GETFH now costs one wire operation per OPEN instead of a single final
  WRITE pin; this can reduce capacity near an operation limit. Shared-prefix
  traversal assertions retain their directory-walk bound separately from these
  mandatory cleanup operations. Paired performance measurements remain required.
- SMB raw, descriptor, path and concurrent WRITE engines preserve independent
  outcomes and acknowledged byte counts. One compound WRITE/FLUSH/CLOSE decoder
  serves serial and concurrent paths. Missing FLUSH/CLOSE replies and failed
  cleanup retain validated WRITE progress; temporary handles keep cleanup owners.
  Raw chunk failure advances retained cursors only by confirmed bytes and does
  not replay the prefix. The private vector engine isolates invalid inputs,
  continues ordinary item failures, preserves identity-based alias ordering and
  keeps healthy independent path jobs concurrent. The canonical native hook
  returns the completed outcomes directly. Managed-server tests cover actual permission
  failures, invalid-path/overflow mapping, exact chunk boundaries, cursor
  progress and cleanup; Python compatibility remains exercised.
- Canonical VectorBackend::vwrite_impl now returns VecResult on Local, NFS and
  SMB, including Local's io_uring executor. Existing scalar helpers, C/Python
  compatibility paths, legacy examples and tests explicitly select their strict
  collection policy. Test doubles emit rich successes directly; the copy
  experiment's acknowledged prefix and lost sibling reply stay separate.
- FsClient::vwrite_native and vwrite_mapped_native now return VecResult. Foreign,
  closed and overflowing positional inputs receive NotAttempted slots; healthy
  payloads are borrowed into one backend dispatch. Projection runs once per
  input outside the lock. Healthy vectors allocate no sparse index/placeholder
  maps. Missing replies preserve existing siblings and mark only omitted slots
  Unknown; excess replies quarantine their affected group. Successful identity/
  offset/count checks and error progress kind/bounds validation preserve
  trustworthy completions. Status causes are normalized to input position.
  Client regressions cover reentrant projection, pointer equality, empty/all-
  rejected vectors, definite and uncertain partial writes, invalid progress and
  missing/excess outcomes. Shared direct-hook conformance covers input-local
  rejection, actual executor/server failure and healthy later-wave work across
  POSIX, io_uring, all NFS modes and SMB; SMB's known access-mode rejection is
  asserted as pre-dispatch instead of inventing a wire failure.
- Canonical public Vfsi::vwrite now returns VecResult directly through shared
  macros, concrete facades and mounted routing. WriteOptions keeps its explicit
  partial/complete policy. VfsiExt::write_batch is the deliberate all-success
  collector: one vector call, error selection by input order, no retry/prefix
  promise. Singular and whole-file write conveniences delegate to that policy.
  Existing examples and strict callers choose it explicitly; no temporary
  outcomes method or duplicate public write trait is introduced.
- Native complete writes now retain per-input outcomes through short-write
  waves. Sparse preflight rejects closed/foreign/overflowing inputs without
  suppressing healthy ones, including validation of empty requests. Existing
  interval/frontier dependency scheduling, append handling and borrowed payloads
  remain intact. Definite failures release later eligible writes; uncertain
  predecessors cancel only their dependency descendants. Independent short
  writes continue. Whole-wave rejection after earlier acknowledgments stays in
  slots with known prefixes; zero progress, invalid/missing/excess replies and
  unrepresentable append positions never become nominal successes or retries.
  Identity-discovery failures use the existing conservative alias graph.
- Confirmed WRITE-prefix merging is shared in vfsi-core::internal and reused
  by NFS short repair and native complete-write accumulation. It preserves
  trustworthy prior bytes, validates bounds/progress kind, clears legacy indices
  and upgrades an unsubmitted continuation with prior bytes to a definite
  incomplete logical write. No sibling completed-count field is added.
- Mounted WRITE cohorts preserve local rejections and other routes' results.
  Shared route-shape validation is reused; diagnostic paths refer to caller
  paths. Public conformance covers both write policies, read-only failures,
  invalid/closed/foreign handles, overflow, strict-helper side effects and
  empty vectors on POSIX, mounted, io_uring, NFS and SMB. Existing reference
  wave/frontier tests remain enabled; new tests cover later ordinary failure,
  uncertain descendants, cumulative prefixes, shape corruption and identity
  lookup failure without payload replay.
- StdIo preserves per-input WRITE OpError as the standard I/O error's source.
  A failed write leaves its independent cursor at the last successful position;
  an append-error byte prefix alone does not establish the resolved EOF.
  Unknown effects or acknowledged bytes cannot be reported as retryable
  Interrupted, preventing standard write_all from replaying that input.
  Definite zero-progress interruption remains retryable. Read behavior and C
  ABI v3 layouts are unchanged; Python keeps its explicit legacy collection.
- NFS unaddressable OPEN ownership now has an explicit quarantine policy.
  A named OPEN followed by failure before GETFH, or a lost reply that can hide
  an OPEN stateid (including CLAIM_FH), quarantines that client identity.
  Further work cannot renew it. Recovery uses a fresh owner/verifier and reopens
  known descriptors by exact FH; it never substitutes a diagnostic pathname.
  Failed replacement preparation propagates quarantine back when it shares
  the original client ID. Known GETFH operands and ordinary failed OPENs do not
  unnecessarily quarantine an otherwise usable identity. Server-side state can
  remain pending lease expiration; immediate CLOSE is not claimed without an
  exact operand/stateid. The overall cleanup and paired wall/CPU/memory
  performance gates remain open.
- Canonical Vfsi::vreadlink and native readlink hooks now return VecResult.
  Local attempts each independent readlink syscall, preserving preflight versus
  syscall failures; non-UTF-8, absolute and dangling target text is unchanged.
  NFS READLINK uses paired completion decoding and child lookup now retains
  known results, submitted uncertainty and untouched suffixes across RPCs.
  Parent cohorts preserve one another's outcomes when resolution fails.
  Native selective reconnect retries only failed readlink inputs, preserving
  completed siblings. SMB retains its existing genuinely unsupported operation.
  Shared sync dispatch validates missing/excess slots and rejects impossible
  I/O progress without discarding trustworthy siblings. Mounted routing retains
  its existing kernel-path link behavior and rich results.
  VfsiExt::readlinks_all invokes the vector once and deliberately selects a
  legacy error by input order; read_link and native scalar compatibility helpers
  compose that collection policy. Legacy NFS namespace consumers now attach the
  owning input position when collecting a rich resolution error, independently
  of a nested transport's old or absent index.
  Tests cover POSIX, mounted and io_uring facades, live NFS mixed outcomes,
  original bytes/context, malformed replies, strict/singular dispatch counts,
  server status failures, reply loss and exact healthy/recovery RPC counts.
  C ABI v3 and existing Python scalar readlink conventions are unchanged.
- Canonical Vfsi::vsymlink and vhardlink now return VecResult directly.
  POSIX continues independent syscalls after ordinary failures. NFS source and
  destination-parent resolution retains sparse input outcomes; a failed or lost
  preparation lookup is NotAttempted for the mutation, since no CREATE/LINK was
  submitted. Actual wire-status failures are Failed, and lost mutation replies
  mark only submitted inputs Unknown while untouched inputs remain NotAttempted.
  There is no reconnect/replay loop for ambiguous namespace mutations.
  One fixed-width completion engine now serves the two-operation descriptor/
  CREATE/READLINK groups and four-operation LINK/child-lookup groups. The separate
  child-lookup scheduler is removed. Known results survive repacking and failures;
  a single oversized request is rejected without suppressing healthy siblings.
  Healthy namespace cohorts retain their existing protocol batching.
- Shared sync namespace validation covers symbolic links, hard links and
  readlink, including missing/excess replies, original destination paths and
  invalid byte progress. Mounted hard-link routes retain per-input EXDEV
  rejections and continue independent eligible cohorts. Symbolic-link text keeps
  its mounted-kernel interpretation; SMB retains genuinely unsupported links.
  VfsiExt::symlinks_all and hardlinks_all explicitly collect one completed vector
  and choose the first error by input order. Singular helpers delegate to them;
  native scalar and Python compatibility callers explicitly collect outcomes.
  Existing C ABI v3 and Python exception conventions are unchanged.
- Link conformance covers POSIX, mounted, io_uring and live NFS, including raw
  target bytes, collisions, missing sources/parents and healthy later inputs.
  Native tests verify malformed shapes, invalid progress and exact dispatch
  counts. Live CREATE/LINK tests cover status failures, request-budget rejection,
  resource repacking, lost replies, actual server effects and exact RPC counts.
  Directory CREATE now carries an optional initial mode, as described below;
  the old post-create pathname lookup/SETATTR collector is removed.
- Canonical Vfsi::vmkdir and native directory hooks now return VecResult.
  Duplicate request paths have independent NotAttempted validation errors;
  valid siblings retain one native cohort instead of global preflight rejection.
  Shared namespace validation checks cardinality, error progress and caller paths.
  Mounted routing preserves sparse rejections and continues later cohorts.
  VfsiExt::mkdirs_all is the explicit all-success collector; singular and recursive
  directory conveniences and tree-builder cohorts delegate to that policy.
- POSIX reports failures after a successful mkdir (including exact-mode
  adjustment) as definite incomplete logical operations and continues independent
  siblings. NFS supplies requested permission bits inside CREATE, validates the
  returned mode-attribute acknowledgement and no longer re-resolves paths for a
  separate chmod. This removes extra RPCs and avoids modifying a replacement
  object in that post-create phase. Missing mode acknowledgement is Unknown,
  never a nominal success or an instruction to replay CREATE.
  Live mode/RPC bounds and wire-encoding tests retain the exact-mode coverage.
  See [RFC 8881 CREATE](https://www.rfc-editor.org/rfc/rfc8881.html#section-18.4.3).
- SMB validates modes and paths per input, preserves concurrent independent
  creation and stops only unsubmitted dependent work after infrastructure loss.
  Directories share the cleanup-aware related CREATE/CLOSE engine with metadata
  probes: healthy creation uses one RPC and acquired handles with unconfirmed
  cleanup remain backend-owned. The conventional 0777/server-permissions policy
  remains; unsupported modes do not suppress other inputs.
- Transfer setup consumes mkdir outcomes directly: known fresh directories are
  not replayed or mistaken for pre-existing ones when a sibling returns EEXIST.
  C ABI v3 uses the same unit-outcome mapper for mkdir and close without layout
  changes; Python deliberately collects errors under its existing convention.
  Tests cover public POSIX/mounted/io_uring/NFS clients, native shape corruption,
  sparse duplicates and rejection, real post-create mode failure, server effects,
  missing mode acknowledgement, ABI non-prefix successes and managed SMB.
- Canonical Vfsi::vsetattrs and native typed/raw SETATTR hooks now return
  VecResult on Local, NFS and SMB. Target projection occurs once before locking;
  invalid, foreign and closed inputs retain NotAttempted slots while valid
  siblings keep equal-policy vector runs. Missing replies, excess replies and
  illegal byte progress are normalized without losing trustworthy siblings.
  VfsiExt::setattrs_all deliberately selects the first error after dispatch;
  truncate, chmod and chown use this strict convenience. A failed logical
  update can have applied some requested attributes; it never implies rollback.
- NFS descriptor SETATTR uses the shared paired completion driver. Merged
  path updates preserve wire-status results and continue only proven untouched
  suffixes; submitted reply loss is Unknown, untouched inputs NotAttempted.
  Only pre-mutation symlink guards yield private repair continuations, before
  later alias mutations. Ordinary SETATTR failures are never replayed through a
  phased fallback. Ownership runs resolve valid candidates together and submit
  one descriptor vector. Raw legacy OPEN/copy finalization explicitly collects
  rich results while preserving native status and transport classification.
- Local SETATTR attempts independent siblings after ordinary syscall failures.
  SMB retains its serial size/time engine, isolates unsupported requests, and
  stops untouched inputs after transport loss. C ABI v3 layouts and optional
  symbols are unchanged; C item results report actual non-prefix successes,
  and Python deliberately retains its aggregate exception convention.
  Regression coverage includes malformed results, sparse input validation,
  policy boundaries, retained handles, alias repair, exact compound counts,
  pre-dispatch faults and lost replies with required live fixtures.
- NFS rename backend preparation is implemented: descriptor and merged path
  drivers preserve per-input results, resume only untouched suffixes after
  ordinary status failures, and retain submitted/untouched distinctions after
  reply loss. SETATTR and RENAME share private bounded packing/completion code;
  each keeps its existing wire operations and shared-parent batching.
  Both rename paths are validated before appending wire operations, so a local
  destination rejection cannot leave an untracked source walk.
- Native NFS rename repairs only parent-type failures that cannot have performed
  the rename. A backend-only continuation retains the failed wire opcode; a
  NOTDIR at RENAME is retried only after observing a symlink parent, preventing
  ordinary incompatible-target failures from being replayed. Parent resolution
  verifies directory types and caches successes/failures once per unique parent.
  This follows [RFC 8881 sections 15.1.2 and 18.26](https://www.rfc-editor.org/rfc/rfc8881.html#section-18.26.3).
  No healthy-path type probes are added. Live tests cover ancestor repair before
  dependent later renames, invalid parents, mixed local/wire errors, actual
  destination identities, exact compound counts and before/after dispatch faults.
- Canonical Vfsi::vrename and native rename hooks now return VecResult on all
  backends. Local Replace/NoReplace/Exchange attempt eligible siblings through
  their original atomic syscalls. Mounted routes isolate cross-route and
  unsupported-policy inputs without suppressing other route cohorts.
  VfsiExt::renames_all explicitly selects the first failing input; singular
  rename delegates to it. No completed-prefix guarantee is implied.
- SMB rename retains concurrent jobs for independent pairs and serial execution
  for dependencies. Preparation errors remain NotAttempted; submitted loss is
  Unknown. Ordinary shares use the same cleanup-aware CREATE/SET_INFO/CLOSE
  decoder as deletion, with unconfirmed acquired handles deferred for cleanup.
  The connection layer already rejects DFS shares, so rename needs no SDK
  fallback with hidden cleanup. Replace uses the atomic ReplaceIfExists flag;
  request access remains DELETE plus READ_ATTRIBUTES, matching the old SDK
  request without adding data-read or SYNCHRONIZE requirements.
- Bulk move consumes actual rename outcomes. It counts confirmed renames and
  considers copy/delete fallback only for definite eligible failures after
  source-identity validation. It never probes successes to infer a prefix,
  replays renamed siblings, or falls back on unknown effects. Malformed
  cardinality and illegal progress are rejected before copying.
  C ABI v3 layouts/symbols are unchanged and item arrays retain non-prefix
  successes; Python retains explicit strict error selection.
- Canonical Vfsi::vcopy and native extent-copy hooks now return VecResult on
  Local, NFS and SMB. VfsiExt::copies_all deliberately selects a legacy error;
  scalar copy helpers use it. FsClient projects each path once before locking,
  and mounted routing preserves eligible cohorts around incompatible pairs.
- NFS COPY decodes each synchronous acknowledgement and retains healthy
  siblings through short/EOF continuations and destination-size finalization.
  Async callbacks, oversized counts, invalid completion flags and offset
  overflow become Unknown rather than untracked success. The implementation
  follows [RFC 7862 section 15.2](https://www.rfc-editor.org/rfc/rfc7862.html#section-15.2).
  Source-size query failures keep their original copy slot; missing SIZE is
  rejected, never silently interpreted as a zero-byte copy.
- Server COPY uses canonical independent OPEN owners and one shared temporary
  descriptor retirement path. Known successes are never replayed after another
  input fails. Only a definite eligible rejection with no acknowledged copy
  bytes falls back for that input, using retained source/destination handles.
  Identity dependencies keep backend sequencing without reopening paths.
  Failed scalar fallback CLOSEs also retain cleanup ownership.
- A failed copy may already have created or changed its destination. Later
  metadata or cleanup failure is therefore Failed/Unknown, not NotAttempted.
  The C ABI v3 still exports the same layouts and symbols, now with actual
  per-item copy statuses. Python extent copy consumes one outcome vector per
  wave, maps filtered input identities explicitly, preserves its transport
  exception convention, and never infers a completed prefix or retries a suffix.
- Copy regressions cover non-prefix successes, original paths/indices, malformed
  replies, pre-dispatch versus lost-reply boundaries, sparse size queries,
  short/zero/EOF copies, aliases, cleanup ownership, C item statuses and Python
  no-replay behavior. Paired performance measurements remain an open gate.
- Canonical removal is implemented: Vfsi::vremove, retained-directory contents,
  native recursive/root hooks, VectorBackend::vremove_impl,
  NFS chained remove_many and merged removev_path_compound now return VecResult.
  Local continues independent syscalls, SMB preserves concurrent delete jobs and
  exact cleanup ownership, and NFS resumes only a proven untouched suffix.
  C ABI v3 item arrays export actual non-prefix successes. Python's existing
  strict convention deliberately selects one error after the vector, without
  retry or prefix inference; its cache regression checks both completed siblings.
- Chained NFS REMOVE retains one PUTFH per parent batch, including twenty compact
  names under a 512-byte argument limit. Symlink-parent repair batches the proven
  untouched suffix instead of falling into singleton RPCs. Parent resolution
  uses its already-returned type to reject non-directories before REMOVE.
  Raw status, resource rejection, local validation, oversized input, pre-dispatch
  failure and lost-reply boundaries are tested against a required live fixture.
- Recursive NFS traversal consumes actual native item results. Known successes
  are never reconstructed from an aggregate index, and only an individually
  rejected transient name is eligible for its existing bounded retry policy.
  NFS root tracking distinguishes completed roots, confirmed descendant work
  without root completion, unknown submitted removal and untouched roots.
  Ordinary failures remain local to their root regardless of continue_on_error;
  the option controls continuation within a root, not vector-wide cancellation.
- Path Contents resolves NFS directory identities in one native vector; retained
  Contents validates each owner independently and dispatches valid descriptors
  together without reopening diagnostic paths. Successful borrowed owners stay
  live. Mounted route groups preserve original slots and healthy siblings.
  VfsiExt::removes_all and remove_contents_all collect once without retry.
- Shared recursive classification batches metadata and leaf roots. Directory
  levels retain dependency barriers. Native result-shape checks preserve known
  slots, reject illegal progress and never reconstruct a successful prefix.
  Public regressions exercise all three modes, missing roots and final-symlink
  rejection on POSIX, mounted Auto, io_uring and required live NFS.
- Python recursive removal now groups directory deletions by depth rather than
  assuming vector execution order. A reproducer executes each batch backwards;
  it failed before the fix and passes on local and required live NFS. Independent
  directories at the same depth remain batched.
- Canonical Vfsi::vopen_dirs now returns VecResult with independently owned
  directory handles. The shared frontend acquires one native vector; NFS resolves
  grouped directory identities and registers each known success. The singular
  native helper delegates to that same hook. Path-only backends retain their
  existing Unsupported public behavior.
- Directory acquisition keeps successful owners around missing paths, final
  symlinks and regular files. Lost LOOKUP replies retain prior registered owners,
  mark submitted uncertain slots Unknown and leave untouched slots NotAttempted.
  Export-root identities use their known filehandle without I/O; a live red/green
  reproducer caught the slash-normalization edge case.
- Owned-directory validation quarantines duplicate descriptors under one cleanup
  owner, closes distinct excess resources, retains known slots on omitted results,
  rejects negative descriptors and illegal I/O progress, and preserves input
  context. The increasing-descriptor common path needs no duplicate hash table.
  VfsiExt::open_dirs is the explicit strict helper, dropping all acquired siblings
  on any failure. Tests verify one native dispatch, no replay and cleanup exactly
  once for every distinct valid owner.
- Mounted directory acquisition keeps each input's route/alias and outcome across
  cohorts. Required live coverage includes unsupported POSIX inputs around NFS
  groups, a missing NFS input, distinct mount aliases, renamed roots and borrowed
  contents removal that leaves replacement paths intact.
- Native paging preparation is implemented. Raw NFS readdir_pages and
  readdir_children (including bounded variants) return VecResult. Both share the
  fixed-operation completion decoder, retaining complete pages before a wire
  failure, continuing only proven untouched suffixes, and isolating submitted
  reply loss from untouched input ranges. Actual wire-wave width controls
  READDIR response budgets, including after request repacking or learned limits.
- VectorBackend::vlistdir_pages_impl now returns one native page outcome per
  directory. The shared local/SMB default continues independent scalar page
  operations after ordinary errors and stops untouched work after fatal transport
  failure. NFS retains its existing grouped resolution, anchored child LOOKUP +
  READDIR waves, buffered continuations and bounded page projection.
- Native page errors preserve healthy siblings, including buffered pages after
  another child READDIR reply is lost. Invalid cursor types affect only their
  own input. Fresh inputs whose identity resolution completed are Failed if
  their final READDIR stays unsent; a wholly unsubmitted input remains
  NotAttempted. Fresh-only automatic recovery retries only failed/untouched
  directories and retains earlier pages; retained continuations and child seeds
  are never reopened by diagnostic pathname.
- Canonical Vfsi::vlistdir_pages and vlistdirs now return VecResult directly,
  together with native projection, mounted routing and the portable fallback.
  Invalid/foreign cursors reject only their own input before dispatch. Valid
  siblings preserve batching, owned continuations and anchored child seeds.
  The fallback consumes one rich visitor vector, never an indexed aggregate
  error or a singleton replay. Missing slots quarantine only omitted outcomes;
  excess replies invalidate their affected cohort.
- Directory callbacks fail only their logical root; independent roots remain
  eligible. Explicit Stop ends delivery and produces one outcome per input,
  marking unfinished/unvisited roots Stopped while preserving known successes
  and failures, including errors from an already-submitted prefetch wave.
  No callback is replayed. Failed roots may already have emitted pages.
- VfsiExt::listdirs_all and dir_pages_all collect one vector strictly, select the
  first failed input by position and discard other outcomes. Dropped successful
  pages release their owned cursors; these helpers imply neither an untouched
  suffix nor retry safety. Existing scalar/collecting conveniences use them.
- Returned public pages charge shared entry/path budgets atomically: a declined
  page does not consume a sibling's returned-data allowance. Streaming visitors
  retain their bounded partial-page delivery policy, charge emitted entries and
  report a root-local error when admission fails.
- Existing aggregate native traversal and C cursor snapshots explicitly collect
  one native vector. C ABI v3 layouts and callback-before-snapshot safeguards are
  unchanged.
- Canonical Vfsi::vstream returns VecResult through the shared bounded-cohort
  engine for both singleton and vector calls. OPEN, prefetched READ, continuation
  READ and CLOSE retain each input's outcome. Successful sibling handles remain
  eligible after ordinary failures; callback order remains input order and
  prefetched chunks are never replayed. Every acquired handle closes even on
  cancellation; failed closes retain RAII cleanup ownership.
- File callback errors affect only their file. Explicit Stop ends delivery,
  produces a full outcome vector, marks unvisited inputs Stopped at zero and
  retains known failures from already-submitted work. Confirmed READ error
  prefixes are moved into callback delivery, without cloning their bytes.
- IoProgress::Stream records the contiguous offset accepted by successful
  callbacks on a failed stream. It excludes un-emitted speculative data and any
  effects of a failing callback. It is not a replay permission. Successful streams
  need no error-progress allocation. Plain READ/WRITE reject stream-only progress.
- VfsiExt::streams_all collects one vector strictly; read_stream_with_options
  consumes its singleton outcome and deliberately preserves nested callback error
  provenance. Obsolete private scalar streaming/open paths and their forwarding
  arguments are removed, leaving one public streaming execution engine.
- Still pending: richer external adapters where appropriate, application port
  migration, final API/
  documentation consolidation, versioning and release/port compatibility gates.

SMB server copy now owns both admitted handles and directly decodes COPYCHUNK
replies. The opaque SDK path helper is removed. Definite zero-progress rejection
can fall back through the same handles; acknowledged chunks, lost replies,
malformed successes and explicit partial-failure replies are never replayed.
Success requires every requested range and exact byte counts, following
[MS-SMB2 section 3.3.5.15.6](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/cd0162e4-7650-4293-8a2a-d696923203ef).
The healthy request shape remains 16 chunks and 16 MiB; limit negotiation only
shrinks that bound. Source size comes from the retained OPEN and final size and
flush use the retained destination, even after pathname replacement. Both CLOSEs
are attempted, failures are reported, and unconfirmed owners remain deferred.
Required live Samba checks cover acknowledged later-wave failure, lost replies,
destination replacement and each cleanup owner; all native and Python suites
pass with real server-copy coverage enabled for the native tests.

A paired local-Samba control runs the former SDK path choreography only inside
the ignored manual benchmark. Three alternating samples per arm verify contents
and failed-input parity before accepting measurements. Request counts were
stable across all three samples:

| Inputs | Former SDK requests | Retained requests | Median elapsed, former → retained |
|---|---:|---:|---:|
| 8 × 4 KiB, healthy | 120 | 64 | 24.022 → 16.852 ms |
| 8 × 4 KiB, one missing source | 110 | 57 | 20.750 → 15.315 ms |
| 2 × 17 MiB, healthy | 32 | 18 | 19.088 → 17.349 ms |
| 2 × 17 MiB, one missing source | 21 | 10 | 9.873 → 9.131 ms |

These are native-latency measurements on the managed fixture, not a general
throughput claim. The request reduction comes from removing pathname metadata
and final-size reopening. CPU, allocations and paired peak-memory measurements
remain part of the overall performance gate.

Internal recursive-removal classification and NFS retained-identity preparation
now deliberately collect metadata item results before consuming their payloads.
Reproducing tests demonstrate the prior incorrect removal and cached failed
identity, then verify no mutation or cache publication after a failed slot.

## Purpose

Allow every logical input operation to report its own result without imposing
sequential execution. Backends may batch, pipeline, reorder and execute independent
operations concurrently. Results remain aligned with input positions.

The earlier ordered-execution approach is not the basis for this migration.
There is no completed-prefix model, global stop index, or requirement to wait for
one input before submitting the next. Dependent steps within one logical
operation still obey their protocol and resource dependencies.

## Open live verification findings

The required-server Python run against local Ganesha (NFSv4.1) currently has
2,556 passing tests, four skips and two failing compound-count checks:
test_nfs_integration::test_round_trip_bounds and
test_round_trips_do_not_scale_with_file_count. A 20-file create/write batch
takes four requests on a cold client versus the historical one-request bound;
the five-file batch takes one. Repeated 20-file diagnostics settle to two
requests after adaptive learning. The server logs reject a 105-operation
compound, while successful five-file writes use 30 operations.
The retained-owner GETFH operations are required cleanup work and must not be
removed simply to restore a counter. Do not relax the tests without accounting
for real operation limits and preserving explicit healthy-path packing checks.
This is an unresolved performance/verification gate, not a completed check.
The namespace regression suites, public NFS suite, live SMB native/Python suites,
io_uring facade tests, workspace/Python Clippy, native Python unit suite and
smoke runner pass. The new Python list-move/cache regression passes on both
local and required live NFS backends. The two unchanged RPC-count assertions
remain the only failures in the complete required-server Python run.

## Removal migration verification

The canonical removal migration compiles all workspace features and targets.
Strict workspace and detached Python adapter Clippy pass. Portable core tests
and documentation examples pass. The required smoke runner passes in 27 seconds.
Focused native tests cover malformed outcome counts, forbidden byte progress,
one dispatch, input-order strict selection and path projection before locking.

Required live NFS validation passes 222 backend unit tests, 55 native integration
tests and 11 exclusive/symlink regressions. Seven unrelated/manual backend cases
remain ignored. Public NFS passes 153 tests with two fixture-dependent ignores;
POSIX passes 42. Public/mounted client-vector tests and all seven io_uring facade
tests pass. Managed SMB native tests and all 169 SMB Python tests pass.
The detached Python adapter unit suite passes 58 with one manual test ignored.
The full required-server Python run passes 2,556 tests with four skips and the
same two open write-RPC assertions described above.

Public paging, directory callbacks and file streaming have since migrated.
Whole-plan completion
still requires the performance, compatibility and cleanup gates listed below.

## Directory-open migration verification

All workspace features/targets and the detached Python adapter pass strict
Clippy. Native directory ownership/projection tests and the full native-client
suite pass (125 tests). Portable core tests and documentation examples pass.
Required live NFS backend validation passes 225 unit tests, 55 native integration
tests and 11 exclusive/symlink regressions. The public NFS and POSIX targets pass
153 and 42 tests respectively. Dedicated live mounted-cohort and public retained
directory tests pass without requiring a kernel mount.

A manual paired directory-open control alternates native singleton and vector
dispatch for 24 directories, over three samples per scenario. Healthy median
wall time is 9.058 ms for the singleton control versus 0.433 ms for the vector;
RPCs are 72 versus 3–4. With one missing input, medians are 9.589 ms versus
0.658 ms and RPCs are 72 versus 4. Every result position is checked and every
acquired handle closes; no registered owners remain. This demonstrates native
batching against the singleton control, not a full pre-migration CPU/allocation/
RSS benchmark. The whole-plan performance gate remains open. The reproducible
ignored test is directory_open_paired_batching_control in vfsi-nfs, requiring
test-faults and a required live NFS fixture.

## Native paging preparation verification

The workspace compiles all features and targets, and workspace plus detached
Python adapter strict Clippy pass. Required live NFS validation passes 231 unit
tests (eight unrelated/manual ignores), 55 integration tests and 11 exclusive/
symlink regressions. Public NFS and POSIX pass 153 and 42 tests. Shared native
dispatch passes 125 tests; Local passes 91. C passes 71 unit tests with six
fixture-dependent ignores, and both retained-cursor NFS variants pass when run
explicitly against the required live server. Managed SMB passes 61 unit tests
(two manual ignores), 43 integration tests (one optional fixture ignore) and
169 Python tests. The new native SMB page-vector test executes in that run.
Local/mounted client-vector coverage and all seven io_uring facade tests pass.
The required smoke runner passes in 26 seconds.

New raw regressions cover success/failure/success, whole-vector rejection with
zero dispatch, pre-dispatch failure, lost replies, adaptive resource rejection,
request-byte splitting and response budgets for each actual batch width.
Native cursor tests cover renamed roots, anchored children, malformed cursor
isolation, buffered success across reply loss and no replay of completed pages.
A live recovery assertion observes exactly seven READDIR input submissions for
five directories when two replies become ambiguous: two known successes remain
retained, while only the two ambiguous and one untouched input retry.
These checks cover native preparation. The coordinated public page/visitor
migration described below adds per-input exposure and cancellation/budget
contracts.

## Public directory migration contract and coverage

The source-breaking page and visitor signatures are coordinated across all
in-tree clients. Each accepted N-root call returns exactly N outcomes; callback
cancellation is represented by Stopped, rather than a shortened vector.
Callback errors belong to the root's OpError, with its cause index cleared.
Scalar lifecycle/listdir helpers preserve their existing nested callback error
provenance deliberately. Neither strict collection nor Stop erases failures
already known from speculative sibling work.

Shared public conformance runs on POSIX, mounted routing, io_uring, required live
NFS and managed SMB. It covers success/failure/success, foreign cursors, renamed
roots with replacement paths, callback errors, explicit Stop, shared budgets,
empty calls and recursive traversal where supported. Additional portable tests
cover missing completions and invalid byte progress without replay or loss of
healthy roots. Native cursor-state tests retain exactly-once drop checks.

File streaming has since migrated through the shared engine. External port
migration, end-to-end allocation/peak-memory measurements and remaining
compatibility/cleanup gates are not complete.

Verification for this checkpoint:
- Portable core unit/contract targets pass, including 19 portable API tests and
  ten vector-result ownership/layout tests.
- Shared dispatch passes 16 unit, 125 native-client, six closed-handle read and
  one public backend-contract tests. Local passes 91 tests.
- Required live NFS passes 231 backend unit tests (eight manual/unrelated
  ignores), 55 native integration tests and 11 exclusive/symlink regressions.
  Public NFS passes 154 tests (two optional-fixture ignores); POSIX passes 42,
  directory helpers pass 13, public application API passes 25 and io_uring
  executes all eight facade tests with its feature explicitly enabled.
- Managed SMB passes 61 unit tests (two manual ignores), 44 integration tests
  (one optional-fixture ignore) and all 169 Python tests. The runner completes
  in 56 seconds, with Python tests taking 24.45 seconds.
- C passes 71 unit tests (six fixture/manual ignores); both retained-cursor
  variants also pass explicitly against the required live NFS server. C ABI v3
  and existing optional symbols are unchanged.
- The detached Python native adapter passes 58 unit tests (one manual ignore)
  and strict all-feature/all-target Clippy. Core and vNFS documentation examples
  pass. Workspace strict all-feature/all-target Clippy passes.
- The required smoke runner passes in 37 seconds. git diff --check passes.

The full NFS Python run is not repeated for this directory-only checkpoint.
Its previous two write-RPC gate failures remain open, as recorded above; the
green SMB and detached-adapter runs do not establish those gates are resolved.
No external port, release, or end-to-end memory/performance gate is claimed.

## File-stream migration verification

All canonical Vfsi vector methods now return VecResult, including vstream.
Streaming uses the same bounded OPEN/READ/CLOSE cohort engine for singleton and
vector calls; callbacks remain in input order. A failed OPEN no longer compacts
away a healthy suffix, and a failed READ no longer discards another file's
prefetched bytes. Every admitted owner participates in one close vector.
The former private scalar streaming implementation is removed.

Shared public streaming conformance executes on POSIX, mounted routing, io_uring,
live NFS and managed SMB. It checks success/failure/success, accepted callback
offsets, callback reentry, root-local callback errors, explicit Stop retaining
known speculative errors, empty input and continuation into later bounded
cohorts. Native fault tests move confirmed error prefixes into delivery exactly
once, retain Unknown alongside the accepted offset, isolate uncertain CLOSEs,
close all acquired handles, and reject a zero byte budget before OPEN. Chunk
hints larger than the read budget remain bounded even for one file.

Verification at this checkpoint:
- Portable core contracts pass; portable API coverage grows to 21 tests, with
  explicit one-dispatch strict collection and nested callback provenance tests.
  Error/layout tests reject stream-only progress when merging WRITE prefixes.
- Shared native-client coverage passes 127 tests, including nine streaming
  regressions; its unit, closed-handle and backend-contract targets also pass.
- Public vNFS passes 33 unit tests (15 optional-fixture ignores), 155 required
  live NFS tests (two mount-fixture ignores), 42 POSIX, 26 public API, 46 transfer
  helpers, 12 port helpers and all nine io_uring facade tests. Native io_uring
  passes all 32 engine tests. Existing live streaming RPC-count and ordered/
  round-robin writer batching assertions remain active and pass.
- Managed SMB passes 61 unit tests (two manual ignores), 45 integration tests
  (one optional-fixture ignore) and all 169 Python tests. The managed run takes
  63 seconds; Python tests take 17.42 seconds.
- C passes 71 unit tests (six fixture/manual ignores). C ABI v3 and optional
  symbols are unchanged; its legacy callback conventions remain deliberate
  compatibility adapters. The detached Python native adapter passes 58 unit
  tests (one manual ignore) and strict all-feature/all-target Clippy.
- Workspace strict all-feature/all-target Clippy and core/vNFS doctests pass.
  The required smoke runner passes in 48 seconds. git diff --check passes.

At this streaming checkpoint those gates were still open. The latest
verification checkpoint above supersedes that status for SMB cleanup, local
port compatibility, Python RPC tests and local paired measurements. Final
audit, coordinated Rust release/port source changes and any additional remote
performance controls remain separate release work. All source work is local
and uncommitted.

## NFS unaddressable-owner cleanup verification

The central dispatch boundary records whether a request contains OPEN, and
whether a failed reply leaves a successful named OPEN without an exact GETFH
cleanup operand. Lost replies quarantine any potentially acquired OPEN stateid,
including CLAIM_FH. A quarantined identity rejects work before wire submission;
known per-input completions remain intact, and untouched later work is not
misclassified as submitted. An empty OPEN vector does no I/O or deferred cleanup;
a subsequent real operation drains known deferred owners.

Reconnect explicitly retires the quarantined owner and verifier, even when a
stable client owner was configured. If replacement preparation itself loses an
OPEN under the same stable client ID, quarantine transfers back before that
replacement is discarded, so the next attempt cannot renew the orphaned state.
An unrelated replacement identity does not quarantine the original. Recovery
preserves live descriptor numbers and uses pinned FH operands after rename and
pathname replacement. No public result type, C ABI v3 layout or optional symbol
changes for this policy.

This deliberately does not promise immediate server-state destruction when
the cleanup FH or stateid is unknowable. The client stops renewing the retired
identity and leaves its state subject to lease expiration/revocation; it does
not risk closing a replacement object through its old name.
[RFC 8881 sections 8.3 and 18.50](https://www.rfc-editor.org/rfc/rfc8881.html#section-18.50)
describe lease handling and why active state can prevent DESTROY_CLIENTID.
Healthy calls add no RPC, no cleanup-owner allocation and no successful-reply
scan; request flags are recorded while building OPEN operations.

Five new regressions cover an actual successful OPEN followed by failure before
GETFH, an explicitly failed GETFH response, fully published requests with lost
replies for named and handle-based OPENs, stable-identity retirement with a
renamed retained object, and quarantine propagation from a failed replacement.
They also preserve coverage for ordinary failed OPENs, known GETFH followed by
READ failure, known CLAIM_FH operands, zero-RPC quarantine and empty vectors.
Focused live cases execute on both NFSv4.1 and NFSv4.2.

Verification passes 236 native NFS unit tests (eight manual/unrelated ignores),
55 native integration tests, 11 exclusive/symlink regressions and 155 public
NFS tests (two optional mount-fixture ignores). Workspace strict all-feature/
all-target Clippy and the required smoke runner pass. Existing healthy-path
compound-count assertions remain enabled. The two Python write-RPC gates are
not claimed resolved and are not rerun for this focused cleanup change.

### SMB cleanup ownership checkpoint

SMB descriptor and temporary-handle cleanup now follows the SDK directory
reader's one-attempt, session-pinned CLOSE policy. Each live handle shares its
original share/session owner. A submitted close retires its local descriptor;
if completion is uncertain, the backend retains the original cleanup owner for
share teardown and never resubmits its FileId. This prevents a later CLOSE from
targeting a different open after identifier reuse. A close proven unsubmitted
keeps its usable descriptor and remains eligible for one cleanup attempt.

Temporary compounds preserve a typed CLOSE outcome independently of CREATE,
data-operation results and reply cardinality. A lost, truncated, or malformed
CLOSE reply retains teardown ownership. Only explicit related-chain
STATUS_INVALID_HANDLE/STATUS_INVALID_PARAMETER rejection allows one standalone
CLOSE with the acquired FileId; failure of that attempt is never replayed.
Deferred cleanup does not hide outcomes from an unrelated OPEN or COPY.

Share owners pin the original SDK Tree and connection generation/session.
Cleanup of an ended session emits no CLOSE on its replacement. Explicit
shutdown closes only never-attempted owners, then disconnects each retained
live tree. Unknown owners can therefore remain server-side until original
share/session teardown; successful sibling handles stay usable meanwhile.
There is one shared allocation per connected tree and one Arc per live owner,
with no per-success cleanup allocation or additional healthy CLOSE RPC.

New regression coverage includes exact CLOSE-submission counts, deterministic
FileId reuse after a lost reply, shutdown without replay, retained usable owners
after pre-dispatch failure, and a table covering definite versus uncertain
related-chain rejection. Existing truncated reply/data-progress coverage remains.
Required managed Samba validation passes 66 unit tests (two manual benchmark
ignores), 45 integration tests (one optional mount fixture ignore), and 169
Python tests. Strict workspace all-feature/all-target Clippy passes.

The latest verification checkpoint above records subsequent port checks,
Python RPC accounting and local paired measurements. Coordinated Rust package
versioning, port source/pin changes and final release review remain open.

## Original problem and scope

The public trait lives in crates/vfsi-core/src/api/vfsi.rs. Methods such as
vgetattrs, vread, vwrite and vopen previously returned one Result around an entire vector.
At the start of this migration, methods such as vremove and vsetattrs also
returned Result<()> despite accepting multiple inputs. Those signatures could
not retain successful results alongside
individual failures.

VfError describes a failure; it does not itself stop execution. Its existing
index identifies an input, not completed progress or retry safety. Dispatch
layers and early error propagation currently collapse richer backend outcomes.
For example, the io_uring engine already tracks per-request completions, while
upper layers return one error. Some schedulers also stop untouched work after an
ordinary item failure; changing the return type alone will not change that.

The migration must cover native backend reporting, vfsi-sync adaptation, the
portable trait, helpers and external adapters. Changing only the public typedef
would not recover information already discarded below it.

## Shared result shape

Proposed aliases, with OpError defined as described below:

~~~rust
pub type OpResult<T> = Result<T, OpError>;
pub type VecResult<T> = Result<Vec<OpResult<T>>, VfError>;
~~~

Illustrative final signatures, retaining the existing arguments:

~~~text
vgetattrs(...) -> VecResult<Attrs>
vstatfs(...)   -> VecResult<FilesystemStats>
vreadlink(...) -> VecResult<PathBuf>
vsymlink(...)  -> VecResult<()>
vhardlink(...) -> VecResult<()>
vread(...)     -> VecResult<ReadResult>
vwrite(...)    -> VecResult<WriteResult>
vopen(...)     -> VecResult<Self::File>
vopen_dirs(...) -> VecResult<Self::Dir>
vremove_dir_contents(...) -> VecResult<()>
vlistdir_pages(...) -> VecResult<DirectoryPage>
vlistdirs(...) -> VecResult<TraversalCompletion>
vstream(...) -> VecResult<StreamCompletion>
vclose(...)    -> VecResult<()>
vfsync(...)    -> VecResult<()>
vsetattrs(...) -> VecResult<()>
vmkdir(...)    -> VecResult<()>
vremove(...)   -> VecResult<()>
vrename(...)   -> VecResult<()>
vcopy(...)     -> VecResult<()>
~~~

ReadResult and WriteResult remain successful payloads. Do not put an error field
inside ReadResult or add a second independent success/failure flag to it.

The outer and inner errors have different contracts:

| Location | Meaning |
|---|---|
| Outer Err(VfError) | Reject the entire vector before dispatching any input operation. |
| Inner Ok(T) | The corresponding logical input completed successfully. |
| Inner Err(OpError) | The corresponding input did not complete successfully; its completion state and any confirmed progress are explicit. |

After dispatch starts, return the outcome vector, including on transport or
engine failure. An outer error must never discard already-established successes,
owned handles, or per-operation progress.

For an accepted vector of N inputs, return exactly N outcomes. Empty input
returns Ok with an empty vector and performs no input operation. Allocate result
bookkeeping before dispatch where needed; resource exhaustion must not become a
post-dispatch outer error that hides effects. Normal return guarantees do not
purport to survive process termination or an unrecoverable allocation abort.

## Failure knowledge and progress

OpError retains the underlying VfError and a typed completion classification.
The core foundation uses private fields with explicit not_attempted, failed and
unknown constructors. Its representation and layout bounds are tested in
crates/vfsi-core/tests/vector_results.rs.

| State | Meaning |
|---|---|
| NotAttempted | The logical input was never submitted for execution. |
| Failed | A definite unsuccessful outcome is known. |
| Unknown | Execution or effects may have occurred, but completion cannot be established. |

NotAttempted includes the cause that prevented submission. Do not attribute a
sibling's filesystem errno to an unsubmitted input as if that input executed.

Failed is not synonymous with no effects. A compound logical operation can make
confirmed progress before failing. Unknown is not permission to retry.

Reads and writes spanning multiple submissions must also retain confirmed
operation-specific progress. Examples include a valid caller-buffer prefix,
owned bytes already read, and write bytes acknowledged before a later failure.
Unknown effects beyond confirmed progress remain unknown. A whole-file read
that produced only a prefix must not become a successful whole-file ReadResult.

IoProgress::Read retains a ReadResult prefix, including whether it describes
owned data or a valid caller-buffer prefix. IoProgress::Write records the
contiguous acknowledged prefix of this input's payload. The optional progress
is boxed on error, keeping the status-only error path free of progress allocation.
IoProgress::Stream records only the end of the prefix accepted by successful
stream callbacks. It retains no duplicate payload; a failing callback may have
additional effects, and prefetched but un-emitted bytes are not delivery progress.
Before migrating those I/O forms, enforce the request-specific bounds and valid
prefix guarantees in each backend and its adapters. Do not add a generic batch completed count, overload EOF,
or use a bare byte count to imply that all later bytes are untouched. This
representation is a design gate, not something an adapter may silently omit.

Keep native errno/NFS/SMB statuses, transport provenance and error sources.
Per-operation identity comes from the outcome slot. Do not add another input
index to OpError or reinterpret VfError.index as execution progress. Retain
indexed VfError compatibility for legacy APIs during the transition; normalize
or remove redundant indexing at the new boundary as that migration permits.

## Execution, validation and retry rules

- Result order follows input order. Submission and completion order are backend
  decisions. No vector-wide atomicity, rollback or snapshot is implied.
- An ordinary item failure does not cancel independent siblings or prevent later
  independent inputs from being submitted.
- Fatal connection, authentication or engine failures may stop further dispatch.
  Preserve known completions and classify every remaining input truthfully.
- Batch-wide structural validation and aggregate resource-budget rejection happen
  before dispatch and use the outer error. Do not partially accept an oversized
  allocation request in an input-order-dependent way.
- Input-local problems, such as a closed or foreign handle, normally produce
  that input's inner error while valid independent inputs remain eligible.
  Validate ownership before any unsafe handle use.
- Shared-buffer and other structural safety constraints still apply to the
  whole call. Return only after every submitted operation can no longer access
  borrowed buffers, even if cancellation or an engine error occurs.
- Retry only work proven unsubmitted, or work whose specific protocol recovery
  contract permits recovery. Do not replay mutations merely because another
  input failed. Preserve existing session/replay protections.
- Aliasing operations do not gain ordering guarantees. For example,
  rename(a, b) followed by rename(b, c) requires separate calls or a deliberately
  sequenced high-level workflow. Do not silently run legacy ordered rename
  semantics through a newly unordered implementation.

## Backend requirements

### io_uring

Retain the current wave/queue batching and CQE attribution. Preserve completions
from an entire submitted wave, including successful siblings after another CQE
reports an error. Distinguish untouched jobs from completed zero-byte requests.

Continue independent pending work after ordinary per-operation errors. Fatal
engine failures may terminate dispatch, but drain or safely cancel published
SQEs before releasing borrowed storage. Never implement the new API as a loop
that waits for each singleton request.

### POSIX/local

Collect each syscall outcome and continue independent inputs after an item
failure. Sequential syscall execution is allowed as a backend implementation;
it is not imposed by the portable contract.

### NFS

Decode successful results before a COMPOUND failure rather than discarding them.
Map the actual failing protocol operation to its logical input. A protocol
failure in lookup/setup can fail a logical input before its terminal operation.

A known unexecuted suffix is not a set of executed failures. Schedule eligible,
independent suffix inputs separately when the protocol proves they were not
executed. If a logical input spans several protocol operations, preserve any
confirmed partial effects and retain its internal dependencies.

On a lost reply, preserve successes established by other RPCs and classify the
affected inputs as Unknown unless protocol session recovery establishes a
stronger outcome. Do not infer execution from transport error position zero.

### SMB and shared adaptation

Preserve individual request outcomes and distinguish unsubmitted requests from
requests with lost replies. The same grouping and mapping rules apply.

vfsi-sync must retain outcome slots through preflight, type-based read phases,
chunking, backend cohort splitting and mapping back to original positions.
Malformed backend replies must not turn into an outer error after effects:
retain independently validated outcomes and mark affected inputs appropriately.

## Direct trait migration and VfsiExt conveniences

Change each canonical Vfsi method directly to VecResult when its operation
family is migrated. There are no temporary _outcomes methods, parallel vector
method families, or Vfsi2 trait. For example, vgetattrs itself changes from
Result<Vec<Attrs>, VfError> to VecResult<Attrs>.

This is a source-breaking change for callers and trait implementors. Migrate one
operation family at a time, updating all in-tree backend implementations,
vfsi-sync adapters, public re-exports, tests and affected callers together in a
buildable integration commit. Backend preparation may land earlier behind the
unchanged public contract, but must not be presented as completed migration.

Do not change a signature while leaving an in-tree backend to return a newly
introduced Unsupported rejection merely because it has not been migrated.
Existing genuinely unsupported operations may still reject before dispatch.
External implementors need migration notes and the planned breaking release.
Update the compilation-facing C/Python and helper call sites in the same
operation-family migration; the later adapter stage is for richer interfaces
and compatibility verification, not permission to leave them broken.
Apply the project's versioning, release and port-compatibility gates.

Implement the new contract through real backend batch reporting. Do not provide
a default that executes one old singleton call per input, and do not reconstruct
outcomes from a legacy error index. Existing stronger ordering guarantees must
be removed explicitly in the breaking contract and dependent workflows updated;
an aggregate convenience wrapper does not restore those guarantees.

### Convenience wrappers belong in VfsiExt

Vfsi owns the vector execution contract and all individual outcomes. VfsiExt
owns conveniences that deliberately choose a simpler consumption policy.
A conceptual metadata example is:

~~~text
Vfsi::vgetattrs(...) -> VecResult<Attrs>
VfsiExt::getattrs_all(...) -> Result<Vec<Attrs>, VfError>
~~~

Use short operation-oriented helper names rather than an _outcomes suffix.
The existing singular attrs helper remains singular; attrs_all denotes that all
requested attribute queries must succeed. Check existing names and semantics
before adding a helper. Do not mechanically introduce an _all wrapper for every
trait method when an existing helper already serves that purpose.

An aggregate wrapper invokes the corresponding vector primitive once and
consumes its result. It collects values if every input succeeds; otherwise it
selects the first error in input order, not the first error observed in time.
The complete vector call has already finished: selecting one error does not
mean execution stopped at that input or that later inputs were untouched.

These Result<..., VfError> wrappers are intentionally lossy: they discard other
outcomes and any operation-specific completion/progress information that cannot
be represented by VfError. Document that limitation and preserve the selected
underlying cause, native status, source and appropriate diagnostic input index.
They must not advertise retry safety, a completed prefix or sequential execution.
Callers that need partial results, completion certainty, reconciliation or
selective retry must use the core VecResult. No wrapper automatically retries,
rolls back mutations or submits a second copy of the original operation.

Implement collection/error selection with shared internal helpers where it is
actually shared. Avoid duplicate scheduling engines or per-backend convenience
implementations. Prevent recursion: VfsiExt calls Vfsi, not the reverse.

### Ownership and existing helpers

Resource-owning results require explicit collection policy. An all-success open
helper that cannot return successful siblings on another input's failure must
clean up those owned handles. A close wrapper must preserve the correct cleanup
ownership of each borrowed handle. Do not roll back unrelated mutations to
simulate all-or-nothing behavior.

Migrate existing VfsiExt singular helpers to extract the single inner result and
map outer rejection deliberately. Vector helpers that need individual outcomes
consume VecResult directly; helpers whose documented policy requires all inputs
to succeed may share the aggregate collection logic. Helpers such as
copy_to_writers must close every successfully opened handle and preserve writer
failures without replay.

C ABI and Python APIs require explicit compatibility/version decisions.
Do not change C layouts or existing exception/return conventions implicitly.
Their compatibility adapters may collapse outcomes under their existing
documented behavior, while richer results can be additive where appropriate.
Those adapters do not justify retaining duplicate methods in Vfsi.

## Incremental rollout

| Stage | Deliverable and exit criteria |
|---|---|
| 1. Core contract | VecResult/OpResult, OpError completion/progress design, invariants, result-size measurements and backend conformance fixtures. Resolve I/O progress representation before implementing affected I/O forms. |
| 2. Metadata queries | Change vgetattrs and vstatfs directly to VecResult across all in-tree backends, shared mapping and callers. Add/reuse VfsiExt conveniences such as getattrs_all. Mixed successes/failures retained without singleton dispatch. |
| 3. Reads | Positional allocating and borrowed-buffer vread first, prioritizing io_uring; then mixed phases and whole-file reads with explicit partial-data handling. |
| 4. Handle lifecycle | vopen, vclose and vfsync with per-input ownership, successful-handle retention and ambiguous cleanup coverage. |
| 5. Writes | vwrite, short writes and write_all; confirmed progress retained and no replay of ambiguous suffixes. |
| 6. Namespace mutations | vsetattrs, mkdir, links/readlink, rename, copy and removal. Define one logical outcome per input; recursive failure can include earlier descendant mutations. |
| 7. Streaming/adapters | Streaming, directory cursors/traversal, callbacks, high-level helpers, C/Python and application ports. Preserve emitted data and cursor ownership; specify how callback cancellation differs from per-input failure. |
| 8. Consolidation | Audit canonical VecResult methods and VfsiExt conveniences for duplication, update documentation/examples and external migration notes, complete release and port verification. No temporary public methods need removal. |

Each stage may have backend preparation commits followed by a coordinated,
buildable signature-and-caller migration. Do not leave half-migrated trait
implementations or claim an operation family is migrated while a backend still
discards completions internally.
The first practical milestone is metadata queries, immediately followed by
vread so the design meets the performance-sensitive case early.

## Required verification

- Mixed success/failure/success with one output slot per input.
- Out-of-order completions mapped correctly after splitting, regrouping and
  partial backend submission.
- No input operations dispatched on outer rejection; zero-input behavior.
- Invalid local input does not block valid independent siblings.
- Ordinary item failure does not stop later waves; fatal engine failure retains
  known outcomes and identifies unattempted/unknown inputs.
- NFS failed COMPOUND prefixes, known unexecuted suffixes and lost replies;
  relevant SMB disconnect cases.
- Partial read/write progress, short writes, zero-byte success versus untouched
  work, and uncertainty after acknowledged write progress.
- Borrowed-buffer safety after errors/cancellation and cleanup of every
  successfully acquired resource.
- VfsiExt aggregate wrappers call the vector primitive once, select errors in
  input order and release owned successful results on failure. They and external
  compatibility adapters do not leak handles, replay mutations or invent prefix
  guarantees. Callback-based APIs do not repeat already-emitted output.
- Each direct trait-signature migration compiles all in-tree implementors and
  callers, with no temporary method family or migration-only Unsupported stub.
- Deterministic batching assertions: multiple independent requests remain in
  flight on io_uring; no per-item frontend/backend locking or accidental
  singleton RPC loops.
- Paired success-path and mixed-failure benchmarks: throughput, wall time,
  submissions/RPCs, peak memory and allocation counts. Inspect size_of for
  representative outcome/error types; avoid unnecessary per-success heap
  allocation and duplicated paths or indices.

Use the focused test targets and smoke runner required by AGENTS.md for each
implementation stage. Live recovery and protocol uncertainty require live/fault
coverage where mocks cannot establish the contract. Documentation-only changes
need proofreading and git diff --check rather than a Rust rebuild.
