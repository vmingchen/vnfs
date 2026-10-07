# Native backend migration ledger

The `VecFs` and `VecFsExt` traits and their blanket native adapters have been
removed in a deliberate breaking Rust migration. Concrete types `DummyVecFs`,
`NfsVecFs`, and `SmbVecFs` keep their names. The native contracts are now
`FileSystem` (the minimum owned-handle contract) and `Backend: FileSystem`
(native vector engines and overridable workflows). The intermediate operation-
family traits and methodless aggregates were consolidated; no blanket
implementation synthesizes a backend from scalar I/O.

Shared defaults are free functions in `crates/vfsi-sync/src/backend_helpers/`,
grouped into handle, I/O, metadata, directory, namespace, link, copy, read, and
removal modules. They are generic over `FileSystem` or `Backend`, including
`?Sized` for dynamic dispatch. Trait defaults delegate to those functions;
backend overrides remain execution hooks. NFS compounds/recovery, SMB transport
and COPYCHUNK, and local anchored descriptors remain backend implementations.

## Contracts that need distinct names

- `vopen_impl` accepts typed requests and returns a strict ordered handle vector.
  `vopen_outcomes_impl` accepts raw flags and exposes indexed partial outcomes to
  the strict-open collector; `vopen_raw_impl` preserves the raw-flags boundary.
- `vwrite_impl` borrows payloads; `vwrite_owned_impl` accepts owned payloads.
- `vsetattrs_impl` accepts typed updates; `vsetattrs_raw_impl` and its no-follow
  variant accept raw attribute masks.
- `vcopy_impl` accepts extent pairs and `CopyOption`; `vcopy_data_impl` is the
  explicit client-copy engine. Following source symlinks defaults to true.
- Scalar complete-file collection uses `read_file_impl`.
- Lifecycle, configuration, notification, and negotiated-capability hooks keep
  descriptive names. Selected filesystem execution helpers get a backend suffix;
  wire-level method names and compound tags retain their existing vocabulary.

## Legacy method inventory

The last column lists preserved concrete overrides. Other entries use shared
defaults or explicitly unsupported optional seams for scalar-only implementations.
Application consumers are `FsClient`/`Vfsi`; C and Python bindings use the
object-safe `Backend` contract. Native test support and integration tests call
its operation engines directly.

| Legacy method | Target contract / method | Preserved overrides |
|---|---|---|
| `vstatfs_impl` | `FileSystem::vstatfs_impl` | local, NFS |
| `close_deferred` | `FileSystem::close_deferred` | NFS |
| `take_notifications` | `FileSystem::take_notifications` | NFS |
| `nfs_minorversion` | Protocol extension APIs; bindings-specific `BindingBackend` inspection | NFS |
| `smb_dialect` | Protocol extension APIs; bindings-specific `BindingBackend` inspection | SMB |
| `capabilities` | `FileSystem::capability_bits` | local, NFS, SMB |
| `typed_capabilities` | `FileSystem::capabilities` | — |
| `abs_path` | `FileSystem::abs_path` | local, NFS, SMB |
| `open_by_path` | `FileSystem::open_path_impl` | local, NFS, SMB |
| `close` | `FileSystem::close_impl` | local, NFS, SMB |
| `sync_data` | `FileSystem::sync_data` | local, NFS, SMB |
| `sync_all` | `FileSystem::sync_all` | local |
| `chdir` | `FileSystem::chdir` | local, NFS, SMB |
| `getcwd` | `FileSystem::getcwd` | local, NFS, SMB |
| `readv` | `Backend::vread_impl` | local, NFS, SMB |
| `readv_into` | `Backend::vread_into_impl` | local, NFS |
| `read_allv` | `Backend::vread_all_impl` | — |
| `read_allv_with_options` | `Backend::vread_all_with_options_impl` | NFS |
| `writev` | `Backend::vwrite_owned_impl` | local, NFS, SMB |
| `writev_borrowed` | `Backend::vwrite_impl` | local, NFS, SMB |
| `fseek` | `FileSystem::seek_raw_impl` | local, NFS, SMB |
| `getattrsv` | `Backend::vgetattrs_impl` | local, NFS, SMB |
| `lgetattrsv` | `Backend::vgetattrs_nofollow_impl` | local, NFS, SMB |
| `setattrsv` | `Backend::vsetattrs_raw_impl` | local, NFS, SMB |
| `lsetattrsv` | `Backend::vsetattrs_raw_nofollow_impl` | local, NFS, SMB |
| `listdir` | `Backend::listdir_impl` | local, NFS, SMB |
| `listdir_page` | `Backend::listdir_page_impl` | local, NFS |
| `directory_page_batch_size` | `Backend::directory_page_batch_size` | NFS |
| `listdir_pages` | `Backend::vlistdir_pages_impl` | NFS |
| `walk` | `Backend::walk_impl` | — |
| `walk_with_options` | `Backend::walk_with_options_impl` | NFS |
| `renamev` | `Backend::vrename_impl` | local, NFS, SMB |
| `removev` | `Backend::vremove_impl` | local, NFS, SMB |
| `mkdirv` | `Backend::vmkdir_impl` | local, NFS, SMB |
| `symlinkv` | `Backend::vsymlink_impl` | local, NFS, SMB |
| `readlinkv` | `Backend::vreadlink_impl` | local, NFS, SMB |
| `hardlinkv` | `Backend::vhardlink_impl` | local, NFS, SMB |
| `dupv` | `Backend::vcopy_data_impl` | local, NFS, SMB |
| `write_adb` | `Backend::vwrite_adb_impl` | local, NFS, SMB |
| `read_streamv` | `Backend::vstream_impl` | — |
| `rm` | `Backend::remove_paths_impl` | — |
| `rm_with_options` | `Backend::remove_paths_with_options_impl` | NFS |
| `open_dir` | `Backend::open_dir_impl` | NFS |
| `rm_dir_contents` | `Backend::remove_dir_contents_handle_impl` | — |
| `rm_dir_contents_with_options` | `Backend::remove_dir_contents_handle_with_options_impl` | NFS |
| `close_dir` | `Backend::close_dir_impl` | NFS |
| `rm_contents` | `Backend::remove_dir_contents_path_impl` | — |
| `rm_contents_with_options` | `Backend::remove_dir_contents_path_with_options_impl` | NFS |
| `ensure_empty_dir` | `Backend::ensure_empty_dir_impl` | — |
| `cp_recursive` | `Backend::copy_tree_impl` | local, NFS, SMB |
| `vf_path` | `FileSystem::vf_path` | — |
| `open` | `FileSystem::open_raw_impl` | — |
| `read` | `FileSystem::read_raw_impl` | — |
| `write` | `FileSystem::write_raw_impl` | — |
| `open_many` | `Backend::vopen_outcomes_impl` | local, NFS, SMB |
| `before_open_cleanup` | `Backend::before_open_cleanup` | local, NFS, SMB |
| `before_remove_type` | `Backend::before_remove_type` | local |
| `openv` | `Backend::vopen_raw_impl` | — |
| `openv_simple` | `Backend::vopen_raw_simple_impl` | — |
| `closev` | `Backend::vclose_impl` | NFS, SMB |
| `stat` | `Backend::stat_impl` | — |
| `lstat` | `Backend::lstat_impl` | — |
| `fstat` | `Backend::fstat_impl` | — |
| `exists` | `Backend::exists_impl` | SMB |
| `file_type` | `Backend::file_type_impl` | SMB |
| `listdirv` | `Backend::vlistdirs_impl` | NFS |
| `visit_dir` | `Backend::visit_dir_impl` | local, NFS |
| `unlink` | `Backend::unlink_impl` | — |
| `unlinkv` | `Backend::vunlink_impl` | — |
| `mkdir` | `Backend::mkdir_raw_impl` | — |
| `symlink` | `Backend::symlink_raw_impl` | — |
| `readlink` | `Backend::readlink_raw_impl` | — |
| `vcopy_impl` | `Backend::vcopy_impl` | local, NFS, SMB |
| `ensure_dir` | `Backend::ensure_dir_impl` | — |

## Caller migration

`FsClient` uses `FileSystem` for owned-handle operations and `Backend` for
vectors and namespace workflows. `VfFileHandle` uses `Backend`. Scalar-only
clients remain usable through `FileSystem`; they do not need `Backend`.

Application vectors use `vopen`, `vclose`, `vrename`, `vmkdir`, and `vcopy`.
`vmkdir` accepts `MkDirOp::new(path, mode)` operations; `vsetattrs` accepts
`SetAttrsOp::new(target)` operations with fluent attribute setters and an optional
`follow_symlinks(false)` policy. `VfsiExt::close_files` consumes handles.
Paths and opened objects use the shared `Target` enum and `AsTarget` conversion
trait; paths remain directly accepted without an explicit wrapper.
`SetAttrsOp::file(&file)` prepares a handle update. `MetadataTarget`,
`MetadataOperand`, `MetadataQuery`, and `SetAttributes` are removed rather than
retained as compatibility aliases. Native `metadata_impl` takes `Target<'_, VfFile>` plus
`AttrsOptions`, and native attribute setters borrow `SetAttrsOp<Target<'_, VfFile>>` operations
or slices. Timestamp/mask conversion is an execution detail; it does not require
a second public mutation type. C/Python entry points and wire formats are unchanged.
The native `FsClient` also has `vmkdir_default` and `vclose_owned` compatibility
helpers; these are not re-exported as application methods by `vnfs`. Structured internal I/O
adapters use `vread_native`/`vwrite_native` and corresponding semantic variants
to avoid shadowing portable `Vfsi` methods with different operands.

`VecFsExt` path aliases are removed. Application callers use `VfsiExt`; direct
backend callers pass `&Path` to native methods. `rm_recursive` is replaced by
`backend_helpers::remove_tree`. C/Python exported operations retain their names,
raw flags, partial-result behavior, and descriptor ownership. Protocol inspection
is implemented in binding-specific extension contracts.

## Guarantees retained

Strict-open cleanup validates indexed outcomes, closes confirmed handles on
failure, and preserves primary errors and failed-cleanup ownership. It does not
roll back file creation or truncation. Vector dispatch preserves native batches,
resource-retry classification, request order, and original error indices.

Collection and traversal preserve allocation/page/depth budgets, short-read and
EOF rules, cancellation, continuation ownership, and callback ordering.
Application callbacks run outside the backend lock; direct backend callbacks
retain their existing locking contract. Removal/copying preserve retained
identity, modes, offsets, lengths, truncation, symlink policy, and recovery.

Validation covers all-target workspace builds and Clippy, scalar/native/public
API tests, local containment and ownership, live NFS 4.1/4.2 batching and failure
injection, live SMB copy/cleanup, and separately built Python adapter features.

## Validation results

- Workspace all-target Clippy with NFS/SMB fault injection: passed with warnings denied.
- Formatting, whitespace, minimal-feature builds, and warnings-denied rustdoc: passed.
- Core portable and documentation tests, shared native-client contracts, local
  containment/ownership, public API/routing, and fault-injection tests: passed.
- Live NFS 4.1 and 4.2: 127 application/backend tests plus 26 native fault-injection
  tests per version passed.
- Live SMB: all 16 integration tests passed, including copy and failed-open cleanup.
- Five kernel-mount/direct-Auto routing tests passed using the existing NFS mount.
- C library build and local/SMB smoke consumers passed.
- Python native adapter (all features and dummy-only), nfs4fs, and vsmb consumers built.

Three specialized mount tests were not run: read-only mount policy, nested-mount
removal, and file bind-mount routing. Their dedicated fixtures were not configured.
The dummy-only Python build retains its pre-existing unused `host` warning.
