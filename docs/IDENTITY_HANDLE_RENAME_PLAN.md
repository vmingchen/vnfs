# Identity and handle rename plan

Status: implementation and verification complete. Live fixture coverage is listed below. This plan was prepared
against the SSH checkout at `/home/ubuntu/vnfs`. The selected vocabulary and
Linux/NFS/GCS comparison are in the
[Rust API naming table](RUST_API.md#object-ids-open-instances-and-mount-routes).

## Implementation sequence (applied)

The following steps were applied as one coordinated Rust API migration. The working checkout is `vm:/home/ubuntu/vnfs`; existing identity changes and unrelated `.orig` backups must be preserved.

1. **Rename NFS-specific types first.** In `crates/vfsi-nfs/src/client.rs`, rename the wire object reference to `NfsFh` and migrate signatures, request/result structs, constructors, and references. In `nfs.rs` and its children, replace the `WireFileHandle` import alias with `NfsFh`. Rename the private open record to `NfsOpenState` throughout open/close, deferred cleanup, vector execution, and recovery. Doing this first frees `OpenFile` for the generic owned handle and avoids conflating two existing `FileHandle` symbols.
2. **Rename filesystem ID storage and accessors.** In `vfsi-core`, change `VfAttrs.fs_id`, private `Attrs`/`FsObjId` storage, and their `fs_id()` accessors. Migrate metadata projection, local `st_dev` population, NFS FSID decoding, request/returned masks, variables, tests, and documentation. Add `AttrMask::FS_ID` with the existing bit value `1 << 13`; retain the old spelling as a deprecated associated constant outside the bitflags declaration, so it does not create a second named flag. Keep `file_id`, `FsObjId`, numeric widths, missing-attribute behavior, and the FSID packing unchanged.
3. **Rename the native operand and remove the unused ID.** Rename `VfFile` to `FileOperand` and `Descriptor(Fd)` to `Fd(Fd)`. Update native request fields, `Target<'_, FileOperand>`, backend traits/defaults, constructors, and every pattern match in local, NFS, SMB, sync, bindings, and tests. Keep `from_fd()` and `fd()`; introduce `is_fd()` as the primary predicate and retain `is_descriptor()` as a deprecated forwarding method. Remove `HandleId` and its `from_legacy`/`as_legacy` implementation. Audit wildcard re-exports so the removed type is absent from core, sync, and the NFS `vecfs` compatibility namespace. Spell out File Descriptor in relevant comments and distinguish backend slots from OS descriptors.
4. **Rename the portable handle contract and owned wrapper.** Rename core `FileHandle` to `OpenFd` in its definition, bounds, imports, extension methods, borrowed requests, and re-exports. Rename `vfsi_sync::FsFile<F>` to `OpenFile<F>` in its definition, lifecycle implementations, client constructors/results, typed read/write requests, application implementations/macros, and debug name. Update VNFS facade wrappers, `AutoFile`, NFS read pools, and test support to implement `OpenFd` or contain `OpenFile`. Keep `Vfsi::File`, `NfsFile`, `PosixFile`, `UringFile`, `AutoFile`, `FsDir`, and directory contracts under their existing names.
5. **Migrate every consumer and public import path.** Update core/sync/API/prelude exports and `vnfs::files` to expose canonical names. Migrate root-workspace backends, examples, integration tests, C binding Rust internals, and the detached native Python adapter. Audit all detached adapters and fuzz targets even where they consume the renamed types only indirectly. Use module-aware edits; an unrestricted `OpenFile` replacement would incorrectly rename the third-party `fsspec.core.OpenFile`, and an unrestricted `Descriptor` replacement would affect other enums such as `VfDir`.
6. **Finish documentation, compatibility, and verification together.** Apply canonical names to active documentation, rustdoc links, doctests, comments, example imports, test names, and any affected diagnostic expectations. Add migration entries and examples for the source changes below. Run the focused tests followed by the cross-package checks, then search the complete tracked repository for obsolete names and review every remaining match.

## Compatibility policy

Aliases preserve the same nominal type or trait; they do not create a second implementation or a second representation of state. Internal code, examples, and normal documentation use the new names. Old spellings are explicitly documented in the migration guide and marked deprecated where supported.

| Old spelling | Compatibility treatment |
|---|---|
| Core `FileHandle` | Deprecated re-export of the same `OpenFd` trait at existing core/API/VNFS import paths. Existing implementations remain implementations of that trait. |
| `vfsi_sync::FsFile` | Deprecated re-export of the same `OpenFile` struct, preserving its generic parameter and associated methods. |
| `vfsi_nfs::client::FileHandle` | Deprecated module-scoped re-export of `NfsFh`. This is a separate compatibility alias from core `FileHandle`. |
| `VfFile` | Deprecated re-export of `FileOperand`. This preserves type imports and unchanged associated constructors/variants, but does not preserve the renamed `Descriptor` variant. |
| `Attrs::filesystem_id()` / `FsObjId::filesystem_id()` | Deprecated methods forwarding to `fs_id()`, preserving return types and `const` behavior where currently present. |
| `AttrMask::FILESYSTEM_ID` | Deprecated associated constant equal to `AttrMask::FS_ID`, preserving the bit value without duplicate flag names. |
| `VfFile::Descriptor(fd)` | Source migration required: use `FileOperand::Fd(fd)`. Enum variants cannot be aliased for old pattern matches. Avoid a duplicate variant or a second operand enum. |
| `VfAttrs.filesystem_id` | Source migration required: use `.fs_id` or `VfAttrs { fs_id: ..., .. }`. Rust fields cannot have aliases; do not add duplicate storage. |
| `HandleId` | Remove. Search found no repository consumers, but downstream imports cannot be ruled out; list the removal as a Rust source change. |
| Private NFS `OpenFile` | Rename directly to `NfsOpenState`; no public compatibility alias. |
| Existing `FileIdentity` and `file_identity()` | Retain the existing compatibility surface, with `FsObjId` and `fs_obj_id()` remaining canonical. |

The variant/field changes and `HandleId` removal make this a Rust source migration even with the retained aliases. C exported names, layouts, numeric descriptor values, and Python public method names keep their current contracts; their Rust implementation types and comments migrate. Regenerate or inspect the C header through its existing build path and check for unintended ABI changes.

## Source and documentation coverage

| Area | Files or surfaces to update/audit |
|---|---|
| Core definitions and portable API | `crates/vfsi-core/src/types.rs`, `lib.rs`, `api/{mod,handles,vfsi,read,extensions,listdir,traversal}.rs`, native writes, and contract/doctest targets. |
| Sync and native dispatch | `crates/vfsi-sync/src/client/`, `backend/` and its helpers, `application.rs`, application macros, traits, exports, test support, and native/public tests. |
| Backend implementations | Local metadata/operand matches and contract tests; NFS client, `nfs.rs`, child modules, recovery/deferred cleanup/read pool/mount imports; SMB imports and operand matches. Audit POSIX/uring consumers and feature-specific compilation. |
| VNFS application facade | `lib.rs`, `application.rs`, `facade.rs`, `auto/`, public exports, helpers, examples, integration tests, and all feature-specific handle wrappers. |
| Bindings and detached workspaces | `bindings/c/src/lib.rs`, C header/build/tests; `adapters/vfsi-python`, `adapters/nfs4fs`, `adapters/vsmb`; Python stubs/comments when they describe this Rust API; `fuzz`. Keep third-party fsspec names intact. |
| Active documentation | `docs/RUST_API.md`, `docs/backend-migration.md`, `docs/RESULT_DESIGN.md`, VNFS `overview.md`, `api_boundary.md`, guides, helper README, crate/repository READMEs, and relevant release notes. Preserve historical release descriptions unless correcting a current instruction. |
| Repository-wide audit | Search tracked files including hidden workflow/configuration files, test fixtures, examples, generated outputs owned by this repository, and distribution metadata. Preserve backups, external protocol spellings, standard-library symbols, and third-party APIs. |

## Documentation rules and examples

Document each canonical term at its definition, with scope, ownership, and lifetime:

- `FsObjId` compares underlying objects within a backend/server identity namespace. IDs can be reused after deletion; this is not a durable globally unique identifier.
- `fs_id` selects the filesystem identity component, not a mount ID. Linux uses `st_dev`; NFS uses server FSID. Keep `device_id` separate because it represents a special file's device number.
- `OpenFd` names the owned File Descriptor interface to an open instance. It does not require exposing an OS raw fd, and this rename does not add a separate numeric open-instance ID.
- `OpenFile<F>` owns the existing backend handle and cleanup obligation; borrowed operations retain that instance's lifetime. Its saved path is diagnostic. Separate opens retain separate state, while equal object IDs describe the same underlying object.
- `Fd` is a backend-assigned numeric File Descriptor; `FileOperand::Fd` references it without owning or duplicating it. `FileOperand` also supports path and special operands.
- `NfsFh` is an opaque object reference used in NFS requests. `NfsOpenState` contains the protocol stateid and client cursor/policy; a stateid is not a portable object ID or a globally unique open-instance ID.
- Mount discovery resolves `Auto` or a mount-derived connection to the underlying filesystem. Open handles keep their selected connection, and later operations through those handles use it. Do not fold mount identity into `FsObjId` or add a mount-specific file identity as part of this rename.

Migration examples:

```rust
// Before: vfsi_core::FileHandle / vfsi_sync::FsFile<F>
// After:  vfsi_core::OpenFd / vfsi_sync::OpenFile<F>

// Before: VfFile::Descriptor(fd)
let operand = vfsi_core::FileOperand::Fd(fd);

// Before: attrs.filesystem_id() / object_id.filesystem_id()
let fs_id = attrs.fs_id();
let object_id = attrs.fs_obj_id();
let object_fs_id = object_id.map(|id| id.fs_id());

// Before: VfAttrs { filesystem_id: value, ..Default::default() }
let native = vfsi_core::VfAttrs {
    fs_id: value,
    ..Default::default()
};

// Before: AttrMask::FILESYSTEM_ID
let requested = vfsi_core::AttrMask::FS_ID;
```

These fragments show spelling changes. The migration ledger contains a self-contained example; the portable API doctests and public API regressions cover canonical imports and compatibility.

## Verification record

The root `AGENTS.md` selections were followed with the existing build profile,
root target directory, and normal deterministic fault/test features. Focused
Rust-command durations below exclude each runner's approximately one-second
check of the 16 Python CI-script regressions.

| Executed command / selection | Result | Recorded duration |
|---|---|---|
| `./scripts/test-rust.sh --package vnfs --test public_api --filter renamed_open_descriptors_and_operands_preserve_compatible_imports` | 1 regression passed, confirming canonical/legacy type and trait imports share one open instance and cleanup owner; old FS accessors/mask and operand alias remain usable. | 1 second on the final focused rerun |
| `./scripts/test-rust.sh --package vfsi-core --test portable_api` | 9 portable contract tests passed. | 2 seconds |
| `./scripts/test-rust.sh --package vfsi-sync --test native_client` | 56 native ownership/lifecycle tests passed. | 5 seconds |
| `./scripts/test-rust.sh --package vfsi-nfs --lib` | 87 unit/fault tests passed; 1 configured-mount test ignored. Includes the FSID/FILEID decoder regression. | 4 seconds |
| `./scripts/test-ci-local.sh smoke` | Passed; the required server-independent iteration selection actually executed. | 17 seconds |
| `./scripts/test-ci-local.sh check` | Formatting and Clippy passed for the workspace/all targets/all features and detached Python Rust packages. | 7 seconds on the successful rerun |
| `./scripts/test-rust.sh` | 662 Rust tests/doctests passed; 12 fixture-dependent tests ignored. Includes 18 VNFS public API tests, 48 local backend tests, 22 C binding Rust tests, and 4 shared native Python tests. Wrapper crates compiled successfully but contain no Rust tests. The runner's 16 Python CI-script checks also passed. | 72 seconds across recorded commands |
| `cargo doc --workspace --no-deps --all-features` | Passed with no rustdoc warnings; generated documentation for all workspace crates. | 40 seconds |
| `cargo check -p vnfs --no-default-features --lib` | Passed; 6 unused-import warnings in the no-backend configuration. | 2 seconds |
| `cargo check -p vnfs --no-default-features --features uring --lib` and `cargo check -p vnfs --no-default-features --features uring,posix --example uring_bench` | Passed with no warnings; Linux uring library and benchmark example compile without the default NFS features. | 2 additional seconds |
| `cargo check --manifest-path fuzz/Cargo.toml --locked --all-targets` | Detached fuzz workspace passed compilation. Fuzz execution was not run. | 17 seconds |
| `git diff --check` and tracked-symbol audit | Passed. Remaining old names are compatibility declarations/exports, explicit migration tests, or migration/history documentation. Removed `HandleId` and `WireFileHandle` have no Rust definitions/use sites. The private NFS `OpenFile` name is absent. | Less than 1 second |
| Generated/public C header comparison | Rebuilt generated `vfsi.h` matches its baseline byte-for-byte; both checked-in C headers retain their original SHA-256 fingerprints. | Less than 1 second |

Clippy initially flagged misleading variable reuse in the new compatibility test;
the variables were corrected, then the regression and full checks passed. The
full build also emitted an upstream libntirpc C declaration-style warning.
No unresolved compilation, test, formatting, Clippy, or rustdoc failures remain.

Counts exclude duplicated output from supervised child test processes. Core
identity documentation's new `FsObjId` example executed as a doctest; core
had 52 doctests (51 ordinary and one compile-fail), sync had 7, and VNFS had 113
(20 ordinary and 93 compile-fail). Compatibility spellings are exercised
intentionally under a narrow `allow(deprecated)` on the relevant regression.

### Coverage limits

The 12 ignored tests require configured direct NFS/mount fixtures: one NFS
read-only reconnect test, ten VNFS mount/Auto tests, and one canonical NFSv4.1/
NFSv4.2 workflow test. Live NFS/SMB suites, real io_uring execution, Python-level
extension/fsspec runtime suites, and C executable integration suites were not
run for this Rust naming migration. Canonical types preserve existing wire/state
and routing behavior; compilation is not claimed as live coverage.
The portable no-backend build's unused-import warnings are recorded above.

Aliases cover import/type/accessor compatibility. The public raw attribute
field rename, operand variant rename, and unused public type removal remain
source changes requiring the migration documented above.

## Inspection scope

Inspected core identity and handle definitions, sync owned handles, NFS wire
handles and open-state records, local metadata, facade/Auto implementations,
exports, C/Python consumers, Cargo feature boundaries, and existing test runners.
`HandleId` has no repository consumers beyond its declaration/implementation and
the documentation inventory. External downstream code was not available; the
compatibility section records the public Rust source changes that may affect it.
