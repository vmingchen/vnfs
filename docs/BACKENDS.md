# Backend implementation

This guide describes native execution contracts, not the application API.
Applications use `Vfsi` and `VfsiExt`; start with [Rust API architecture](RUST_API.md)
for requests, object references, ownership, and failure semantics. Native Rust
contracts are pre-1.0 and may change without compatibility aliases.

## Trait and package boundaries

Import `vfsi_sync::backend::{HandleBackend, VectorBackend}`:

- `HandleBackend` is the minimum owned-handle contract: descriptor I/O,
  lifecycle, synchronization, and handle-level attributes/statistics. It does
  not require namespace operations or directory enumeration.
- `VectorBackend: HandleBackend` adds native vectors, directory paging,
  namespace/link/copy operations, and overridable workflows. It is object-safe
  for binding dispatch. Scalar I/O does not imply a blanket vector implementation.
- `vfsi_sync::FsClient<F>` implements the portable application contract when
  `F: VectorBackend`. `OpenFile<F>` and `FsDir<F>` own cleanup obligations;
  copying a `FileOperand` or numeric `Fd` does not duplicate those resources.

Shared defaults are free functions in
[`vfsi-sync/src/backend/helpers`](../crates/vfsi-sync/src/backend/helpers),
generic over the relevant backend trait, including `?Sized` where needed for
dynamic dispatch. Trait defaults delegate to them; native overrides preserve
batching and retained-object semantics. NFS compounds/recovery, SMB transport
and COPYCHUNK, and local anchored descriptors belong to their backend crates.
`vfsi-local` supplies shared local machinery; `vfsi-posix` and `vfsi-uring`
select distinct descriptor-I/O executors.

Use the [trait definitions](../crates/vfsi-sync/src/backend/mod.rs) as the exact
method inventory. There are no root `FileSystem`/`Backend` aliases or
`sfsi`/`vfsi` native namespaces. Protocol negotiation inspection belongs on
protocol backends or binding-specific extension contracts, not `Vfsi`.

## Execution seams

Native filesystem hooks use `*_impl`, with `v` prefixes for vector engines.
Lifecycle, configuration, and notification hooks keep descriptive names.
Protocol wire structs, method names, and compound tags retain their vocabulary.

| Seam | Responsibility |
|---|---|
| `vopen_impl` | Typed strict OPEN; preserve input order and resource cleanup on failure. |
| `vopen_outcomes_impl` | Raw paths/flags/modes to `VfResult<ManyResults<FileOperand>>`; retain confirmed partial successes for the strict collector. |
| `vopen_raw_impl` | Raw-flags adapter, not another application OPEN API. |
| `vread_impl`, `vread_into_impl` | Allocating and caller-buffer reads; validate result cardinality, operand/offset association, and progress bounds. |
| `vread_all_with_options_impl`, `read_file_impl` | Bounded vector/scalar whole-file collection; scalar collection takes an explicit byte bound. |
| `vwrite_impl` | Single borrowed write boundary; short progress is acknowledged progress, not permission to replay an ambiguous failure. |
| `vsetattrs_impl` | Typed `SetAttrsOp<Target<'_, FileOperand>>` updates; raw-mask/no-follow hooks adapt binding inputs. |
| `vcopy_impl`, `vcopy_data_impl` | Extent-copy policy and explicit client-copy execution respectively; do not conflate them with recursive application helpers. |
| Directory paging and anchored-removal hooks | Retain cursors, continuation state, budgets, and real directory identity instead of reopening diagnostic paths. |

Portable writes, native writes, and retained-handle writes specialize one
doc-hidden `vfsi_core::internal::WriteRequest` storage definition. Public
aliases fix their operand, offset, and flag policies. Portable writes use
absolute offsets and borrowed handles; native writes also support `VfOffset`
and path create/truncate flags. Fields are private and construction does no I/O.

Bindings may own preparation storage and project `op.borrowed()` into
`vwrite_impl` without copying payloads or cloning targets. Backends must not
retain borrowed operations after dispatch returns. Owned NFS/SMB wire messages
remain protocol representations, not additional portable request contracts.

## Strict results and resource ownership

The application [error contract](RUST_API.md#durability-and-failure-rules)
returns no partial value vector on failure; it does not roll back mutations.
`ManyResults<T>` is a doc-hidden native mechanism, not an application result
type. Position `n` always corresponds to request `n`:

- NFS executes a COMPOUND in order and stops at its first failing operation.
  Confirmed prefix handles remain available for cleanup. Missing suffix results
  are not fabricated successes. Splitting translates errors to original request
  positions rather than wire operation/chunk numbers.
- SMB requests may complete concurrently and out of order. Native outcomes
  retain original request positions, including successes after a failed item.
- The ordered local OPEN implementation stops at the first failure. This is
  not a promise that every io_uring descriptor-I/O wave completes a prefix.

The strict OPEN collector chooses the lowest request-position failure among
reported outcomes and attempts cleanup of every confirmed successful handle.
It preserves the original failure if cleanup also fails and retains failed
cleanup ownership for later draining. Never discard successful suffix handles
merely because another operation failed. Cleanup does not undo creation or
truncation, and an unconfirmed lost reply may leave effects that cannot be proven.

Read/write reply contract violations must return errors rather than panic or
silently misassociate data. A short read without EOF must not skip a gap;
reported write progress must be contiguous for completion helpers to resume it.
Only cursor-based operations advance backend cursors. Retained-handle recovery
must verify or preserve object identity, not reopen a replacement by name.

Application callbacks run outside backend locks. A direct native callback has
the contract documented by its hook; do not assume application reentrancy rules
apply while holding a native mutable backend borrow. File/directory drops queue
cleanup while a client remains alive; final-owner teardown can block on timeouts.
Native owners such as `NfsVecFs::shutdown` report teardown errors explicitly.

## Binding and application adapters

Keep `FsClient`'s native dispatch methods distinct from portable `Vfsi` methods:
native projections use `vread_native`, `vwrite_native`, and their semantic
variants. Native compatibility helpers such as `vclose_owned` are not additional
methods on the `vnfs` application facade. Applications compose vectors through
`VfsiExt`, including `open_options`, `close_files`, and `std_io`.

C/Python bindings retain their own exported operation names, descriptor ownership,
raw input conversion, and partial-result behavior. They use object-safe native
contracts and binding-specific protocol inspection rather than requiring a
second portable trait family. A Rust source rename does not itself authorize
changes to exported C layouts or Python contracts.

## Fault-injection and validation requirements

The non-default `test-faults` feature supplies deterministic hooks around
dispatch, replies, local handle registration, and cleanup. Scripts are ordered
and one-shot; tests must assert every configured fault was consumed. Synchronize
with explicit gates, not timing sleeps.

Required assertions include:

- exact original indices for attributable failures; `index() == None` for
  unattributable transport failures, never a fabricated zero;
- no partial OPEN vector escapes; every confirmed handle, including an SMB
  suffix success, is cleaned without losing ownership after cleanup failure;
- the primary error survives a secondary cleanup failure;
- ordered NFS prefixes and concurrent/out-of-order outcomes preserve request
  association and malformed replies produce errors, not panics;
- short I/O, cancellation, callback errors, and failures in later vector waves
  preserve bounds, ordering, and acknowledged progress;
- batching/compound-count fast paths survive both success and fault handling;
- potentially mutating operations are not replayed after ambiguous transport
  failures.

Property tests cover NFS-style prefixes and SMB-style complete outcomes. Live
NFS/Samba tests complement deterministic hooks. The live NFS suite also uses a
test-only ONC-RPC record proxy to drop an OPEN reply after server execution,
checking exclusive-create no-replay behavior and unattributed errors. Shared
native contract assertions are available through `vfsi-sync`'s non-default
`test-support` feature.

Select tests by changed behavior using [AGENTS.md](../AGENTS.md) and the
[test runners](../scripts/README.md). Record source revision, commands, enabled
features, executed/skipped counts, fixture requirements, and omitted suites in
the change's validation record. Historical passing counts are not proof that
the current checkout has been tested.

## Identity and open descriptor names

The [object-reference tables](RUST_API.md#object-ids-open-instances-and-mount-routes)
define current semantics and Linux/GCS/NFS analogies. This appendix records only
remaining compatibility spellings and source changes needed around them:

| Previous spelling | Current spelling | Compatibility / migration |
|---|---|---|
| Core `FileHandle` trait | `OpenFd` | Deprecated re-export at existing core/API/prelude and `vnfs::files` paths; the same trait. |
| `vfsi_sync::FsFile<F>` | `vfsi_sync::OpenFile<F>` | Deprecated re-export of the same owned type. |
| NFS `client::FileHandle` | `client::NfsFh` | Deprecated module-scoped re-export of the same opaque reference. |
| `VfFile` | `FileOperand` | Deprecated re-export; unchanged constructors/path variants remain usable. |
| `VfFile::Descriptor(fd)` | `FileOperand::Fd(fd)` | Variants have no alias; update expressions and matches. |
| `is_descriptor()` | `is_fd()` | Deprecated forwarding method; `from_fd()` and `fd()` remain. |
| `Attrs::filesystem_id()` / `FsObjId::filesystem_id()` | `fs_id()` | Deprecated forwarding accessors. |
| `AttrMask::FILESYSTEM_ID` | `AttrMask::FS_ID` | Deprecated associated constant with the same bit value. |
| `VfAttrs.filesystem_id` | `VfAttrs.fs_id` | Fields have no alias; update accesses and literals. |
| `FileIdentity` / `file_identity()` | `FsObjId` / `fs_obj_id()` | Compatibility names remain; prefer the canonical spellings. |

These aliases do not imply a source-stability promise. Removed trait families,
intermediate metadata request types, and the exhaustive historical rename ledger
are available in Git history rather than maintained as another API inventory.
