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
            ) -> Result<Vec<Attrs>> {
                let targets: Vec<_> = targets
                    .iter()
                    .map(|target| match target.as_target() {
                        Target::Path(path) => Target::Path(path),
                        Target::File(file) => Target::File(&file.inner),
                    })
                    .collect();
                self.inner.vgetattrs(&targets, options)
            }
            pub(crate) fn vfsync_impl(&self, files: &[&$file], mode: SyncMode) -> Result<()> {
                let files: Vec<_> = files.iter().map(|file| &file.inner).collect();
                self.inner.vfsync(&files, mode)
            }
            /// Query filesystem statistics for paths and this client's open handles.
            pub(crate) fn vstatfs_impl<P: vfsi_core::AsTarget<$file>>(
                &self,
                targets: &[P],
            ) -> Result<Vec<FilesystemStats>> {
                let targets: Vec<_> = targets
                    .iter()
                    .map(|target| match target.as_target() {
                        vfsi_core::Target::Path(path) => vfsi_core::Target::Path(path),
                        vfsi_core::Target::File(file) => vfsi_core::Target::File(&file.inner),
                    })
                    .collect();
                self.inner.vstatfs(&targets)
            }
            /// Update selected metadata fields for many paths in one backend vector.
            pub(crate) fn vsetattrs_impl<P: vfsi_core::AsTarget<$file>>(
                &self,
                updates: &[SetAttrsOp<P>],
            ) -> Result<()> {
                let updates: Vec<_> = updates
                    .iter()
                    .map(|op| {
                        let target = match op.target().as_target() {
                            vfsi_core::Target::Path(path) => vfsi_core::Target::Path(path),
                            vfsi_core::Target::File(file) => vfsi_core::Target::File(&file.inner),
                        };
                        op.with_target(target)
                    })
                    .collect();
                self.inner.vsetattrs(&updates)
            }
            /// Create symbolic links, retaining each target's original text.
            pub(crate) fn vsymlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
            ) -> Result<()> {
                self.inner.vsymlink(pairs)
            }
            /// Read symlink targets in input order.
            pub(crate) fn vreadlink_impl<P: AsRef<Path>>(
                &self,
                paths: &[P],
            ) -> Result<Vec<std::path::PathBuf>> {
                self.inner.vreadlink(paths)
            }
            /// Create hard links in native backend batches.
            pub(crate) fn vhardlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
            ) -> Result<()> {
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
                options: RenameOptions,
            ) -> Result<()> {
                self.inner.vrename(pairs, options)
            }
            /// Create directories in vector phases; parents must exist.
            /// An error can follow partially completed mutations.
            pub(crate) fn vmkdir_impl<P: AsRef<Path>>(&self, paths: &[MkDirOp<P>]) -> Result<()> {
                self.inner.vmkdir(paths)
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
            ) -> Result<Vec<vfsi_sync::DirectoryPage>> {
                self.inner.read_dir_pages_with_fields(
                    paths,
                    fields,
                    cursors,
                    page_size,
                    max_entries,
                    follow_symlinks,
                )
            }
            /// Empty a directory while keeping it, with explicit removal policy.
            pub(crate) fn remove_dir_contents_impl(
                &self,
                path: impl AsRef<Path>,
                options: RemoveOptions,
            ) -> Result<()> {
                self.inner.remove_dir_contents_with_options(path, options)
            }
            /// Copy whole files in request order. A successful prefix may remain if
            /// a later request fails; this operation does not provide atomicity.
            pub(crate) fn vcopy_impl<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
                options: crate::CopyOption,
            ) -> Result<()> {
                self.inner.vcopy(pairs, options)
            }
            /// Remove paths with explicit error, batching, and retry policy.
            pub(crate) fn vremove_impl<P: AsRef<Path>>(
                &self,
                paths: &[P],
                recursive: bool,
                options: RemoveOptions,
            ) -> Result<()> {
                self.inner.vremove_impl(paths, recursive, options)
            }
            /// Open an ordered vector of files.
            ///
            /// Success returns one RAII handle per request. Failure returns no
            /// handles; VFSI does not promise transactional rollback of other
            /// filesystem effects such as file creation.
            pub(crate) fn vopen_impl(&self, requests: &[OpenOp]) -> Result<Vec<$file>> {
                self.inner
                    .vopen(requests)
                    .map(|files| files.into_iter().map(|inner| $file { inner }).collect())
            }
            pub(crate) fn vopen_dirs_impl<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<$dir>> {
                self.inner
                    .vopen_dirs(paths)
                    .map(|dirs| dirs.into_iter().map(|inner| $dir { inner }).collect())
            }
            pub(crate) fn vremove_dir_contents_impl(
                &self,
                dirs: &[&$dir],
                options: RemoveOptions,
            ) -> Result<()> {
                let dirs: Vec<_> = dirs.iter().map(|dir| &dir.inner).collect();
                self.inner.vremove_dir_contents(&dirs, options)
            }
            /// Try to close a group through one vector operation without consuming
            /// the handles. On failure, all handles remain armed: the backend may
            /// have closed a prefix, so callers must reconcile before retrying.
            pub(crate) fn vclose_impl<'a>(
                &self,
                files: impl IntoIterator<Item = &'a mut $file>,
            ) -> Result<()> {
                self.inner
                    .vclose(files.into_iter().map(|file| &mut file.inner))
            }
            /// Read with an explicit aggregate byte budget. See [`Vfsi::vread`].
            pub(crate) fn vread_impl<'a>(
                &self,
                ops: impl IntoIterator<Item = ReadOp<'a, $file>>,
                options: ReadOptions,
            ) -> Result<Vec<ReadResult>> {
                crate::read::consume_ops(
                    ops,
                    options.limit_or(self.limits().read_byte_limit()),
                    |file, offset, length| file.inner.read_request_at(offset, length),
                    |file, offset, buffer| file.inner.read_request_at_into(offset, buffer),
                    |requests, budget| {
                        vfsi_sync::application::read_backend_owned(&self.inner, requests, budget)
                    },
                    |requests, bytes| self.inner.vread_into_with_limit_native(requests, bytes),
                )
            }
            /// Write ordered positional ranges; short writes are reported and effects are not atomic.
            pub(crate) fn write_partial_native(
                &self,
                requests: &[WriteOp<'_, $file>],
            ) -> Result<Vec<WriteResult>> {
                self.inner.vwrite_mapped_native(requests, |op| {
                    op.file().inner.write_request_at(op.offset(), op.data())
                })
            }
            /// Write every byte in each positional request, retrying short writes in
            /// vector waves. Like `vwrite_native`, this is not transactional: an error may
            /// follow a successfully written prefix. Overlapping requests through the
            /// same path complete in input order; different paths are presumed
            /// independent (including hard-link aliases).
            pub(crate) fn write_complete(
                &self,
                requests: &[WriteOp<'_, $file>],
            ) -> Result<Vec<WriteResult>> {
                self.inner.vwrite_all_mapped_native(requests, |op| {
                    op.file().inner.write_request_at(op.offset(), op.data())
                })
            }
        }
        impl $client {
            pub(crate) fn open_native(&self, request: OpenOp) -> Result<$file> {
                self.inner.open_with(request).map(|inner| $file { inner })
            }
            pub(crate) fn stream_native(
                &self,
                path: impl AsRef<Path>,
                options: StreamOptions,
                callback: impl FnMut(u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
            ) -> Result<StreamCompletion> {
                self.inner.read_stream_with_options(path, options, callback)
            }
        }
        /// Opened object with private backend ownership; closes best-effort on Drop.
        #[derive(Debug)]
        pub struct $file {
            pub(crate) inner: vfsi_sync::FsFile<$backend>,
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
        /// Access this host directory through kernel filesystem operations.
        /// Namespace rooting is not a security sandbox.
        pub fn new(root: impl AsRef<Path>) -> Result<Self> {
            let root = root.as_ref();
            if !root.is_dir() {
                return Err(Error::client(0, libc::ENOTDIR as u32).with_context("posix", root));
            }
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
