//! Owned, shareable synchronous client and file handles.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom as IoSeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use vfsi_core::api::internal::OwnedReadResult as FsReadResult;
use vfsi_core::api::{
    DirectoryListing, ReadIntoResult as FsReadIntoResult, ResourceLimits, StreamCompletion,
    WriteResult as FsWriteResult,
};

use crate::traits::{validate_read_into_results, validate_read_results, validate_write_results};
use crate::{
    AttrMask, Attrs, Backend, Capabilities, DirEntry, FileSystem, OpenFlags, OpenOp, Permissions,
    ReadAllOptions, ReadOp, ReadResult, RemoveOptions, StreamOptions, VfDir, VfError, VfFile,
    VfOffset, VfResult, WriteOpRef, WriteResult,
};

fn read_result(result: ReadResult) -> FsReadResult {
    FsReadResult {
        offset: result.offset,
        data: result.data,
        eof: result.eof,
    }
}

fn write_result(result: WriteResult) -> FsWriteResult {
    FsWriteResult {
        offset: result.offset,
        written: result.written,
        stable: result.stable,
    }
}

fn io_error(error: VfError) -> io::Error {
    error.into()
}

fn poisoned() -> VfError {
    VfError::client(0, crate::ERR_IO)
}

fn wrong_result_count(operation: &str, expected: usize, actual: usize) -> VfError {
    VfError::transport_with_kind(
        None,
        crate::TransportKind::InvalidReply,
        format!("{operation} backend returned {actual} results for {expected} requests"),
    )
}

enum CleanupTarget {
    File(VfFile, PathBuf),
    Directory(VfDir, PathBuf),
}
struct PendingClose<F> {
    target: CleanupTarget,
    close: fn(&mut F, &CleanupTarget) -> VfResult<()>,
}

/// The cleanup queue has a separate short-held lock: handle Drop never waits
/// for an RPC or for the backend mutex. Final-owner teardown remains synchronous.
struct SharedBackend<F: FileSystem> {
    backend: Mutex<Option<F>>,
    pending: Mutex<Vec<PendingClose<F>>>,
    has_pending: AtomicBool,
}
impl<F: FileSystem> SharedBackend<F> {
    fn new(backend: F) -> Self {
        Self {
            backend: Mutex::new(Some(backend)),
            pending: Mutex::new(Vec::new()),
            has_pending: AtomicBool::new(false),
        }
    }
    fn defer(&self, target: CleanupTarget, close: fn(&mut F, &CleanupTarget) -> VfResult<()>) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(PendingClose { target, close });
        self.has_pending.store(true, Ordering::Release);
    }
    fn lock_without_cleanup(&self) -> Result<BackendGuard<'_, F>, ()> {
        self.backend
            .lock()
            .map(|guard| BackendGuard {
                guard: Some(guard),
                pending: &self.pending,
                has_pending: &self.has_pending,
            })
            .map_err(|_| ())
    }
    fn lock(&self) -> Result<BackendGuard<'_, F>, ()> {
        let mut guard = self.lock_without_cleanup()?;
        // Ordinary I/O must not lose its result to an unrelated cleanup error.
        // Retain failures; drain_cleanup is the explicit reporting boundary.
        let _ = guard.drain_cleanup();
        Ok(guard)
    }
}
impl<F: FileSystem> Drop for SharedBackend<F> {
    fn drop(&mut self) {
        if let Some(backend) = self
            .backend
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            let pending = self.pending.get_mut().unwrap_or_else(|e| e.into_inner());
            for close in pending.drain(..) {
                let _ = (close.close)(backend, &close.target);
            }
            let callbacks = backend.take_notifications();
            for callback in callbacks {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
            }
        }
    }
}
struct BackendGuard<'a, F: FileSystem> {
    guard: Option<std::sync::MutexGuard<'a, Option<F>>>,
    pending: &'a Mutex<Vec<PendingClose<F>>>,
    has_pending: &'a AtomicBool,
}
impl<F: FileSystem> std::ops::Deref for BackendGuard<'_, F> {
    type Target = F;
    fn deref(&self) -> &F {
        self.guard
            .as_ref()
            .expect("live guard")
            .as_ref()
            .expect("live backend")
    }
}
impl<F: FileSystem> std::ops::DerefMut for BackendGuard<'_, F> {
    fn deref_mut(&mut self) -> &mut F {
        self.guard
            .as_mut()
            .expect("live guard")
            .as_mut()
            .expect("live backend")
    }
}
impl<F: FileSystem> BackendGuard<'_, F> {
    fn drain_cleanup(&mut self) -> VfResult<()> {
        if !self.has_pending.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        let mut failed = Vec::new();
        let mut first = None;
        for close in pending {
            if let Err(error) = (close.close)(self, &close.target) {
                first.get_or_insert(error);
                failed.push(close);
            }
        }
        if !failed.is_empty() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(failed);
            self.has_pending.store(true, Ordering::Release);
        }
        first.map_or(Ok(()), Err)
    }
}
impl<F: FileSystem> Drop for BackendGuard<'_, F> {
    fn drop(&mut self) {
        let callbacks = self.take_notifications();
        drop(self.guard.take());
        for callback in callbacks {
            // Observability must not turn successful I/O into a panic or lose
            // ownership of a newly opened descriptor before its RAII wrapping.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
        }
    }
}

/// Cloneable owner of one synchronous backend connection.
/// Clones share a mutex and serialize backend calls; use separate connections
/// for parallel RPCs. Explicitly close handles to observe cleanup failures.
/// Handle Drop queues cleanup, drained before later operations or by
/// `drain_cleanup`. Dropping the final backend owner performs synchronous
/// teardown and may wait for pending closes and backend request timeouts.
pub struct FsClient<F: FileSystem> {
    inner: Arc<SharedBackend<F>>,
    limits: ResourceLimits,
}

impl<F: FileSystem> Clone for FsClient<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            limits: self.limits,
        }
    }
}

impl<F: FileSystem> fmt::Debug for FsClient<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FsClient").finish_non_exhaustive()
    }
}

impl<F: FileSystem> FsClient<F> {
    pub fn new(filesystem: F) -> Self {
        Self {
            inner: Arc::new(SharedBackend::new(filesystem)),
            limits: ResourceLimits::default(),
        }
    }

    /// Configure this client view and its future clones. Existing clones keep
    /// their policy; all views still share the same connection and ownership.
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }

    fn lock(&self) -> VfResult<BackendGuard<'_, F>> {
        self.inner.lock().map_err(|_| poisoned())
    }

    pub fn into_inner(self) -> Result<F, Self> {
        if self.drain_cleanup().is_err() {
            return Err(self);
        }
        match Arc::try_unwrap(self.inner) {
            Ok(mut shared) => Ok(shared
                .backend
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .expect("owned backend")),
            Err(inner) => Err(Self {
                inner,
                limits: self.limits,
            }),
        }
    }

    /// Drain queued Drop cleanup and report the first failure. Failed targets
    /// remain owned for later cleanup. No new file operation is replayed.
    pub fn drain_cleanup(&self) -> VfResult<()> {
        self.inner
            .lock_without_cleanup()
            .map_err(|_| poisoned())?
            .drain_cleanup()
    }
}

impl<F: FileSystem> FsClient<F> {
    pub(crate) fn capabilities(&self) -> VfResult<Capabilities> {
        Ok(self.lock()?.capabilities())
    }
    /// Open a path read-only.
    pub(crate) fn open(&self, path: impl AsRef<Path>) -> VfResult<FsFile<F>> {
        self.open_with_native(OpenOp::new(path.as_ref(), OpenFlags::READ))
    }

    pub(crate) fn open_with_native(&self, request: OpenOp) -> VfResult<FsFile<F>> {
        let file = self.lock()?.open_impl(&request)?;
        Ok(FsFile {
            inner: Arc::clone(&self.inner),
            file: Some(file),
            path: request.path,
        })
    }

    pub fn open_options(&self) -> OpenOptions<'_, F> {
        OpenOptions::new(self)
    }
}

impl<F: FileSystem> FsClient<F> {
    /// Stream one file using an explicit maximum chunk size.
    ///
    /// The callback runs without holding the backend lock, so it may use this
    /// client or drop other files owned by it. Return `Ok(false)` to stop
    /// successfully. Callback errors propagate. At most one requested chunk is
    /// buffered at once, and the file closes on success, cancellation, or error.
    pub(crate) fn read_stream_with_options(
        &self,
        path: impl AsRef<Path>,
        options: StreamOptions,
        mut callback: impl FnMut(u64, &[u8]) -> VfResult<bool>,
    ) -> VfResult<StreamCompletion> {
        let chunk_size = options.chunk_size_bytes();
        if chunk_size == 0 {
            return Err(VfError::failure(0, crate::ERR_INVAL));
        }

        let file = self.open(path)?;
        let raw_file = file.raw()?.clone();
        let operation = (|| -> VfResult<StreamCompletion> {
            let mut offset = 0u64;
            loop {
                let request = ReadOp::at(raw_file.clone(), offset, chunk_size);
                let result = {
                    let mut backend = self.lock()?;
                    let result = backend.read_impl(&request)?;
                    validate_read_results(
                        "read_stream",
                        std::slice::from_ref(&request),
                        std::slice::from_ref(&result),
                    )?;
                    result
                };
                let length = result.data.len();
                if !result.data.is_empty() && !callback(offset, &result.data)? {
                    return Ok(StreamCompletion::Stopped {
                        next_offset: offset
                            .checked_add(length as u64)
                            .ok_or_else(|| VfError::client(0, crate::ERR_INVAL))?,
                    });
                }
                if result.eof {
                    return Ok(StreamCompletion::Complete);
                }
                offset = offset
                    .checked_add(length as u64)
                    .ok_or_else(|| VfError::client(0, libc::EOVERFLOW as u32))?;
            }
        })();
        let close = file.close();
        match operation {
            Err(error) => Err(error),
            Ok(completion) => close.map(|()| completion),
        }
    }
}

impl<F: Backend> FsClient<F> {
    pub fn set_metadata(&self, path: impl AsRef<Path>) -> SetMetadata<'_, F> {
        SetMetadata {
            client: self,
            path: path.as_ref().to_path_buf(),
            update: vfsi_core::SetAttrsOp::new(()),
        }
    }
}

impl<F: Backend> FsClient<F> {
    fn removal_metadata(&self, path: &Path, operation: &'static str) -> VfResult<Attrs> {
        let mut filesystem = self.lock()?;
        let follow = !filesystem.capabilities().contains(Capabilities::LSTAT);
        filesystem
            .metadata_path_impl(path, follow)
            .map_err(|error| error.with_context(operation, path))
    }
}

impl<F: Backend> FsClient<F> {
    /// Rename independent source/destination pairs in one vector phase.
    /// Requested atomic destination semantics are applied per pair; the vector
    /// is not transactional. Unsupported semantics are never emulated.
    pub(crate) fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::RenameOptions,
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let requests: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                (
                    VfFile::from_os_path(from.as_ref()),
                    VfFile::from_os_path(to.as_ref()),
                )
            })
            .collect();
        self.lock()?
            .vrename_with_options_impl(&requests, options)
            .map_err(|error| match error.index() {
                Some(index) if index < pairs.len() => {
                    error.with_context("vrename", pairs[index].0.as_ref())
                }
                Some(_) => {
                    VfError::transport(None, "rename backend returned an invalid error index")
                }
                None => error,
            })
    }

    /// Create directories with per-request Unix permission bits.
    pub(crate) fn vmkdir<P: AsRef<Path>>(
        &self,
        directories: &[vfsi_core::MkDirOp<P>],
    ) -> VfResult<()> {
        let mut seen = HashSet::with_capacity(directories.len());
        for (index, op) in directories.iter().enumerate() {
            if !seen.insert(op.path()) {
                return Err(
                    VfError::client(index, crate::ERR_INVAL).with_context("vmkdir", op.path())
                );
            }
        }
        if directories.is_empty() {
            return Ok(());
        }
        let dirs: Vec<_> = directories
            .iter()
            .map(|op| crate::VfAttrs {
                file: VfFile::from_os_path(op.path()),
                masks: AttrMask::MODE,
                mode: op.mode(),
                ..Default::default()
            })
            .collect();
        self.lock()?
            .vmkdir_impl(&dirs)
            .map_err(|error| match error.index() {
                Some(index) if index < directories.len() => {
                    error.with_context("vmkdir", directories[index].path())
                }
                Some(_) => {
                    VfError::transport(None, "mkdir backend returned an invalid error index")
                }
                None => error,
            })
    }

    /// Create symbolic links in one native backend vector.
    pub(crate) fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let targets: Vec<_> = pairs.iter().map(|(target, _)| target.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        self.lock()?
            .vsymlink_impl(&targets, &links)
            .map_err(|error| link_error(error, &links, "vsymlink"))
    }

    /// Read targets without converting Unix path bytes through UTF-8.
    pub(crate) fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        let targets = self
            .lock()?
            .vreadlink_impl(&paths)
            .map_err(|error| link_error(error, &paths, "vreadlink"))?;
        if targets.len() != paths.len() {
            return Err(VfError::transport(
                None,
                "readlink backend returned an invalid result count",
            ));
        }
        Ok(targets
            .into_iter()
            .map(crate::native::bytes_to_path)
            .collect())
    }

    /// Create hard links in one native backend vector.
    pub(crate) fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let sources: Vec<_> = pairs.iter().map(|(source, _)| source.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        self.lock()?
            .vhardlink_impl(&sources, &links)
            .map_err(|error| link_error(error, &links, "vhardlink"))
    }

    /// Lazy between directories, with selective no-follow metadata and pruning.
    /// Enter runs before any listing; Leave follows even a pruned directory.
    /// Sorting buffers one bounded directory, not the whole tree. The callback
    /// runs outside the backend lock. Limits include the starting object.
    /// Maximum safe cohort size for incremental directory paging.
    #[doc(hidden)]
    pub fn directory_page_batch_size(&self) -> VfResult<usize> {
        Ok(self.lock()?.directory_page_batch_size().clamp(1, 32))
    }
    /// Fetch a bounded vector of directory pages, retaining backend cursors.
    #[doc(hidden)]
    pub fn read_dir_pages_with_fields(
        &self,
        paths: &[&Path],
        fields: AttrMask,
        cursors: Vec<Option<crate::DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<Vec<crate::DirectoryPage>> {
        let pages = self.lock()?.vlistdir_pages_impl(
            paths,
            fields | AttrMask::MODE | AttrMask::SIZE,
            cursors,
            page_size,
            max_entries,
        )?;
        if pages.len() != paths.len() {
            return Err(VfError::transport(
                None,
                "directory pages returned an invalid result count",
            ));
        }
        pages
            .into_iter()
            .zip(paths)
            .enumerate()
            .map(|(index, ((attrs, next, children), path))| {
                if children.len() != attrs.len()
                    || attrs.len() > page_size
                    || (attrs.is_empty() && next.is_some())
                {
                    return Err(VfError::transport(
                        Some(index),
                        "invalid directory page progress",
                    ));
                }
                let mut seeds = Vec::new();
                let entries = attrs
                    .into_iter()
                    .zip(children)
                    .map(|(attrs, child)| {
                        let entry_path = attrs
                            .file
                            .path()
                            .ok_or_else(|| {
                                VfError::transport(Some(index), "directory entry has no path")
                            })?
                            .to_path_buf();
                        if entry_path.parent() != Some(*path) {
                            return Err(VfError::transport(
                                Some(index),
                                "directory entry is outside its parent",
                            ));
                        }
                        if let Some(child) = child {
                            seeds.push((entry_path.clone(), child));
                        }
                        Ok(DirEntry::new(
                            entry_path,
                            vfsi_core::metadata_from_attrs(attrs),
                        ))
                    })
                    .collect::<VfResult<Vec<_>>>()?;
                Ok((
                    DirectoryListing {
                        path: path.to_path_buf(),
                        entries,
                    },
                    next,
                    seeds,
                ))
            })
            .collect()
    }
}

impl<F: Backend> FsClient<F> {
    /// Create `path` if missing, otherwise empty it. Errors if it exists and is
    /// not a directory (a symlink to a directory is not a directory here).
    pub fn ensure_empty_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.ensure_empty_dir_impl(path.as_ref())
    }

    /// Empty a directory while keeping it, with explicit removal policy.
    pub(crate) fn remove_dir_contents_with_options(
        &self,
        path: impl AsRef<Path>,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let path = path.as_ref();
        if !self
            .removal_metadata(path, "remove_dir_contents_impl")?
            .is_dir()
        {
            return Err(
                VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir_contents", path)
            );
        }
        self.lock()?
            .remove_dir_contents_path_with_options_impl(path, options)
            .map_err(|error| error.with_context("remove_dir_contents", path))
    }

    /// Open a *genuine* directory handle for race-resistant, handle-rooted
    /// removal. Backends that only return a path fail instead of silently
    /// losing the handle safety guarantee.
    pub fn open_dir_handle(&self, path: impl AsRef<Path>) -> VfResult<FsDir<F>> {
        let path = path.as_ref();
        let mut backend = self.lock()?;
        let dir = backend.open_dir_impl(path)?;
        if !matches!(dir, VfDir::Descriptor { .. }) {
            let _ = backend.close_dir_impl(&dir);
            return Err(VfError::unsupported(0).with_context("open_dir_handle", path));
        }
        Ok(FsDir {
            inner: Arc::clone(&self.inner),
            dir: Some(dir),
            path: path.to_path_buf(),
        })
    }
}

/// Owned, handle-rooted directory. Dropping it queues backend cleanup;
/// [`close`](Self::close) reports cleanup errors explicitly.
pub struct FsDir<F: Backend> {
    inner: Arc<SharedBackend<F>>,
    dir: Option<VfDir>,
    path: PathBuf,
}

impl<F: Backend> fmt::Debug for FsDir<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsDir")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<F: Backend> FsDir<F> {
    pub fn is_closed(&self) -> bool {
        self.dir.is_none()
    }
    /// Name used at open, retained for diagnostics; not updated after rename.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn remove_contents(&self) -> VfResult<()> {
        self.remove_contents_with_options(RemoveOptions::default())
    }

    pub fn remove_contents_with_options(&self, options: RemoveOptions) -> VfResult<()> {
        let dir = self
            .dir
            .as_ref()
            .ok_or_else(|| VfError::client(0, crate::ERR_EBADF))?;
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .remove_dir_contents_handle_with_options_impl(dir, options)
            .map_err(|error| error.with_context("remove_dir_contents", &self.path))
    }

    pub fn close(mut self) -> VfResult<()> {
        self.try_close()
    }

    /// Keep cleanup ownership on failure so the caller can retry explicitly.
    pub fn try_close(&mut self) -> VfResult<()> {
        let Some(dir) = self.dir.as_ref() else {
            return Ok(());
        };
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .close_dir_impl(dir)?;
        self.dir = None;
        Ok(())
    }
}

impl<F: Backend> Drop for FsDir<F> {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            self.inner.defer(
                CleanupTarget::Directory(dir, std::mem::take(&mut self.path)),
                |backend, target| {
                    let CleanupTarget::Directory(dir, path) = target else {
                        unreachable!()
                    };
                    backend
                        .close_dir_impl(dir)
                        .map_err(|error| error.with_context("close_dir", path))
                },
            );
        }
    }
}

impl<F: Backend> FsClient<F> {
    /// Copy whole files in request order. A successful prefix may remain if
    /// a later request fails; this operation does not provide atomicity.
    pub(crate) fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::CopyOption,
    ) -> VfResult<()> {
        let extents: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                crate::ExtentPair::from_os_paths(from.as_ref(), 0, to.as_ref(), 0, None)
            })
            .collect();
        self.lock()?.vcopy_impl(&extents, options).map_err(|error| {
            error
                .index()
                .and_then(|index| pairs.get(index))
                .map_or(error.clone(), |(_, to)| {
                    error.with_context("vcopy", to.as_ref())
                })
        })
    }

    /// Remove paths in request order, optionally recursing into directories.
    /// A successful prefix may remain if a later path fails.
    #[doc(hidden)]
    pub fn vremove_native<P: AsRef<Path>>(&self, paths: &[P], recursive: bool) -> VfResult<()> {
        self.vremove_with_options_native(paths, recursive, RemoveOptions::default())
    }

    /// Remove paths with explicit error, batching, and retry policy.
    #[doc(hidden)]
    pub fn vremove_with_options_native<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let paths: Vec<&Path> = paths.iter().map(AsRef::as_ref).collect();
        self.lock()?
            .remove_paths_with_options_impl(&paths, recursive, options)
            .map_err(|error| {
                error
                    .index()
                    .and_then(|index| paths.get(index))
                    .map_or(error.clone(), |path| {
                        error.with_context("vremove_native", path)
                    })
            })
    }

    /// Vector metadata query with explicit fields and final-symlink handling.
    /// Ancestor symlinks follow the backend's normal namespace semantics.
    #[doc(hidden)]
    pub fn vgetattrs_native<P: AsRef<Path>>(
        &self,
        paths: &[P],
        fields: AttrMask,
        follow: bool,
    ) -> VfResult<Vec<Attrs>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut attrs: Vec<_> = paths
            .iter()
            .map(|path| crate::VfAttrs {
                file: VfFile::from_os_path(path.as_ref()),
                masks: fields | AttrMask::MODE,
                ..crate::VfAttrs::default()
            })
            .collect();
        let result = {
            let mut backend = self.lock()?;
            if follow {
                backend.vgetattrs_impl(&mut attrs)
            } else {
                backend.vgetattrs_nofollow_impl(&mut attrs)
            }
        };
        result.map_err(|error| match error.index() {
            Some(index) if index < paths.len() => {
                error.with_context("metadatav", paths[index].as_ref())
            }
            Some(_) => VfError::transport(None, "metadata backend returned an invalid error index"),
            None => error,
        })?;
        Ok(attrs
            .into_iter()
            .map(vfsi_core::metadata_from_attrs)
            .collect())
    }

    /// Fetch no-follow metadata for many paths using the backend vector operation.
    #[doc(hidden)]
    pub fn vsymlink_attrs_native(&self, paths: &[&Path]) -> VfResult<Vec<Attrs>> {
        let mut attrs: Vec<_> = paths
            .iter()
            .map(|path| crate::VfAttrs {
                file: VfFile::from_os_path(path),
                masks: AttrMask::MODE,
                ..crate::VfAttrs::default()
            })
            .collect();
        self.lock()?
            .vgetattrs_nofollow_impl(&mut attrs)
            .map_err(|error| {
                error
                    .index()
                    .and_then(|index| paths.get(index))
                    .map_or(error.clone(), |path| {
                        error.with_context("vsymlink_attrs_native", path)
                    })
            })?;
        Ok(attrs
            .into_iter()
            .map(vfsi_core::metadata_from_attrs)
            .collect())
    }
}

impl<F: Backend> FsClient<F> {
    /// Open an ordered vector of files.
    ///
    /// Success returns one RAII handle per request. Failure returns no
    /// handles; VFSI does not promise transactional rollback of other
    /// filesystem effects such as file creation.
    pub(crate) fn vopen(&self, requests: &[OpenOp]) -> VfResult<Vec<FsFile<F>>> {
        let mut filesystem = self.lock()?;
        let files = filesystem.vopen_impl(requests).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("vopen", &request.path)
                })
        })?;
        if files.len() != requests.len() {
            let error = wrong_result_count("vopen", requests.len(), files.len());
            for file in &files {
                let _ = filesystem.close_impl(file);
            }
            return Err(error);
        }
        drop(filesystem);
        Ok(files
            .into_iter()
            .zip(requests)
            .map(|(file, request)| FsFile {
                inner: Arc::clone(&self.inner),
                file: Some(file),
                path: request.path.clone(),
            })
            .collect())
    }

    /// Try to close a group through one vector operation without consuming
    /// the handles. On failure, all handles remain armed: the backend may
    /// have closed a prefix, so callers must reconcile before retrying.
    #[doc(hidden)]
    pub fn vclose<'a>(&self, files: impl IntoIterator<Item = &'a mut FsFile<F>>) -> VfResult<()>
    where
        F: 'a,
    {
        let mut files: Vec<_> = files.into_iter().collect();
        for (index, file) in files.iter().enumerate() {
            self.validate_owner(file, index)?;
        }
        let positions: Vec<_> = files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| (!file.is_closed()).then_some(index))
            .collect();
        files.retain(|file| !file.is_closed());
        if files.is_empty() {
            return Ok(());
        }
        let descriptors: Vec<VfFile> = files
            .iter()
            .map(|file| file.raw().cloned())
            .collect::<VfResult<_>>()?;
        self.lock()?.vclose_impl(&descriptors).map_err(|error| {
            error
                .index()
                .and_then(|index| files.get(index))
                .map_or(error.clone(), |file| {
                    error.with_context("vclose_owned", file.path())
                })
                .map_index(|index| positions.get(index).copied().unwrap_or(index))
        })?;
        for file in &mut files {
            file.file = None;
        }
        Ok(())
    }

    /// Close a group of files through one vector operation.
    ///
    /// On failure, the handles are dropped and the backend receives
    /// best-effort scalar cleanup attempts. Use [`vclose`](Self::vclose)
    /// to retain the handles after an error.
    #[doc(hidden)]
    pub fn vclose_owned(&self, mut files: Vec<FsFile<F>>) -> VfResult<()> {
        self.vclose(&mut files)
    }

    /// Read an ordered vector with a 16 MiB aggregate request limit.
    /// Use [`vread_with_limit_native`](Self::vread_with_limit_native) to tune the limit or
    /// [`vread_into_native`](Self::vread_into_native) to provide bounded caller-owned buffers.
    #[doc(hidden)]
    pub fn vread_native(&self, requests: &[FsRead<'_, F>]) -> VfResult<Vec<FsReadResult>> {
        self.vread_with_limit_native(requests, self.limits.max_read_bytes)
    }

    /// Read an ordered vector with an explicit aggregate request limit.
    #[doc(hidden)]
    pub fn vread_with_limit_native(
        &self,
        requests: &[FsRead<'_, F>],
        max_total_bytes: usize,
    ) -> VfResult<Vec<FsReadResult>> {
        self.vread_with_limit_projected_native(requests, max_total_bytes, |request| request)
    }

    /// Backend adapter for opaque application requests; projection does not allocate.
    /// The projection must return the same embedded request on every invocation.
    #[doc(hidden)]
    pub fn vread_with_limit_projected_native<'b, T>(
        &self,
        requests: &[T],
        max_total_bytes: usize,
        project: impl for<'r> Fn(&'r T) -> &'r FsRead<'b, F>,
    ) -> VfResult<Vec<FsReadResult>>
    where
        F: 'b,
    {
        let mut requested = 0usize;
        for (index, request) in requests.iter().map(&project).enumerate() {
            requested = requested
                .checked_add(request.length)
                .ok_or_else(|| VfError::client(index, libc::EFBIG as u32))?;
            if requested > max_total_bytes {
                return Err(VfError::client(index, libc::EFBIG as u32)
                    .with_context("vread_native", request.file.path()));
            }
        }
        let reads = self.read_ops(requests, &project)?;
        let results = self.lock()?.vread_impl(&reads).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_native", request.file.path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count(
                "vread_native",
                requests.len(),
                results.len(),
            ));
        }
        validate_read_results("vread_native", &reads, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_native", request.file.path())
                })
        })?;
        Ok(results.into_iter().map(read_result).collect())
    }

    fn read_ops<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsRead<'b, F>,
    ) -> VfResult<Vec<ReadOp>>
    where
        F: 'b,
    {
        let mut reads = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file, index)?;
            reads.push(ReadOp::new(
                request.file.raw()?.clone(),
                request.offset,
                request.length,
            ));
        }
        Ok(reads)
    }

    #[doc(hidden)]
    pub fn vread_into_native(
        &self,
        requests: &mut [FsReadInto<'_, F>],
    ) -> VfResult<Vec<FsReadIntoResult>> {
        self.vread_into_with_limit_native(requests, self.limits.max_read_bytes)
    }

    /// Read into caller storage with an explicit aggregate buffer budget.
    /// This also bounds allocation in copying fallback implementations.
    #[doc(hidden)]
    pub fn vread_into_with_limit_native(
        &self,
        requests: &mut [FsReadInto<'_, F>],
        max_bytes: usize,
    ) -> VfResult<Vec<FsReadIntoResult>> {
        self.vread_into_with_limit_projected_native(
            requests,
            max_bytes,
            |request| request,
            |request| request,
        )
    }

    /// Project borrowed buffers without an intermediate request allocation.
    /// Both projections must identify the same embedded request, and remain
    /// stable across preflight, dispatch, and result validation.
    #[doc(hidden)]
    pub fn vread_into_with_limit_projected_native<'b, T>(
        &self,
        requests: &mut [T],
        max_bytes: usize,
        project: impl for<'r> Fn(&'r T) -> &'r FsReadInto<'b, F>,
        mut project_mut: impl for<'r> FnMut(&'r mut T) -> &'r mut FsReadInto<'b, F>,
    ) -> VfResult<Vec<FsReadIntoResult>>
    where
        F: 'b,
    {
        let mut requested = 0usize;
        let mut reads = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file, index)?;
            requested = requested
                .checked_add(request.buffer.len())
                .ok_or_else(|| VfError::client(index, libc::EFBIG as u32))?;
            if requested > max_bytes {
                return Err(VfError::client(index, libc::EFBIG as u32));
            }
            reads.push(ReadOp::new(
                request.file.raw()?.clone(),
                request.offset,
                request.buffer.len(),
            ));
        }
        let results = {
            let mut buffers: Vec<&mut [u8]> = requests
                .iter_mut()
                .map(&mut project_mut)
                .map(|request| &mut *request.buffer)
                .collect();
            self.lock()?.vread_into_impl(&reads, &mut buffers)
        }
        .map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_into_native", request.file.path())
                })
        })?;
        validate_read_into_results("vread_into_native", &reads, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_into_native", request.file.path())
                })
        })?;
        Ok(results
            .into_iter()
            .map(|result| FsReadIntoResult {
                offset: result.offset,
                read: result.read,
                eof: result.eof,
            })
            .collect())
    }

    #[doc(hidden)]
    pub fn vwrite_native(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<FsWriteResult>> {
        self.vwrite_projected_native(requests, |request| request)
    }

    /// Backend adapter for opaque application requests, preserving borrowed payloads.
    /// The projection must return the same embedded request on every invocation.
    #[doc(hidden)]
    pub fn vwrite_projected_native<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        self.vwrite_mapped_native(requests, |item| {
            let request = project(item);
            FsWrite {
                file: request.file,
                offset: request.offset,
                data: request.data,
            }
        })
    }

    /// Adapter constructing cheap borrowed requests without a temporary vector.
    /// The mapper must return the same file, offset, and payload on each call.
    #[doc(hidden)]
    #[doc(hidden)]
    pub fn vwrite_mapped_native<'b, T>(
        &self,
        requests: &[T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        let writes = self.write_ops(requests, &project)?;
        let results = self.lock()?.vwrite_impl(&writes).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vwrite_native", request.file.path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count(
                "vwrite_native",
                requests.len(),
                results.len(),
            ));
        }
        validate_write_results("vwrite_native", &writes, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vwrite_native", request.file.path())
                })
        })?;
        Ok(results.into_iter().map(write_result).collect())
    }

    /// Write every byte in each positional request, retrying short writes in
    /// vector waves. Like `vwrite_native`, this is not transactional: an error may
    /// follow a successfully written prefix. Overlapping requests through the
    /// same path complete in input order; different paths are presumed
    /// independent (including hard-link aliases).
    #[doc(hidden)]
    pub fn vwrite_all_native(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<FsWriteResult>> {
        self.vwrite_all_projected_native(requests, |request| request)
    }

    /// Complete projected requests with the same preflight and dependency waves.
    /// The projection must return the same embedded request on every invocation.
    #[doc(hidden)]
    pub fn vwrite_all_projected_native<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        self.vwrite_all_mapped_native(requests, |item| {
            let request = project(item);
            FsWrite {
                file: request.file,
                offset: request.offset,
                data: request.data,
            }
        })
    }

    /// Complete mapped writes with whole-batch preflight and dependency waves.
    /// The mapper must return the same file, offset, and payload on each call.
    #[doc(hidden)]
    #[doc(hidden)]
    pub fn vwrite_all_mapped_native<'b, T>(
        &self,
        requests: &[T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        // Validate the entire batch before writing any prefix. In particular,
        // empty requests must not conceal a foreign or already-closed file.
        let mut prior_by_path: HashMap<&Path, Vec<(usize, u64, u64)>> = HashMap::new();
        let mut blocked_by = vec![Vec::new(); requests.len()];
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file, index)?;
            request.file.raw().map_err(|error| {
                error
                    .with_index(index)
                    .with_context("vwrite_all_native", request.file.path())
            })?;
            let VfOffset::At(offset) = request.offset else {
                return Err(VfError::client(index, crate::ERR_INVAL)
                    .with_context("vwrite_all_native", request.file.path()));
            };
            let end = offset
                .checked_add(request.data.len() as u64)
                .ok_or_else(|| {
                    VfError::client(index, libc::EOVERFLOW as u32)
                        .with_context("vwrite_all_native", request.file.path())
                })?;
            // Non-overlapping writes commute. An overlapping later request
            // must wait until every earlier conflicting request is complete:
            // otherwise a short-write retry can overwrite the later bytes.
            let prior = prior_by_path.entry(request.file.path()).or_default();
            for &(earlier, start, earlier_end) in prior.iter() {
                if offset < earlier_end && start < end {
                    blocked_by[index].push(earlier);
                }
            }
            prior.push((index, offset, end));
        }
        let mut totals = vec![0usize; requests.len()];
        let mut stable = vec![true; requests.len()];
        let mut pending: Vec<usize> = (0..requests.len())
            .filter(|&index| !project(&requests[index]).data.is_empty())
            .collect();
        while !pending.is_empty() {
            let wave_indices: Vec<usize> = pending
                .iter()
                .copied()
                .filter(|&index| {
                    blocked_by[index]
                        .iter()
                        .all(|&earlier| totals[earlier] == project(&requests[earlier]).data.len())
                })
                .collect();
            let wave: Vec<_> = wave_indices
                .iter()
                .map(|&index| {
                    let request = project(&requests[index]);
                    let VfOffset::At(offset) = request.offset else {
                        return Err(VfError::client(index, crate::ERR_INVAL)
                            .with_context("vwrite_all_native", request.file.path()));
                    };
                    let offset = offset.checked_add(totals[index] as u64).ok_or_else(|| {
                        VfError::client(index, libc::EOVERFLOW as u32)
                            .with_context("vwrite_all_native", request.file.path())
                    })?;
                    Ok(FsWrite {
                        file: request.file,
                        offset: VfOffset::At(offset),
                        data: &request.data[totals[index]..],
                    })
                })
                .collect::<VfResult<_>>()?;
            let results = self.vwrite_native(&wave).map_err(|error| {
                error.map_index(|index| wave_indices.get(index).copied().unwrap_or(index))
            })?;
            for (&index, result) in wave_indices.iter().zip(results) {
                if result.written == 0 {
                    return Err(VfError::client(index, crate::ERR_IO)
                        .with_context("vwrite_all_native", project(&requests[index]).file.path()));
                }
                totals[index] += result.written;
                stable[index] &= result.stable;
            }
            pending.retain(|&index| totals[index] < project(&requests[index]).data.len());
        }
        requests
            .iter()
            .map(&project)
            .enumerate()
            .map(|(index, request)| {
                let VfOffset::At(offset) = request.offset else {
                    return Err(VfError::client(index, crate::ERR_INVAL)
                        .with_context("vwrite_all_native", request.file.path()));
                };
                Ok(FsWriteResult {
                    offset,
                    written: totals[index],
                    stable: stable[index],
                })
            })
            .collect()
    }

    fn write_ops<'a, 'b, T>(
        &self,
        requests: &'a [T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> VfResult<Vec<WriteOpRef<'a>>>
    where
        F: 'b,
        'b: 'a,
    {
        let mut writes = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file, index)?;
            writes.push(WriteOpRef::new(
                request
                    .file
                    .raw()
                    .map_err(|error| error.with_index(index))?,
                request.offset,
                request.data,
            ));
        }
        Ok(writes)
    }

    fn validate_owner(&self, file: &FsFile<F>, index: usize) -> VfResult<()> {
        if Arc::ptr_eq(&self.inner, &file.inner) {
            Ok(())
        } else {
            Err(VfError::client(index, crate::ERR_INVAL))
        }
    }
}

impl<F: Backend> FsClient<F> {
    /// Native bounded whole-file reads for backend adapters.
    ///
    /// Applications should use [`VfsiExt::read_files_with_options`].
    #[doc(hidden)]
    pub fn read_files_native<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        let files: Vec<_> = paths
            .iter()
            .map(|path| VfFile::from_os_path(path.as_ref()))
            .collect();
        let result = self
            .lock()?
            .vread_all_with_options_impl(&files, options)
            .map_err(|error| {
                error
                    .index()
                    .and_then(|index| paths.get(index))
                    .map_or(error.clone(), |path| {
                        error.with_context("read_files", path.as_ref())
                    })
            });
        result.and_then(|buffers| {
            if buffers.len() != paths.len() {
                Err(wrong_result_count("read_files", paths.len(), buffers.len()))
            } else {
                Ok(buffers)
            }
        })
    }
}

/// `std::fs::OpenOptions`-style builder tied to an [`FsClient`].
pub struct OpenOptions<'a, F: FileSystem> {
    client: &'a FsClient<F>,
    flags: OpenFlags,
    mode: u32,
}

impl<F: FileSystem> Clone for OpenOptions<'_, F> {
    fn clone(&self) -> Self {
        Self {
            client: self.client,
            flags: self.flags,
            mode: self.mode,
        }
    }
}

impl<F: FileSystem> fmt::Debug for OpenOptions<'_, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenOptions")
            .field("flags", &self.flags)
            .field("mode", &format_args!("{:#o}", self.mode))
            .finish()
    }
}

impl<'a, F: FileSystem> OpenOptions<'a, F> {
    fn new(client: &'a FsClient<F>) -> Self {
        Self {
            client,
            flags: OpenFlags::empty(),
            mode: 0o666,
        }
    }

    pub fn read(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::READ, enabled);
        self
    }

    pub fn write(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::WRITE, enabled);
        self
    }

    pub fn append(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::APPEND, enabled);
        self
    }

    pub fn truncate(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::TRUNCATE, enabled);
        self
    }

    pub fn create(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE, enabled);
        self
    }

    pub fn create_new(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE_NEW, enabled);
        self
    }

    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> VfResult<FsFile<F>> {
        self.client
            .open_with_native(OpenOp::new(path.as_ref(), self.flags).mode(self.mode))
    }
}

impl<F: Backend> OpenOptions<'_, F> {
    pub fn vopen<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<FsFile<F>>> {
        let requests: Vec<OpenOp> = paths
            .iter()
            .map(|path| OpenOp::new(path.as_ref(), self.flags).mode(self.mode))
            .collect();
        self.client.vopen(&requests)
    }
}

/// Builder for an atomic metadata update request.
pub struct SetMetadata<'a, F: Backend> {
    client: &'a FsClient<F>,
    path: PathBuf,
    update: vfsi_core::SetAttrsOp<()>,
}

impl<F: Backend> Clone for SetMetadata<'_, F> {
    fn clone(&self) -> Self {
        Self {
            client: self.client,
            path: self.path.clone(),
            update: self.update,
        }
    }
}

impl<F: Backend> fmt::Debug for SetMetadata<'_, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetMetadata")
            .field("path", &self.path)
            .field("update", &self.update)
            .finish_non_exhaustive()
    }
}

impl<F: Backend> SetMetadata<'_, F> {
    pub fn permissions(&mut self, permissions: Permissions) -> &mut Self {
        self.update = self.update.permissions(permissions);
        self
    }

    pub fn len(&mut self, len: u64) -> &mut Self {
        self.update = self.update.len(len);
        self
    }

    pub fn accessed(&mut self, accessed: SystemTime) -> &mut Self {
        self.update = self.update.accessed(accessed);
        self
    }

    pub fn modified(&mut self, modified: SystemTime) -> &mut Self {
        self.update = self.update.modified(modified);
        self
    }

    pub fn follow_symlinks(&mut self, follow: bool) -> &mut Self {
        self.update = self.update.follow_symlinks(follow);
        self
    }

    pub fn uid(&mut self, uid: u32) -> &mut Self {
        self.update = self.update.uid(uid);
        self
    }
    pub fn gid(&mut self, gid: u32) -> &mut Self {
        self.update = self.update.gid(gid);
        self
    }
    pub fn apply(&self) -> VfResult<()> {
        self.client
            .vsetattrs(&[self.update.with_target(&self.path)])
    }
}

/// Owned RAII file which does not borrow the client.
///
/// Dropping an open file attempts a best-effort CLOSE. This can block on the
/// backend mutex and on network I/O; use [`FsFile::try_close`] when the close
/// result matters or when its timing must be controlled.
pub struct FsFile<F: FileSystem> {
    inner: Arc<SharedBackend<F>>,
    file: Option<VfFile>,
    path: PathBuf,
}

impl<F: FileSystem> fmt::Debug for FsFile<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsFile")
            .field("path", &self.path)
            .field("is_open", &self.file.is_some())
            .finish()
    }
}

impl<F: FileSystem> FsFile<F> {
    /// Whether explicit close has completed successfully on this handle.
    pub fn is_closed(&self) -> bool {
        self.file.is_none()
    }
    fn raw(&self) -> VfResult<&VfFile> {
        self.file
            .as_ref()
            .ok_or_else(|| VfError::client(0, crate::ERR_EBADF))
    }

    /// Positional read which does not alter the file cursor.
    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> VfResult<usize> {
        self.read_into(buffer, VfOffset::At(offset))
    }

    /// Positional write which does not alter the file cursor.
    pub fn write_at(&self, buffer: &[u8], offset: u64) -> VfResult<usize> {
        self.write_from(buffer, VfOffset::At(offset))
    }

    pub fn read_request_at(&self, offset: u64, length: usize) -> FsRead<'_, F> {
        FsRead {
            file: self,
            offset: VfOffset::At(offset),
            length,
        }
    }

    /// Name used at open, retained for diagnostics; not a current namespace
    /// lookup or proof of identity. Handle operations continue to use the
    /// opened object even if its pathname changes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Query metadata for the open object without resolving its path again.
    pub fn attrs(&self) -> VfResult<Attrs> {
        let attributes = AttrMask::MODE
            | AttrMask::SIZE
            | AttrMask::NLINK
            | AttrMask::FILEID
            | AttrMask::UID
            | AttrMask::GID
            | AttrMask::ATIME
            | AttrMask::MTIME
            | AttrMask::CTIME
            | AttrMask::CHANGE;
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .metadata_impl(
                vfsi_core::Target::File(self.raw()?),
                vfsi_core::api::AttrsOptions::new().fields(attributes),
            )
            .map(vfsi_core::metadata_from_attrs)
            .map_err(|error| error.with_context("metadata", &self.path))
    }

    /// Truncate or extend the open file.
    pub fn truncate(&self, len: u64) -> VfResult<()> {
        self.set_attrs_op(vfsi_core::SetAttrsOp::new(vfsi_core::Target::File(self)).len(len))
    }

    /// Change permissions on the open file through the shared vector engine.
    pub fn chmod(&self, permissions: Permissions) -> VfResult<()> {
        self.set_attrs_op(
            vfsi_core::SetAttrsOp::new(vfsi_core::Target::File(self)).permissions(permissions),
        )
    }

    fn set_attrs_op(&self, op: vfsi_core::SetAttrsOp<vfsi_core::Target<'_, Self>>) -> VfResult<()> {
        let client = FsClient {
            inner: Arc::clone(&self.inner),
            limits: ResourceLimits::default(),
        };
        client.vsetattrs(&[op])
    }

    pub fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> FsWrite<'a, F> {
        FsWrite {
            file: self,
            offset: VfOffset::At(offset),
            data,
        }
    }

    pub fn read_request_at_into<'a>(
        &'a self,
        offset: u64,
        buffer: &'a mut [u8],
    ) -> FsReadInto<'a, F> {
        FsReadInto {
            file: self,
            offset: VfOffset::At(offset),
            buffer,
        }
    }

    pub fn read_native(&mut self, buffer: &mut [u8]) -> VfResult<usize> {
        self.read_into(buffer, VfOffset::Cur)
    }

    /// Collect the remaining bytes from this opened object, starting at its
    /// cursor, with an explicit logical payload limit. This does not reopen
    /// its path. Unlike standard `Read::read_to_end`, allocation is bounded.
    ///
    /// An exact-limit read uses at most a one-byte EOF probe. On overflow or
    /// I/O failure no buffer is returned and the cursor may have advanced,
    /// including the probe byte; this operation does not restore the cursor.
    pub fn read_to_end_with_limit(&mut self, max_bytes: usize) -> VfResult<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let remaining = max_bytes - data.len();
            if remaining == 0 {
                let mut probe = [0_u8; 1];
                if self.read_native(&mut probe)? == 0 {
                    return Ok(data);
                }
                return Err(VfError::client(0, libc::EFBIG as u32)
                    .with_context("read_to_end_with_limit", &self.path));
            }
            let chunk = remaining.min(crate::DEFAULT_READ_STREAM_CHUNK_BYTES);
            data.try_reserve(chunk).map_err(|_| {
                VfError::client(0, libc::ENOMEM as u32)
                    .with_context("read_to_end_with_limit", &self.path)
            })?;
            let start = data.len();
            data.resize(start + chunk, 0);
            let count = self.read_native(&mut data[start..])?;
            data.truncate(start + count);
            if count == 0 {
                return Ok(data);
            }
        }
    }

    pub fn write_native(&mut self, buffer: &[u8]) -> VfResult<usize> {
        self.write_from(buffer, VfOffset::Cur)
    }

    fn read_into(&self, buffer: &mut [u8], offset: VfOffset) -> VfResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let request = ReadOp::new(self.raw()?.clone(), offset, buffer.len());
        let result = self
            .inner
            .lock()
            .map_err(|_| poisoned())?
            .read_into_impl(&request, buffer)
            .map_err(|error| error.with_context("read", &self.path))?;
        validate_read_into_results(
            "read",
            std::slice::from_ref(&request),
            std::slice::from_ref(&result),
        )
        .map_err(|error| error.with_context("read", &self.path))?;
        Ok(result.read)
    }

    fn write_from(&self, buffer: &[u8], offset: VfOffset) -> VfResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let file = self.raw()?.clone();
        let result = self
            .inner
            .lock()
            .map_err(|_| poisoned())?
            .write_impl(WriteOpRef::new(&file, offset, buffer))
            .map_err(|error| error.with_context("write", &self.path))?;
        if result.written > buffer.len() {
            return Err(VfError::client(0, crate::ERR_IO).with_context("write", &self.path));
        }
        Ok(result.written)
    }

    pub fn sync_data(&self) -> VfResult<()> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .sync_data(self.raw()?)
            .map_err(|error| error.with_context("sync_data", &self.path))
    }

    pub fn sync_all(&self) -> VfResult<()> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .sync_all(self.raw()?)
            .map_err(|error| error.with_context("sync_all", &self.path))
    }

    /// Attempt to close without consuming the handle. A failed close leaves
    /// the handle armed so the caller can reconcile or retry cleanup explicitly.
    /// After an ambiguous close failure, remote state is unknown: do not resume
    /// data I/O just because `is_closed()` is false. This is local ownership,
    /// not proof that the server still has the descriptor open.
    pub fn try_close(&mut self) -> VfResult<()> {
        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .close_impl(file)
            .map_err(|error| error.with_context("close", &self.path))?;
        self.file = None;
        Ok(())
    }

    /// Consume and close the handle. On failure, `Drop` queues cleanup for
    /// a later operation or explicit cleanup drain; use
    /// [`try_close`](Self::try_close) to retain control.
    pub fn close(mut self) -> VfResult<()> {
        self.try_close()
    }

    /// Seek while retaining [`VfError`] protocol and path information.
    pub fn seek_native(&mut self, position: IoSeekFrom) -> VfResult<u64> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .seek_impl(self.raw()?, position)
            .map_err(|error| error.with_context("seek", &self.path))
    }
}

impl<F: FileSystem> Read for FsFile<F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_native(buffer).map_err(io_error)
    }
}

impl<F: FileSystem> Write for FsFile<F> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_native(buffer).map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sync_data().map_err(io_error)
    }
}

impl<F: FileSystem> Seek for FsFile<F> {
    fn seek(&mut self, position: IoSeekFrom) -> io::Result<u64> {
        self.seek_native(position).map_err(io_error)
    }
}

/// Typed read request for [`FsClient::vread_native`].
pub struct FsRead<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    length: usize,
}

/// Typed borrowed write request for [`FsClient::vwrite_native`].
pub struct FsWrite<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    data: &'a [u8],
}

/// Typed vector read into caller-provided storage.
pub struct FsReadInto<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    buffer: &'a mut [u8],
}

impl<F: FileSystem> Drop for FsFile<F> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            self.inner.defer(
                CleanupTarget::File(file, std::mem::take(&mut self.path)),
                |backend, target| {
                    let CleanupTarget::File(file, path) = target else {
                        unreachable!()
                    };
                    backend
                        .close_deferred(file)
                        .map_err(|error| error.with_context("close", path))
                },
            );
        }
    }
}

impl<F: FileSystem> FsClient<F> {
    /// Query filesystems using one native vector of paths and retained handles.
    pub(crate) fn vstatfs<P: vfsi_core::AsTarget<FsFile<F>>>(
        &self,
        targets: &[P],
    ) -> VfResult<Vec<crate::FilesystemStats>> {
        use vfsi_core::Target;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut files = Vec::with_capacity(targets.len());
        let mut paths = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (raw, path) = match target.as_target() {
                Target::Path(path) => (VfFile::from_os_path(path), path),
                Target::File(file) => {
                    if !Arc::ptr_eq(&self.inner, &file.inner) {
                        return Err(VfError::client(index, crate::ERR_INVAL)
                            .with_context("vstatfs", &file.path));
                    }
                    (
                        file.raw()
                            .map_err(|e| e.with_index(index).with_context("vstatfs", &file.path))?
                            .clone(),
                        file.path.as_path(),
                    )
                }
            };
            files.push(raw);
            paths.push(path);
        }
        let results = self
            .lock()?
            .vstatfs_impl(&files)
            .map_err(|error| match error.index() {
                Some(index) if index < paths.len() => error.with_context("vstatfs", paths[index]),
                Some(_) => {
                    VfError::transport(None, "statfs backend returned an invalid error index")
                }
                None => error,
            })?;
        if results.len() != targets.len() {
            return Err(VfError::transport(
                None,
                "statfs backend returned an invalid result count",
            ));
        }
        Ok(results)
    }
    /// Update paths and open objects with one native attribute vector.
    pub(crate) fn vsetattrs<P: vfsi_core::AsTarget<FsFile<F>>>(
        &self,
        updates: &[vfsi_core::SetAttrsOp<P>],
    ) -> VfResult<()> {
        use vfsi_core::Target;
        if updates.is_empty() {
            return Ok(());
        }
        let mut paths = Vec::with_capacity(updates.len());
        let mut attrs = Vec::with_capacity(updates.len());
        for (index, op) in updates.iter().enumerate() {
            let target = op.target();
            let (raw, path) = match target.as_target() {
                Target::Path(path) => (Target::Path(path), path),
                Target::File(file) => {
                    if !Arc::ptr_eq(&self.inner, &file.inner) {
                        return Err(VfError::client(index, crate::ERR_INVAL)
                            .with_context("vsetattrs", &file.path));
                    }
                    (
                        Target::File(file.raw().map_err(|e| {
                            e.with_index(index).with_context("vsetattrs", &file.path)
                        })?),
                        file.path.as_path(),
                    )
                }
            };
            if op.requested_uid() == Some(u32::MAX) || op.requested_gid() == Some(u32::MAX) {
                return Err(
                    VfError::client(index, crate::ERR_INVAL).with_context("vsetattrs", path)
                );
            }
            paths.push(path);
            for time in [op.requested_accessed(), op.requested_modified()]
                .into_iter()
                .flatten()
            {
                crate::native::system_time_parts(time)
                    .map_err(|e| e.with_index(index).with_context("vsetattrs", path))?;
            }
            attrs.push(op.with_target(raw));
        }
        let map_error = |error: VfError, start: usize, count: usize| match error.index() {
            Some(index) if index < count => error
                .with_index(start + index)
                .with_context("vsetattrs", paths[start + index]),
            Some(_) => VfError::transport(None, "setattrs backend returned an invalid error index"),
            None => error,
        };
        let mut backend = self.lock()?;
        let follow = updates[0].follows_symlinks();
        if updates.iter().all(|op| op.follows_symlinks() == follow) {
            return backend
                .vsetattrs_impl(&attrs)
                .map_err(|error| map_error(error, 0, updates.len()));
        }
        let mut start = 0;
        while start < updates.len() {
            let follow = updates[start].follows_symlinks();
            let count = updates[start..]
                .iter()
                .take_while(|op| op.follows_symlinks() == follow)
                .count();
            backend
                .vsetattrs_impl(&attrs[start..start + count])
                .map_err(|error| map_error(error, start, count))?;
            start += count;
        }
        Ok(())
    }
}

fn link_error(error: VfError, paths: &[&Path], operation: &'static str) -> VfError {
    match error.index() {
        Some(index) if index < paths.len() => error.with_context(operation, paths[index]),
        Some(_) => VfError::transport(None, "link backend returned an invalid error index"),
        None => error,
    }
}
