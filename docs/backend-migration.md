# Native backend migration ledger

The `VecFs` and `VecFsExt` traits and their blanket native adapters have been
removed in a deliberate breaking Rust migration. Concrete types `DummyVecFs`,
`NfsVecFs`, and `SmbVecFs` keep their names and implement operation facets
directly. `NativeFileSystem` and `Backend` contain no methods. Marker blanket
implementations only aggregate already implemented contracts; they do not
synthesize batching or provide reciprocal adapters.

Shared defaults are free functions in `vfsi-sync/src/backend_helpers.rs`,
generic over the required facets. Trait defaults delegate to those functions;
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
object-safe `Backend` aggregate. Native test support and integration tests call
the operation facets directly.

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
| `readv` | `VectorFileSystem::vread_impl` | local, NFS, SMB |
| `readv_into` | `VectorFileSystem::vread_into_impl` | local, NFS |
| `read_allv` | `ReadWorkflowFileSystem::vread_all_impl` | — |
| `read_allv_with_options` | `ReadWorkflowFileSystem::vread_all_with_options_impl` | NFS |
| `writev` | `VectorFileSystem::vwrite_owned_impl` | local, NFS, SMB |
| `writev_borrowed` | `VectorFileSystem::vwrite_impl` | local, NFS, SMB |
| `fseek` | `FileSystem::seek_raw_impl` | local, NFS, SMB |
| `getattrsv` | `MetadataFileSystem::vgetattrs_impl` | local, NFS, SMB |
| `lgetattrsv` | `MetadataFileSystem::vgetattrs_nofollow_impl` | local, NFS, SMB |
| `setattrsv` | `MetadataFileSystem::vsetattrs_raw_impl` | local, NFS, SMB |
| `lsetattrsv` | `MetadataFileSystem::vsetattrs_raw_nofollow_impl` | local, NFS, SMB |
| `listdir` | `DirectoryFileSystem::listdir_impl` | local, NFS, SMB |
| `listdir_page` | `DirectoryFileSystem::listdir_page_impl` | local, NFS |
| `directory_page_batch_size` | `DirectoryFileSystem::directory_page_batch_size` | NFS |
| `listdir_pages` | `DirectoryFileSystem::vlistdir_pages_impl` | NFS |
| `walk` | `TraversalFileSystem::walk_impl` | — |
| `walk_with_options` | `TraversalFileSystem::walk_with_options_impl` | NFS |
| `renamev` | `NamespaceFileSystem::vrename_impl` | local, NFS, SMB |
| `removev` | `NamespaceFileSystem::vremove_impl` | local, NFS, SMB |
| `mkdirv` | `NamespaceFileSystem::vmkdir_impl` | local, NFS, SMB |
| `symlinkv` | `LinkFileSystem::vsymlink_impl` | local, NFS, SMB |
| `readlinkv` | `LinkFileSystem::vreadlink_impl` | local, NFS, SMB |
| `hardlinkv` | `LinkFileSystem::vhardlink_impl` | local, NFS, SMB |
| `dupv` | `CopyFileSystem::vcopy_data_impl` | local, NFS, SMB |
| `write_adb` | `ApplicationDataFileSystem::vwrite_adb_impl` | local, NFS, SMB |
| `read_streamv` | `ReadWorkflowFileSystem::vstream_impl` | — |
| `rm` | `RemovalFileSystem::remove_paths_impl` | — |
| `rm_with_options` | `RemovalFileSystem::remove_paths_with_options_impl` | NFS |
| `open_dir` | `RemovalFileSystem::open_dir_impl` | NFS |
| `rm_dir_contents` | `RemovalFileSystem::remove_dir_contents_handle_impl` | — |
| `rm_dir_contents_with_options` | `RemovalFileSystem::remove_dir_contents_handle_with_options_impl` | NFS |
| `close_dir` | `RemovalFileSystem::close_dir_impl` | NFS |
| `rm_contents` | `RemovalFileSystem::remove_dir_contents_path_impl` | — |
| `rm_contents_with_options` | `RemovalFileSystem::remove_dir_contents_path_with_options_impl` | NFS |
| `ensure_empty_dir` | `RemovalFileSystem::ensure_empty_dir_impl` | — |
| `cp_recursive` | `CopyFileSystem::copy_tree_impl` | local, NFS, SMB |
| `vf_path` | `FileSystem::vf_path` | — |
| `open` | `FileSystem::open_raw_impl` | — |
| `read` | `FileSystem::read_raw_impl` | — |
| `write` | `FileSystem::write_raw_impl` | — |
| `open_many` | `VectorFileSystem::vopen_outcomes_impl` | local, NFS, SMB |
| `before_open_cleanup` | `VectorFileSystem::before_open_cleanup` | local, NFS, SMB |
| `before_remove_type` | `RemovalFileSystem::before_remove_type` | local |
| `openv` | `VectorFileSystem::vopen_raw_impl` | — |
| `openv_simple` | `VectorFileSystem::vopen_raw_simple_impl` | — |
| `closev` | `VectorFileSystem::vclose_impl` | NFS, SMB |
| `stat` | `MetadataFileSystem::stat_impl` | — |
| `lstat` | `MetadataFileSystem::lstat_impl` | — |
| `fstat` | `MetadataFileSystem::fstat_impl` | — |
| `exists` | `MetadataFileSystem::exists_impl` | SMB |
| `file_type` | `MetadataFileSystem::file_type_impl` | SMB |
| `listdirv` | `TraversalFileSystem::vlistdirs_impl` | NFS |
| `visit_dir` | `TraversalFileSystem::visit_dir_impl` | local, NFS |
| `unlink` | `NamespaceFileSystem::unlink_impl` | — |
| `unlinkv` | `NamespaceFileSystem::vunlink_impl` | — |
| `mkdir` | `NamespaceFileSystem::mkdir_raw_impl` | — |
| `symlink` | `LinkFileSystem::symlink_raw_impl` | — |
| `readlink` | `LinkFileSystem::readlink_raw_impl` | — |
| `vcopy_impl` | `CopyFileSystem::vcopy_impl` | local, NFS, SMB |
| `ensure_dir` | `NamespaceFileSystem::ensure_dir_impl` | — |

## Caller migration

`FsClient` uses native facets and `Backend` where complete workflow support is
required. `VfFileHandle` needs only `VectorFileSystem`. Scalar-only clients remain
usable through `FileSystem`; they do not need the complete backend aggregate.

Concrete frontend vectors use `vopen`, `vclose`, `vrename`, `vmkdir`, and `vcopy`.
`vmkdir` accepts `(path, mode)` pairs. The default-mode paths-only helper is
`vmkdir_default`; consuming close is `vclose_owned`. Structured internal I/O
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
