//! Opaque application handles. No backend extraction or Deref escape hatch.
use crate::*;

macro_rules! owned_client {
    ($client:ident, $file:ident, $dir:ident, $backend:ty) => {
        /// Owned application client. Clones share one connection and its lock.
        #[derive(Debug, Clone)]
        pub struct $client {
            pub(crate) inner: vfsi_sync::FsClient<$backend>,
        }
        impl $client {
            /// Query paths and retained handles through the native metadata vector.
            pub(crate) fn vgetattrs_impl<P: vfsi_core::AsTarget<$file>>(
                &self,
                targets: &[P],
                options: AttrsOptions,
            ) -> vfsi_core::VecResult<Attrs> {
                self.inner.vgetattrs_projected_native(
                    targets,
                    options.fields(),
                    options.follow_final_symlink(),
                    |target| match target.as_target() {
                        Target::Path(path) => Target::Path(path),
                        Target::File(file) => Target::File(&file.inner),
                    },
                )
            }
            pub(crate) fn vfsync_impl(
                &self,
                files: &[&$file],
                mode: SyncMode,
            ) -> vfsi_core::VecResult<()> {
                self.inner
                    .vfsync_projected_native(files, mode, |file| &file.inner)
            }
            pub(crate) fn vstatfs_impl<P: vfsi_core::AsTarget<$file>>(
                &self,
                targets: &[P],
            ) -> vfsi_core::VecResult<FilesystemStats> {
                self.inner
                    .vstatfs_projected_native(targets, |target| match target.as_target() {
                        Target::Path(path) => Target::Path(path),
                        Target::File(file) => Target::File(&file.inner),
                    })
            }
            pub(crate) fn vsetattrs_impl<P: vfsi_core::AsTarget<$file>>(
                &self,
                updates: &[SetAttrsOp<P>],
            ) -> vfsi_core::VecResult<()> {
                self.inner
                    .vsetattrs_projected_native(updates, |target| match target.as_target() {
                        Target::Path(path) => Target::Path(path),
                        Target::File(file) => Target::File(&file.inner),
                    })
            }

            /// Create symbolic links, retaining each target's original text.
            pub(crate) fn vsymlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
            ) -> vfsi_core::VecResult<()> {
                self.inner.vsymlink(pairs)
            }
            /// Read symlink targets in input order.
            pub(crate) fn vreadlink_impl<P: AsRef<Path>>(
                &self,
                paths: &[P],
            ) -> vfsi_core::VecResult<std::path::PathBuf> {
                self.inner.vreadlink(paths)
            }
            /// Create hard links in native backend batches.
            pub(crate) fn vhardlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
            ) -> vfsi_core::VecResult<()> {
                self.inner.vhardlink(pairs)
            }
            /// Return this client's allocation and traversal limits.
            pub(crate) fn limits_impl(&self) -> ResourceLimits {
                self.inner.limits()
            }
            /// Drain file/directory cleanup queued by Drop. Failed targets
            /// remain owned; explicit file close avoids this deferred path.
            /// Last-owner backend teardown can still block on network timeouts.
            pub fn drain_cleanup(&self) -> Result<()> {
                self.inner.drain_cleanup()
            }
            /// Configure this client view and its future clones. Existing clones keep
            /// their policy; all views still share the same connection and ownership.
            pub fn with_limits(self, limits: ResourceLimits) -> Self {
                Self {
                    inner: self.inner.with_limits(limits),
                }
            }
            /// Query the backend's supported operations.
            pub(crate) fn capabilities_impl(&self) -> Result<Capabilities> {
                self.inner.capabilities()
            }
            /// Rename source/destination pairs with per-pair atomic semantics.
            pub(crate) fn vrename_impl<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
                options: RenameMode,
            ) -> vfsi_core::VecResult<()> {
                self.inner.vrename(pairs, options)
            }
            /// Create directories in vector phases; parents must exist.
            /// An error can follow partially completed mutations.
            pub(crate) fn vmkdir_impl<P: AsRef<Path>>(
                &self,
                paths: &[MkDirOp<P>],
            ) -> vfsi_core::VecResult<()> {
                self.inner.vmkdir(paths)
            }
            pub(crate) fn vlistdir_pages_impl<P: AsRef<Path>>(
                &self,
                paths: &[P],
                cursors: Vec<Option<vfsi_core::api::DirectoryCursor>>,
                options: vfsi_core::api::ListDirOptions,
                page_size: std::num::NonZeroUsize,
            ) -> vfsi_core::VecResult<vfsi_core::api::DirectoryPage> {
                self.inner
                    .vlistdir_pages_impl(paths, cursors, options, page_size)
            }
            pub(crate) fn directory_page_batch_size(&self, _paths: &[&Path]) -> Result<usize> {
                self.inner.directory_page_batch_size()
            }
            pub(crate) fn read_dir_pages_with_fields(
                &self,
                paths: &[&Path],
                fields: crate::Attributes,
                cursors: Vec<Option<vfsi_sync::DirPageCursor>>,
                page_size: usize,
                max_entries: usize,
                follow_symlinks: bool,
            ) -> vfsi_core::VecResult<vfsi_sync::DirectoryPage> {
                self.inner.read_dir_pages_with_fields(
                    paths,
                    fields,
                    cursors,
                    page_size,
                    max_entries,
                    follow_symlinks,
                )
            }
            /// Copy whole files with independent per-input outcomes.
            pub(crate) fn vcopy_impl<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
                options: crate::CopyOptions,
            ) -> vfsi_core::VecResult<()> {
                self.inner.vcopy(pairs, options)
            }
            /// Remove paths with explicit error, batching, and retry policy.
            pub(crate) fn vremove_impl<P: AsRef<Path>>(
                &self,
                paths: &[P],
                mode: vfsi_core::api::RemoveMode,
                options: RemoveOptions,
            ) -> vfsi_core::VecResult<()> {
                self.inner.vremove_impl(paths, mode, options)
            }
            /// Acquire each successful input's owned handle independently.
            pub(crate) fn vopen_impl(&self, requests: &[OpenOp]) -> vfsi_core::VecResult<$file> {
                self.inner.vopen(requests).map(|outcomes| {
                    outcomes
                        .into_iter()
                        .map(|result| result.map(|inner| $file { inner }))
                        .collect()
                })
            }
            pub(crate) fn vopen_dirs_impl<P: AsRef<Path>>(
                &self,
                paths: &[P],
            ) -> vfsi_core::VecResult<$dir> {
                self.inner.vopen_dirs(paths).map(|dirs| {
                    dirs.into_iter()
                        .map(|result| result.map(|inner| $dir { inner }))
                        .collect()
                })
            }
            pub(crate) fn vremove_dir_contents_impl(
                &self,
                dirs: &[&$dir],
                options: RemoveOptions,
            ) -> vfsi_core::VecResult<()> {
                let dirs: Vec<_> = dirs.iter().map(|dir| &dir.inner).collect();
                self.inner.vremove_dir_contents(&dirs, options)
            }
            /// Close one native batch, retiring each confirmed success and
            /// retaining cleanup ownership for unsuccessful inputs.
            pub(crate) fn vclose_impl<'a>(
                &self,
                files: impl IntoIterator<Item = &'a mut $file>,
            ) -> vfsi_core::VecResult<()> {
                self.inner
                    .vclose(files.into_iter().map(|file| &mut file.inner))
            }
            /// Read with an explicit aggregate byte budget. See [`Vfsi::vread`].
            pub(crate) fn vread_impl<'a>(
                &self,
                ops: impl IntoIterator<Item = ReadOp<'a, $file>>,
                options: ReadOptions,
            ) -> vfsi_core::VecResult<ReadResult> {
                crate::read::consume_ops(
                    ops,
                    options.max_total_bytes_or(self.limits().max_read_bytes()),
                    |file, offset, length| file.inner.read_request_at(offset, length),
                    |file, offset, buffer| file.inner.read_request_at_into(offset, buffer),
                    |requests, budget| {
                        vfsi_sync::application::read_backend_owned(&self.inner, requests, budget)
                    },
                    |requests, bytes| self.inner.vread_into_with_limit_native(requests, bytes),
                )
            }
            /// Write independent positional ranges; short writes are reported and effects are not atomic.
            pub(crate) fn write_partial_native(
                &self,
                requests: &[WriteOp<'_, $file>],
            ) -> vfsi_core::VecResult<WriteResult> {
                self.inner.vwrite_mapped_native(requests, |op| {
                    op.file().inner.write_request_at(op.offset(), op.data())
                })
            }
            /// Write every byte in each positional request, retrying short writes in
            /// vector waves. Like `vwrite_native`, this is not transactional: an error may
            /// follow a successfully written prefix. Overlapping requests through the
            /// same object preserve mutation dependencies, including hard-link aliases.
            pub(crate) fn write_complete(
                &self,
                requests: &[WriteOp<'_, $file>],
            ) -> vfsi_core::VecResult<WriteResult> {
                self.inner.vwrite_all_mapped_native(requests, |op| {
                    op.file().inner.write_request_at(op.offset(), op.data())
                })
            }
        }
        /// Opened object with private backend ownership; closes best-effort on Drop.
        #[derive(Debug)]
        pub struct $file {
            pub(crate) inner: vfsi_sync::OpenFile<$backend>,
        }
        impl $file {
            /// Whether explicit close has completed successfully on this handle.
            pub fn is_closed(&self) -> bool {
                self.inner.is_closed()
            }
            /// Name used at open, retained for diagnostics; not updated after rename.
            pub fn path(&self) -> &Path {
                self.inner.path()
            }
            /// Keep cleanup ownership on failure so the caller can retry explicitly.
            pub fn try_close(&mut self) -> Result<()> {
                self.inner.try_close()
            }
            /// Consume and close the handle. On failure, `Drop` queues cleanup
            /// for a later operation or drain; use `try_close` to retain control.
            pub fn close(self) -> Result<()> {
                self.inner.close()
            }
        }
        /// An opened directory, never a publicly extractable backend token.
        #[derive(Debug)]
        pub struct $dir {
            inner: vfsi_sync::FsDir<$backend>,
        }
        impl $dir {
            /// Name used at open, retained for diagnostics; not updated after rename.
            pub fn path(&self) -> &std::path::Path {
                self.inner.path()
            }
            /// Whether explicit close has completed successfully on this handle.
            pub fn is_closed(&self) -> bool {
                self.inner.is_closed()
            }
            /// Keep cleanup ownership on failure so the caller can retry explicitly.
            pub fn try_close(&mut self) -> Result<()> {
                self.inner.try_close()
            }
            /// Consume and close the handle. On failure, `Drop` queues cleanup
            /// for a later operation or drain; use `try_close` to retain control.
            pub fn close(mut self) -> Result<()> {
                self.try_close()
            }
        }
    };
}
#[cfg(feature = "nfs")]
mod nfs {
    use super::*;
    use std::path::Path;
    owned_client!(NfsClient, NfsFile, NfsDir, vfsi_nfs::NfsVecFs);
}
#[cfg(feature = "nfs")]
pub use nfs::*;
#[cfg(all(feature = "posix", unix))]
mod posix {
    use super::*;
    use std::path::Path;
    owned_client!(Posix, PosixFile, PosixDir, vfsi_local::LocalBackend);
    impl Posix {
        /// Access this existing host directory through kernel filesystem operations.
        /// Namespace rooting is not a security sandbox.
        pub fn new(root: impl AsRef<Path>) -> Result<Self> {
            vfsi_posix::connect(root).map(|inner| Self { inner })
        }
    }
}
#[cfg(all(feature = "posix", unix))]
pub use posix::*;

#[cfg(all(feature = "uring", target_os = "linux"))]
mod uring {
    use super::*;
    use std::path::Path;
    owned_client!(Uring, UringFile, UringDir, vfsi_local::LocalBackend);
    impl Uring {
        /// Open a Linux local backend with bounded io_uring descriptor batches.
        /// Setup errors are reported; no implicit syscall-backend fallback.
        pub fn new(root: impl AsRef<Path>) -> Result<Self> {
            Self::with_options(root, vfsi_uring::Options::default())
        }

        pub fn with_options(root: impl AsRef<Path>, options: vfsi_uring::Options) -> Result<Self> {
            vfsi_uring::connect(root, options).map(|inner| Self { inner })
        }

        /// Return actual ring submission counters alongside the opaque client.
        pub fn with_telemetry(
            root: impl AsRef<Path>,
            options: vfsi_uring::Options,
        ) -> Result<(Self, vfsi_uring::Telemetry)> {
            vfsi_uring::connect_with_telemetry(root, options)
                .map(|(inner, telemetry)| (Self { inner }, telemetry))
        }
    }
}
#[cfg(all(feature = "uring", target_os = "linux"))]
pub use uring::*;
