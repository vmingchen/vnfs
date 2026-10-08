//! Opaque application handles. No backend extraction or Deref escape hatch.
use crate::*;

macro_rules! owned_client {
    ($client:ident, $file:ident, $dir:ident, $open:ident, $set:ident, $backend:ty) => {
        /// Owned application client. Clones share one connection and its lock.
        #[derive(Debug, Clone)]
        pub struct $client {
            pub(crate) inner: vfsi_sync::FsClient<$backend>,
        }
        impl $client {
            /// Vector metadata with selected attributes and final-symlink handling.
            pub(crate) fn vgetattrs_impl<P: AsRef<Path>>(
                &self,
                paths: &[P],
                options: AttrsOptions,
            ) -> Result<Vec<Attrs>> {
                crate::metadata::metadata_backend(&self.inner, paths, options)
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
            /// Create `path` if missing, otherwise empty it. Errors if it exists and is
            /// not a directory (a symlink to a directory is not a directory here).
            pub fn ensure_empty_dir(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.ensure_empty_dir(path)
            }
            /// Empty a directory while keeping it, with explicit removal policy.
            pub fn remove_dir_contents_with_options(
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
            /// Remove paths in request order, optionally recursing into directories.
            /// A successful prefix may remain if a later path fails.
            pub fn vremove_native<P: AsRef<Path>>(
                &self,
                paths: &[P],
                recursive: bool,
            ) -> Result<()> {
                self.inner.vremove_native(paths, recursive)
            }
            /// Remove paths with explicit error, batching, and retry policy.
            pub fn vremove_with_options_native<P: AsRef<Path>>(
                &self,
                paths: &[P],
                recursive: bool,
                options: RemoveOptions,
            ) -> Result<()> {
                self.inner
                    .vremove_with_options_native(paths, recursive, options)
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
            /// Open a *genuine* directory handle for race-resistant, handle-rooted
            /// removal. Backends that only return a path fail instead of silently
            /// losing the handle safety guarantee.
            pub fn open_dir_handle(&self, path: impl AsRef<std::path::Path>) -> Result<$dir> {
                self.inner.open_dir_handle(path).map(|inner| $dir { inner })
            }
            /// Build reusable open flags for scalar or vector opens.
            pub fn open_options(&self) -> $open<'_> {
                $open {
                    inner: self.inner.open_options(),
                }
            }
            /// Build a metadata update for the given path.
            pub fn set_metadata(&self, path: impl AsRef<std::path::Path>) -> $set<'_> {
                $set {
                    inner: self.inner.set_metadata(path),
                }
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
                    options.limit_or(self.limits().max_read_bytes),
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
                callback: impl FnMut(u64, &[u8]) -> Result<bool>,
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
            /// Query metadata for the open object without resolving its path again.
            pub fn attrs(&self) -> Result<Attrs> {
                self.inner.attrs()
            }
            /// Positional read which does not alter the file cursor.
            pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize> {
                self.inner.read_at(buffer, offset)
            }
            /// Positional write which does not alter the file cursor.
            pub fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize> {
                self.inner.write_at(buffer, offset)
            }
            /// Read at the current cursor, retaining the native structured error.
            pub fn read_native(&mut self, buffer: &mut [u8]) -> Result<usize> {
                self.inner.read_native(buffer)
            }
            /// Collect the remaining bytes from this opened object, starting at its
            /// cursor, with an explicit logical payload limit. This does not reopen
            /// its path. Unlike standard `Read::read_to_end`, allocation is bounded.
            ///
            /// An exact-limit read uses at most a one-byte EOF probe. On overflow or
            /// I/O failure no buffer is returned and the cursor may have advanced,
            /// including the probe byte; this operation does not restore the cursor.
            pub fn read_to_end_with_limit(&mut self, max_bytes: usize) -> Result<Vec<u8>> {
                self.inner.read_to_end_with_limit(max_bytes)
            }
            /// Write at the current cursor, retaining the native structured error.
            pub fn write_native(&mut self, buffer: &[u8]) -> Result<usize> {
                self.inner.write_native(buffer)
            }
            /// Truncate or extend the open file.
            pub fn truncate(&self, len: u64) -> Result<()> {
                self.inner.truncate(len)
            }
            /// Change permissions on the open file.
            pub fn chmod(&self, permissions: Permissions) -> Result<()> {
                self.inner.chmod(permissions)
            }
            /// Request durable file data from the backend.
            pub fn sync_data(&self) -> Result<()> {
                self.inner.sync_data()
            }
            /// Request durable file data and metadata from the backend.
            pub fn sync_all(&self) -> Result<()> {
                self.inner.sync_all()
            }
            /// Keep cleanup ownership on failure so the caller can retry explicitly.
            pub fn try_close(&mut self) -> Result<()> {
                self.inner.try_close()
            }
            /// Seek while retaining `VfError` protocol and path information.
            pub fn seek_native(&mut self, position: std::io::SeekFrom) -> Result<u64> {
                self.inner.seek_native(position)
            }
            /// Consume and close the handle. On failure, `Drop` queues cleanup
            /// for a later operation or drain; use `try_close` to retain control.
            pub fn close(self) -> Result<()> {
                self.inner.close()
            }
        }
        impl std::io::Read for $file {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                std::io::Read::read(&mut self.inner, b)
            }
        }
        impl std::io::Write for $file {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                std::io::Write::write(&mut self.inner, b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                std::io::Write::flush(&mut self.inner)
            }
        }
        impl std::io::Seek for $file {
            fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
                std::io::Seek::seek(&mut self.inner, p)
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
            pub fn remove_contents(&self) -> Result<()> {
                self.inner.remove_contents()
            }
            pub fn remove_contents_with_options(&self, options: RemoveOptions) -> Result<()> {
                self.inner.remove_contents_with_options(options)
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
        /// Open-options builder tied to the owning application client.
        #[derive(Debug, Clone)]
        pub struct $open<'a> {
            inner: vfsi_sync::OpenOptions<'a, $backend>,
        }
        impl $open<'_> {
            /// Enable or disable read access.
            pub fn read(&mut self, enabled: bool) -> &mut Self {
                self.inner.read(enabled);
                self
            }
            pub fn write(&mut self, enabled: bool) -> &mut Self {
                self.inner.write(enabled);
                self
            }
            pub fn append(&mut self, enabled: bool) -> &mut Self {
                self.inner.append(enabled);
                self
            }
            pub fn truncate(&mut self, enabled: bool) -> &mut Self {
                self.inner.truncate(enabled);
                self
            }
            /// Create the file if missing; does not itself enable truncation.
            pub fn create(&mut self, enabled: bool) -> &mut Self {
                self.inner.create(enabled);
                self
            }
            pub fn create_new(&mut self, enabled: bool) -> &mut Self {
                self.inner.create_new(enabled);
                self
            }
            pub fn mode(&mut self, mode: u32) -> &mut Self {
                self.inner.mode(mode);
                self
            }
            /// Open one path with this builder's flags and permission mode.
            pub fn open(&self, path: impl AsRef<std::path::Path>) -> Result<$file> {
                self.inner.open(path).map(|inner| $file { inner })
            }
            /// Open an ordered vector of files.
            ///
            /// Success returns one RAII handle per request. Failure returns no
            /// handles; VFSI does not promise transactional rollback of other
            /// filesystem effects such as file creation.
            pub fn vopen<P: AsRef<std::path::Path>>(&self, paths: &[P]) -> Result<Vec<$file>> {
                self.inner
                    .vopen(paths)
                    .map(|files| files.into_iter().map(|inner| $file { inner }).collect())
            }
        }
        /// Metadata update builder with no raw attribute access.
        pub struct $set<'a> {
            inner: vfsi_sync::SetMetadata<'a, $backend>,
        }
        impl $set<'_> {
            pub fn permissions(&mut self, permissions: Permissions) -> &mut Self {
                self.inner.permissions(permissions);
                self
            }
            pub fn len(&mut self, len: u64) -> &mut Self {
                self.inner.len(len);
                self
            }
            pub fn accessed(&mut self, accessed: std::time::SystemTime) -> &mut Self {
                self.inner.accessed(accessed);
                self
            }
            pub fn uid(&mut self, uid: u32) -> &mut Self {
                self.inner.uid(uid);
                self
            }
            pub fn gid(&mut self, gid: u32) -> &mut Self {
                self.inner.gid(gid);
                self
            }
            pub fn modified(&mut self, modified: std::time::SystemTime) -> &mut Self {
                self.inner.modified(modified);
                self
            }
            pub fn follow_symlinks(&mut self, follow: bool) -> &mut Self {
                self.inner.follow_symlinks(follow);
                self
            }
            pub fn apply(&self) -> Result<()> {
                self.inner.apply()
            }
        }
    };
}
#[cfg(feature = "nfs")]
mod nfs {
    use super::*;
    use std::path::Path;
    owned_client!(
        NfsClient,
        NfsFile,
        NfsDir,
        NfsOpenOptions,
        NfsSetMetadata,
        vfsi_nfs::NfsVecFs
    );
}
#[cfg(feature = "nfs")]
pub use nfs::*;
#[cfg(all(feature = "auto", target_os = "linux"))]
mod mounted {
    use super::*;
    use std::path::Path;
    owned_client!(
        Mounted,
        MountedFile,
        MountedDir,
        MountedOpenOptions,
        MountedSetMetadata,
        vfsi_local::DummyVecFs
    );
    impl Mounted {
        /// Access this host directory through kernel filesystem operations.
        /// Namespace rooting is not a security sandbox.
        pub fn new(root: impl AsRef<Path>) -> Result<Self> {
            let root = root.as_ref();
            if !root.is_dir() {
                return Err(Error::client(0, libc::ENOTDIR as u32).with_context("mounted", root));
            }
            vfsi_local::DummyVecFs::try_new(root.to_path_buf())
                .map(vfsi_sync::FsClient::new)
                .map(|inner| Self { inner })
        }
    }
}
#[cfg(all(feature = "auto", target_os = "linux"))]
pub use mounted::*;
