//! Opaque application handles. No backend extraction or Deref escape hatch.
use crate::*;

macro_rules! owned_client {
    ($client:ident, $file:ident, $dir:ident, $open:ident, $set:ident, $read:ident, $into:ident, $write:ident, $backend:ty) => {
        /// Owned application client. Clones share one connection and its lock.
        #[derive(Debug, Clone)]
        pub struct $client {
            pub(crate) inner: vfsi_sync::FsClient<$backend>,
        }
        impl $client {
            /// Incremental no-follow tree events; prune before fetching contents.
            pub fn walk_events_with_options(
                &self,
                root: impl AsRef<Path>,
                fields: MetadataFields,
                options: WalkOptions,
                sort_by_name: bool,
                callback: impl FnMut(&WalkEvent) -> Result<WalkControl>,
            ) -> Result<TraversalCompletion> {
                self.inner
                    .walk_events_with_options(root, fields, options, sort_by_name, callback)
            }
            /// Return this client's allocation and traversal limits.
            pub fn limits(&self) -> ResourceLimits {
                self.inner.limits()
            }
            /// Configure this client view and its future clones. Existing clones keep
            /// their policy; all views still share the same connection and ownership.
            pub fn with_limits(self, limits: ResourceLimits) -> Self {
                Self {
                    inner: self.inner.with_limits(limits),
                }
            }
            /// Query the backend's supported operations.
            pub fn capabilities(&self) -> Result<Capabilities> {
                self.inner.capabilities()
            }
            /// Read an entire file within this client's configured byte budget.
            pub fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
                self.inner.read(path)
            }
            /// Read one complete file while limiting the returned allocation.
            ///
            /// Use `owned file::read_native` or `std::io::Read` to stream files that
            /// should not be held in one allocation.
            pub fn read_with_limit(
                &self,
                path: impl AsRef<Path>,
                max_bytes: usize,
            ) -> Result<Vec<u8>> {
                self.inner.read_with_limit(path, max_bytes)
            }
            /// Read a UTF-8 file within this client's byte limit.
            pub fn read_to_string(&self, path: impl AsRef<Path>) -> Result<String> {
                self.inner.read_to_string(path)
            }
            /// Read one complete UTF-8 file with a caller-selected allocation limit.
            pub fn read_to_string_with_limit(
                &self,
                path: impl AsRef<Path>,
                max_bytes: usize,
            ) -> Result<String> {
                self.inner.read_to_string_with_limit(path, max_bytes)
            }
            /// Create or truncate a file and write all supplied bytes; not atomic.
            pub fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
                self.inner.write(path, data)
            }
            /// Stream one file from offset zero in bounded chunks.
            ///
            /// The callback runs without holding the backend lock, so it may use this
            /// client or drop other files owned by it. Return `Ok(false)` to stop
            /// successfully. Callback errors
            /// are propagated. The file is closed on success, cancellation, callback
            /// error, or read error. At most one requested chunk is buffered at once.
            pub fn read_stream(
                &self,
                path: impl AsRef<Path>,
                callback: impl FnMut(u64, &[u8]) -> Result<bool>,
            ) -> Result<StreamCompletion> {
                self.inner.read_stream(path, callback)
            }
            /// Stream one file using an explicit maximum chunk size.
            pub fn read_stream_with_options(
                &self,
                path: impl AsRef<Path>,
                options: ReadStreamOptions,
                callback: impl FnMut(u64, &[u8]) -> Result<bool>,
            ) -> Result<StreamCompletion> {
                self.inner.read_stream_with_options(path, options, callback)
            }
            /// Query metadata for the open object without resolving its path again.
            pub fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata> {
                self.inner.metadata(path)
            }
            /// Query metadata without following a final symlink.
            pub fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata> {
                self.inner.symlink_metadata(path)
            }
            /// Create one directory using the default permission mode.
            pub fn create_dir(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.create_dir(path)
            }
            /// Create directories in vector phases; parents must exist.
            /// An error can follow partially completed mutations.
            pub fn create_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<()> {
                self.inner.create_dirs(paths)
            }
            /// Create one directory with explicit Unix permission bits.
            pub fn create_dir_with_mode(&self, path: impl AsRef<Path>, mode: u32) -> Result<()> {
                self.inner.create_dir_with_mode(path, mode)
            }
            /// List one directory within the configured entry and path-byte limits.
            pub fn read_dir(&self, path: impl AsRef<Path>) -> Result<Vec<DirEntry>> {
                self.inner.read_dir(path)
            }
            /// Read one directory with explicit entry and path-storage limits.
            pub fn read_dir_with_options(
                &self,
                path: impl AsRef<Path>,
                options: ReadDirOptions,
            ) -> Result<Vec<DirEntry>> {
                self.inner.read_dir_with_options(path, options)
            }
            /// Visit one directory one bounded page at a time. `Continue(())` requests
            /// the next entry; `Break(())` stops the entire visit successfully.
            /// The callback runs without the lock and may reenter or drop its files.
            pub fn visit_dir(
                &self,
                path: impl AsRef<Path>,
                callback: impl FnMut(DirEntry) -> Result<std::ops::ControlFlow<()>>,
            ) -> Result<TraversalCompletion> {
                self.inner.visit_dir(path, callback)
            }
            /// Visit entries with explicit entry and cumulative path-byte limits.
            pub fn visit_dir_with_options(
                &self,
                path: impl AsRef<Path>,
                options: ReadDirOptions,
                callback: impl FnMut(DirEntry) -> Result<std::ops::ControlFlow<()>>,
            ) -> Result<TraversalCompletion> {
                self.inner.visit_dir_with_options(path, options, callback)
            }
            /// Create missing parents and the requested directory.
            pub fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.create_dir_all(path)
            }
            /// Remove a non-directory entry; symbolic links are not followed.
            pub fn remove_file(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.remove_file(path)
            }
            /// Remove an empty directory.
            pub fn remove_dir(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.remove_dir(path)
            }
            /// Remove a directory tree without following symbolic links; not atomic.
            pub fn remove_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.remove_dir_all(path)
            }
            /// Remove the contents of a directory, keeping the directory itself.
            pub fn remove_dir_contents(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.remove_dir_contents(path)
            }
            /// Rename one entry within this client's namespace.
            pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
                self.inner.rename(from, to)
            }
            /// Visit a tree using bounded directory pages,
            /// never follows symlinks, and invokes the callback outside the backend
            /// lock. Unlike collecting `walk`, this trades multi-directory batching
            /// for bounded incremental delivery. A backend without native paging
            /// may retain one bounded listing; finish its pages before descending,
            /// so snapshots never accumulate across ancestor directories.
            /// `ControlFlow::Break(())` stops the entire walk successfully, not merely
            /// the current subtree. Directory order is backend-defined.
            pub fn visit_walk(
                &self,
                root: impl AsRef<Path>,
                callback: impl FnMut(&DirEntry) -> Result<std::ops::ControlFlow<()>>,
            ) -> Result<TraversalCompletion> {
                self.inner.visit_walk(root, callback)
            }
            /// Visit a tree incrementally with explicit traversal limits.
            pub fn visit_walk_with_options(
                &self,
                root: impl AsRef<Path>,
                options: $crate::WalkOptions,
                callback: impl FnMut(&DirEntry) -> Result<std::ops::ControlFlow<()>>,
            ) -> Result<TraversalCompletion> {
                self.inner.visit_walk_with_options(root, options, callback)
            }
            /// List several directories with common stat attributes and finite
            /// allocation limits. Use `read_dirs_with_options` for richer fields.
            pub fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>> {
                self.inner.read_dirs(paths)
            }
            /// Recursively enumerate a bounded tree with common stat attributes.
            /// Use `walk_with_options` to select fields or change limits.
            pub fn walk(&self, root: impl AsRef<Path>) -> Result<Vec<DirectoryListing>> {
                self.inner.walk(root)
            }
            /// Create `path` if missing, otherwise empty it. Errors if it exists and is
            /// not a directory (a symlink to a directory is not a directory here).
            pub fn ensure_empty_dir(&self, path: impl AsRef<Path>) -> Result<()> {
                self.inner.ensure_empty_dir(path)
            }
            /// Remove a directory tree with explicit error, batching, and retry policy.
            pub fn remove_dir_all_with_options(
                &self,
                path: impl AsRef<Path>,
                options: RemoveOptions,
            ) -> Result<()> {
                self.inner.remove_dir_all_with_options(path, options)
            }
            /// Empty a directory while keeping it, with explicit removal policy.
            pub fn remove_dir_contents_with_options(
                &self,
                path: impl AsRef<Path>,
                options: RemoveOptions,
            ) -> Result<()> {
                self.inner.remove_dir_contents_with_options(path, options)
            }
            /// Create a symbolic link whose contents are the supplied target.
            pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
                self.inner.symlink(target, link)
            }
            /// Create a hard link to the source object.
            pub fn hard_link(
                &self,
                source: impl AsRef<Path>,
                link: impl AsRef<Path>,
            ) -> Result<()> {
                self.inner.hard_link(source, link)
            }
            /// Read the target of a symbolic link.
            pub fn read_link(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
                self.inner.read_link(path)
            }
            /// Copy one file and return the number of bytes copied.
            pub fn copy(
                &self,
                source: impl AsRef<Path>,
                destination: impl AsRef<Path>,
            ) -> Result<()> {
                self.inner.copy(source, destination)
            }
            /// Fetch selected metadata for one path without following its final symlink.
            /// Unavailable fields remain `None` on `Metadata`.
            pub fn symlink_metadata_with_fields(
                &self,
                path: impl AsRef<Path>,
                fields: MetadataFields,
            ) -> Result<Metadata> {
                self.inner.symlink_metadata_with_fields(path, fields)
            }
            /// List multiple directories in a vector call. The limits apply to the
            /// aggregate returned entries and stored path bytes. Streaming backends
            /// apply these limits before collecting a full listing; a backend using
            /// the compatibility `visit_dir` fallback may buffer one directory first.
            /// An error discards the collected prefix; callers may retry individual
            /// directories if desired.
            pub fn read_dirs_with_options<P: AsRef<Path>>(
                &self,
                paths: &[P],
                fields: MetadataFields,
                options: ReadDirOptions,
            ) -> Result<Vec<DirectoryListing>> {
                self.inner.read_dirs_with_options(paths, fields, options)
            }
            /// Recursively enumerate directories with selected entry attributes.
            /// The walk is bounded by `options`; sorting and presentation remain the
            /// application's responsibility.
            pub fn walk_with_options(
                &self,
                root: impl AsRef<Path>,
                fields: MetadataFields,
                options: $crate::WalkOptions,
            ) -> Result<Vec<DirectoryListing>> {
                self.inner.walk_with_options(root, fields, options)
            }
            /// Copy whole files in request order. A successful prefix may remain if
            /// a later request fails; this operation does not provide atomicity.
            pub fn copy_files<P: AsRef<Path>, Q: AsRef<Path>>(
                &self,
                pairs: &[(P, Q)],
            ) -> Result<()> {
                self.inner.copy_files(pairs)
            }
            /// Remove paths in request order, optionally recursing into directories.
            /// A successful prefix may remain if a later path fails.
            pub fn remove_paths<P: AsRef<Path>>(&self, paths: &[P], recursive: bool) -> Result<()> {
                self.inner.remove_paths(paths, recursive)
            }
            /// Remove paths with explicit error, batching, and retry policy.
            pub fn remove_paths_with_options<P: AsRef<Path>>(
                &self,
                paths: &[P],
                recursive: bool,
                options: RemoveOptions,
            ) -> Result<()> {
                self.inner
                    .remove_paths_with_options(paths, recursive, options)
            }
            /// Fetch no-follow metadata for many paths using the backend's vector
            /// operation. Useful for routing without one metadata RPC per path.
            pub fn symlink_metadatav(&self, paths: &[&Path]) -> Result<Vec<Metadata>> {
                self.inner.symlink_metadatav(paths)
            }
            /// Read several complete files by path using vector READ operations.
            ///
            /// The aggregate returned data is limited to 16 MiB by default. Use
            /// `read_files_with_options` to choose a
            /// different limit, or stream large files instead. This is not a snapshot
            /// or an atomic operation across files.
            pub fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Vec<u8>>> {
                self.inner.read_files(paths)
            }
            /// Read several complete files with an explicit aggregate allocation limit.
            pub fn read_files_with_options<P: AsRef<Path>>(
                &self,
                paths: &[P],
                options: ReadAllOptions,
            ) -> Result<Vec<Vec<u8>>> {
                self.inner.read_files_with_options(paths, options)
            }
            /// Replace several files from borrowed buffers using vector OPEN, WRITE,
            /// and CLOSE phases. Identical path spellings are rejected before opening
            /// anything; aliases such as hard links are still the caller's responsibility.
            /// The batch is not transactional: an error may follow files already
            /// created or written. Large inputs should be chunked by the caller rather
            /// than held in memory solely for this convenience method.
            pub fn write_files<P: AsRef<Path>, B: AsRef<[u8]>>(
                &self,
                entries: &[(P, B)],
            ) -> Result<()> {
                self.inner.write_files(entries)
            }
            /// Open a path read-only.
            pub fn open(&self, path: impl AsRef<std::path::Path>) -> Result<$file> {
                self.inner.open(path).map(|inner| $file { inner })
            }
            /// Create or truncate a file and open it for writing.
            pub fn create(&self, path: impl AsRef<std::path::Path>) -> Result<$file> {
                self.inner.create(path).map(|inner| $file { inner })
            }
            /// Open a file using an explicit request.
            pub fn open_with(&self, request: OpenRequest) -> Result<$file> {
                self.inner.open_with(request).map(|inner| $file { inner })
            }
            /// Open an ordered vector of files.
            ///
            /// Success returns one RAII handle per request. Failure returns no
            /// handles; VFSI does not promise transactional rollback of other
            /// filesystem effects such as file creation.
            pub fn openv(&self, requests: &[OpenRequest]) -> Result<Vec<$file>> {
                self.inner
                    .openv(requests)
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
            pub fn try_closev<'a>(
                &self,
                files: impl IntoIterator<Item = &'a mut $file>,
            ) -> Result<()> {
                self.inner
                    .try_closev(files.into_iter().map(|file| &mut file.inner))
            }
            /// Close a group of files through one vector operation.
            ///
            /// On failure, the handles are dropped and the backend receives
            /// best-effort scalar cleanup attempts. Use `try_closev`
            /// to retain the handles after an error.
            pub fn closev(&self, mut files: Vec<$file>) -> Result<()> {
                self.try_closev(&mut files)
            }
            /// Read an ordered vector with a 16 MiB aggregate request limit.
            /// Use `readv_with_limit` to tune the limit or
            /// `readv_into` to provide bounded caller-owned buffers.
            pub fn readv(&self, requests: &[$read<'_>]) -> Result<Vec<ReadResult>> {
                self.readv_with_limit(requests, self.inner.limits().max_read_bytes)
            }
            /// Read an ordered vector with an explicit aggregate request limit.
            pub fn readv_with_limit(
                &self,
                requests: &[$read<'_>],
                bytes: usize,
            ) -> Result<Vec<ReadResult>> {
                self.inner
                    .readv_with_limit_projected(requests, bytes, |r| &r.inner)
            }
            /// Read ordered positional ranges into caller-owned buffers, within this client's budget.
            pub fn readv_into(&self, requests: &mut [$into<'_>]) -> Result<Vec<ReadIntoResult>> {
                self.readv_into_with_limit(requests, self.inner.limits().max_read_bytes)
            }
            /// Read into caller storage with an explicit aggregate buffer budget.
            /// This also bounds allocation in copying fallback implementations.
            pub fn readv_into_with_limit(
                &self,
                requests: &mut [$into<'_>],
                bytes: usize,
            ) -> Result<Vec<ReadIntoResult>> {
                self.inner.readv_into_with_limit_projected(
                    requests,
                    bytes,
                    |r| &r.inner,
                    |r| &mut r.inner,
                )
            }
            /// Write ordered positional ranges; short writes are reported and effects are not atomic.
            pub fn writev(&self, requests: &[$write<'_>]) -> Result<Vec<WriteResult>> {
                self.inner.writev_projected(requests, |r| &r.inner)
            }
            /// Write every byte in each positional request, retrying short writes in
            /// vector waves. Like `writev`, this is not transactional: an error may
            /// follow a successfully written prefix. Overlapping requests through the
            /// same path complete in input order; different paths are presumed
            /// independent (including hard-link aliases).
            pub fn write_allv(&self, requests: &[$write<'_>]) -> Result<Vec<WriteResult>> {
                self.inner.write_allv_projected(requests, |r| &r.inner)
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
            pub fn metadata(&self) -> Result<Metadata> {
                self.inner.metadata()
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
            pub fn set_len(&self, len: u64) -> Result<()> {
                self.inner.set_len(len)
            }
            /// Change permissions on the open file.
            pub fn set_permissions(&self, permissions: Permissions) -> Result<()> {
                self.inner.set_permissions(permissions)
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
            /// Consume and close the handle. On failure, `Drop` makes one best-effort
            /// cleanup attempt; use `try_close` to retain control.
            pub fn close(self) -> Result<()> {
                self.inner.close()
            }
            /// Borrow this handle for a positional vector read; the cursor is unchanged.
            pub fn read_request_at(&self, offset: u64, length: usize) -> $read<'_> {
                $read {
                    inner: self.inner.read_request_at(offset, length),
                }
            }
            /// Borrow this handle and caller storage for a positional vector read.
            pub fn read_request_at_into<'a>(
                &'a self,
                offset: u64,
                buffer: &'a mut [u8],
            ) -> $into<'a> {
                $into {
                    inner: self.inner.read_request_at_into(offset, buffer),
                }
            }
            /// Borrow this handle and payload for a positional vector write.
            pub fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> $write<'a> {
                $write {
                    inner: self.inner.write_request_at(offset, data),
                }
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
        /// Borrowed positional request; construction performs no I/O or allocation.
        pub struct $read<'a> {
            inner: vfsi_sync::FsRead<'a, $backend>,
        }
        /// Borrowed positional request into caller storage.
        pub struct $into<'a> {
            inner: vfsi_sync::FsReadInto<'a, $backend>,
        }
        /// Borrowed positional write request; payload is not copied.
        pub struct $write<'a> {
            inner: vfsi_sync::FsWrite<'a, $backend>,
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
            /// Consume and close the handle. On failure, `Drop` makes one best-effort
            /// cleanup attempt; use `try_close` to retain control.
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
            pub fn openv<P: AsRef<std::path::Path>>(&self, paths: &[P]) -> Result<Vec<$file>> {
                self.inner
                    .openv(paths)
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
    use std::path::{Path, PathBuf};
    owned_client!(
        NfsClient,
        NfsFile,
        NfsDir,
        NfsOpenOptions,
        NfsSetMetadata,
        NfsRead,
        NfsReadInto,
        NfsWrite,
        vfsi_nfs::NfsVecFs
    );
}
#[cfg(feature = "nfs")]
pub use nfs::*;
#[cfg(all(feature = "auto", target_os = "linux"))]
mod mounted {
    use super::*;
    use std::path::{Path, PathBuf};
    owned_client!(
        Mounted,
        MountedFile,
        MountedDir,
        MountedOpenOptions,
        MountedSetMetadata,
        MountedRead,
        MountedReadInto,
        MountedWrite,
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
