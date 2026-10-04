//! Owned, shareable synchronous client and file handles.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom as IoSeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use vfsi_core::api::internal::OwnedReadResult as FsReadResult;
use vfsi_core::api::{
    DirectoryListing, ReadIntoResult as FsReadIntoResult, ResourceLimits, StreamCompletion,
    TraversalCompletion, WriteResult as FsWriteResult,
};

use crate::traits::{validate_read_into_results, validate_read_results, validate_write_results};
use crate::{
    AttrMask, Capabilities, CopyFileSystem, DirEntry, DirectoryFileSystem, FileSystem,
    LinkFileSystem, Metadata, MetadataFileSystem, MetadataQuery, MetadataUpdate,
    NamespaceFileSystem, OpenFlags, OpenRequest, Permissions, ReadAllOptions, ReadDirOptions,
    ReadOp, ReadResult, ReadStreamOptions, RemoveOptions, VecFs, VectorFileSystem, VfDir, VfError,
    VfFile, VfOffset, VfResult, WriteOpRef, WriteResult,
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
    pub fn capabilities(&self) -> VfResult<Capabilities> {
        Ok(self.lock()?.capabilities())
    }
    /// Open a path read-only.
    pub fn open(&self, path: impl AsRef<Path>) -> VfResult<FsFile<F>> {
        self.open_with(OpenRequest::new(path.as_ref(), OpenFlags::READ))
    }

    pub fn open_with(&self, request: OpenRequest) -> VfResult<FsFile<F>> {
        let file = self.lock()?.open_one(&request)?;
        Ok(FsFile {
            inner: Arc::clone(&self.inner),
            file: Some(file),
            path: request.path,
        })
    }

    pub fn open_options(&self) -> OpenOptions<'_, F> {
        OpenOptions::new(self)
    }

    pub fn create(&self, path: impl AsRef<Path>) -> VfResult<FsFile<F>> {
        self.open_with(OpenRequest::new(
            path.as_ref(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        ))
    }

    pub fn read(&self, path: impl AsRef<Path>) -> VfResult<Vec<u8>> {
        self.read_with_limit(path, self.limits.max_read_bytes)
    }

    /// Read one complete file while limiting the returned allocation.
    ///
    /// Use [`FsFile::read_native`] or [`std::io::Read`] to stream files that
    /// should not be held in one allocation.
    pub fn read_with_limit(&self, path: impl AsRef<Path>, max_bytes: usize) -> VfResult<Vec<u8>> {
        let path = path.as_ref();
        let file = self.open(path)?;
        let operation = self
            .lock()?
            .read_file(file.raw()?, max_bytes)
            .map_err(|error| error.with_context("read", path))
            .and_then(|data| {
                if data.len() > max_bytes {
                    Err(VfError::client(0, libc::EFBIG as u32).with_context("read", path))
                } else {
                    Ok(data)
                }
            });
        // Owned cleanup queues a retry through Drop on close failure, and keeps
        // the read error primary when both read and close fail.
        let cleanup = file.close();
        operation.and_then(|data| cleanup.map(|()| data))
    }

    pub fn read_to_string(&self, path: impl AsRef<Path>) -> VfResult<String> {
        self.read_to_string_with_limit(path, self.limits.max_read_bytes)
    }

    /// Read one complete UTF-8 file with a caller-selected allocation limit.
    pub fn read_to_string_with_limit(
        &self,
        path: impl AsRef<Path>,
        max_bytes: usize,
    ) -> VfResult<String> {
        let path = path.as_ref();
        String::from_utf8(self.read_with_limit(path, max_bytes)?)
            .map_err(|_| VfError::client(0, crate::ERR_INVAL).with_context("read_to_string", path))
    }

    pub fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> VfResult<()> {
        let path = path.as_ref();
        let mut file = self.create(path)?;
        let mut written = 0;
        while written < data.len() {
            let count = file.write_native(&data[written..])?;
            if count == 0 {
                return Err(VfError::client(0, crate::ERR_IO).with_context("write", path));
            }
            written += count;
        }
        file.close()
    }
}

impl<F: FileSystem> FsClient<F> {
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
        callback: impl FnMut(u64, &[u8]) -> VfResult<bool>,
    ) -> VfResult<StreamCompletion> {
        self.read_stream_with_options(
            path,
            ReadStreamOptions::new().chunk_size(self.limits.stream_chunk_bytes),
            callback,
        )
    }

    /// Stream one file using an explicit maximum chunk size.
    pub fn read_stream_with_options(
        &self,
        path: impl AsRef<Path>,
        options: ReadStreamOptions,
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
                    let result = backend.read_one(&request)?;
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

impl<F: MetadataFileSystem> FsClient<F> {
    pub fn metadata(&self, path: impl AsRef<Path>) -> VfResult<Metadata> {
        self.lock()?.metadata_path(path.as_ref(), true)
    }

    pub fn symlink_metadata(&self, path: impl AsRef<Path>) -> VfResult<Metadata> {
        self.lock()?.metadata_path(path.as_ref(), false)
    }

    pub fn set_metadata(&self, path: impl AsRef<Path>) -> SetMetadata<'_, F> {
        SetMetadata {
            client: self,
            path: path.as_ref().to_path_buf(),
            update: MetadataUpdate::new(),
            follow: true,
        }
    }
}

impl<F: DirectoryFileSystem> FsClient<F> {
    pub fn create_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        self.create_dir_with_mode(path, 0o777)
    }

    pub fn create_dir_with_mode(&self, path: impl AsRef<Path>, mode: u32) -> VfResult<()> {
        self.lock()?.create_dir_one(path.as_ref(), mode)
    }

    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfResult<Vec<DirEntry>> {
        self.read_dir_with_options(path, self.limits.directory_options())
    }

    /// Read one directory with explicit entry and path-storage limits.
    pub fn read_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: ReadDirOptions,
    ) -> VfResult<Vec<DirEntry>> {
        self.lock()?.read_dir_one(path.as_ref(), options)
    }

    /// Visit one directory one bounded page at a time. `Continue(())` requests
    /// the next entry; `Break(())` stops the entire visit successfully.
    /// The callback runs without the lock and may reenter or drop its files.
    pub fn visit_dir(
        &self,
        path: impl AsRef<Path>,
        callback: impl FnMut(DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<TraversalCompletion> {
        self.visit_dir_with_options(path, self.limits.directory_options(), callback)
    }

    /// Visit entries with explicit entry and cumulative path-byte limits.
    pub fn visit_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: ReadDirOptions,
        callback: impl FnMut(DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<TraversalCompletion> {
        self.visit_dir_with_fields(path, crate::native::metadata_mask(), options, callback)
    }

    pub fn visit_dir_with_fields(
        &self,
        path: impl AsRef<Path>,
        fields: AttrMask,
        options: ReadDirOptions,
        mut callback: impl FnMut(DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<TraversalCompletion> {
        const PAGE_SIZE: usize = 1024;
        let path = path.as_ref();
        let mut cursor = None;
        let mut count = 0usize;
        let mut path_bytes = 0usize;
        let max_entries = if options.entry_limit() == usize::MAX {
            0 // VecFs uses zero for an explicitly unlimited listing.
        } else {
            options.entry_limit().saturating_add(1)
        };
        loop {
            // Start with one entry so early-stop callbacks and tight limits
            // do not trigger a large local scan before application code runs.
            let page_size = if cursor.is_none() {
                1
            } else {
                PAGE_SIZE.min(
                    options
                        .entry_limit()
                        .saturating_sub(count)
                        .saturating_add(1),
                )
            };
            let (entries, next) = {
                self.lock()?.read_dir_page_with_fields(
                    path,
                    fields,
                    cursor,
                    page_size,
                    max_entries,
                )?
            };
            if entries.is_empty() && next.is_some() {
                return Err(VfError::transport(None, "directory page made no progress"));
            }
            for entry in entries {
                if count >= options.entry_limit() {
                    return Err(
                        VfError::failure(count, libc::EFBIG as u32).with_context("visit_dir", path)
                    );
                }
                path_bytes = path_bytes
                    .checked_add(entry.path().as_os_str().len())
                    .ok_or_else(|| {
                        VfError::failure(count, libc::EFBIG as u32).with_context("visit_dir", path)
                    })?;
                if path_bytes > options.path_byte_limit() {
                    return Err(
                        VfError::failure(count, libc::EFBIG as u32).with_context("visit_dir", path)
                    );
                }
                count += 1;
                if callback(entry)?.is_break() {
                    return Ok(TraversalCompletion::Stopped);
                }
            }
            match next {
                Some(value) => cursor = Some(value),
                None => return Ok(TraversalCompletion::Complete),
            }
        }
    }
}

impl<F: DirectoryFileSystem + MetadataFileSystem> FsClient<F> {
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        let mut current = PathBuf::new();
        for component in path.components() {
            match component {
                Component::RootDir => {
                    current.push(Path::new("/"));
                    continue;
                }
                Component::Normal(part) => current.push(part),
                Component::CurDir => continue,
                Component::ParentDir => {
                    // Preserve `..` so the backend resolves it relative to
                    // its own rooted namespace. Lexically popping `/` would
                    // accidentally turn a later absolute component into a
                    // current-directory-relative path.
                    current.push("..");
                    continue;
                }
                Component::Prefix(_) => return Err(VfError::client(0, crate::ERR_INVAL)),
            }
            match self.create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.err_no() == crate::ERR_EXIST => {
                    if !self.metadata(&current)?.is_dir() {
                        return Err(VfError::client(0, crate::ERR_NOTDIR)
                            .with_context("create_dir_all", &current));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

impl<F: NamespaceFileSystem + MetadataFileSystem> FsClient<F> {
    fn removal_metadata(&self, path: &Path, operation: &'static str) -> VfResult<Metadata> {
        let mut filesystem = self.lock()?;
        let follow = !filesystem.capabilities().contains(Capabilities::LSTAT);
        filesystem
            .metadata_path(path, follow)
            .map_err(|error| error.with_context(operation, path))
    }

    pub fn remove_file(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        if self.removal_metadata(path, "remove_file")?.is_dir() {
            return Err(VfError::client(0, crate::ERR_ISDIR).with_context("remove_file", path));
        }
        self.lock()?.remove_one(path, false)
    }

    pub fn remove_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        if !self.removal_metadata(path, "remove_dir")?.is_dir() {
            return Err(VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir", path));
        }
        self.lock()?.remove_one(path, false)
    }

    pub fn remove_dir_all(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        if !self.removal_metadata(path, "remove_dir_all")?.is_dir() {
            return Err(VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir_all", path));
        }
        self.lock()?.remove_one(path, true)
    }

    /// Remove the contents of a directory, keeping the directory itself.
    pub fn remove_dir_contents(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        if !self.removal_metadata(path, "remove_dir_contents")?.is_dir() {
            return Err(
                VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir_contents", path)
            );
        }
        self.lock()?.remove_dir_contents(path)
    }

    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.rename_one(from.as_ref(), to.as_ref())
    }
}

impl<F: VecFs> FsClient<F> {
    /// Rename independent source/destination pairs in one vector phase.
    pub fn renamev<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> VfResult<()> {
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
            .renamev(&requests)
            .map_err(|error| match error.index() {
                Some(index) if index < pairs.len() => {
                    error.with_context("renamev", pairs[index].0.as_ref())
                }
                Some(_) => {
                    VfError::transport(None, "rename backend returned an invalid error index")
                }
                None => error,
            })
    }

    /// Create directories in input order using vector MKDIR. Parents must
    /// already exist. This is not transactional: failure may leave a prefix.
    pub fn mkdirv<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<()> {
        let directories: Vec<_> = paths.iter().map(|path| (path.as_ref(), 0o777)).collect();
        self.mkdirv_with_modes(&directories)
    }

    /// Create directories with per-request Unix permission bits.
    pub fn mkdirv_with_modes<P: AsRef<Path>>(&self, directories: &[(P, u32)]) -> VfResult<()> {
        let mut seen = HashSet::with_capacity(directories.len());
        for (index, (path, _)) in directories.iter().enumerate() {
            if !seen.insert(path.as_ref()) {
                return Err(
                    VfError::client(index, crate::ERR_INVAL).with_context("mkdirv", path.as_ref())
                );
            }
        }
        if directories.is_empty() {
            return Ok(());
        }
        let dirs: Vec<_> = directories
            .iter()
            .map(|(path, mode)| crate::VfAttrs {
                file: VfFile::from_os_path(path.as_ref()),
                masks: AttrMask::MODE,
                mode: *mode,
                ..Default::default()
            })
            .collect();
        self.lock()?
            .mkdirv(&dirs)
            .map_err(|error| match error.index() {
                Some(index) if index < directories.len() => {
                    error.with_context("mkdirv", directories[index].0.as_ref())
                }
                Some(_) => {
                    VfError::transport(None, "mkdir backend returned an invalid error index")
                }
                None => error,
            })
    }

    /// Create symbolic links in one native backend vector.
    pub fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let targets: Vec<_> = pairs.iter().map(|(target, _)| target.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        self.lock()?
            .symlinkv(&targets, &links)
            .map_err(|error| link_error(error, &links, "vsymlink"))
    }

    /// Read targets without converting Unix path bytes through UTF-8.
    pub fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        let targets = self
            .lock()?
            .readlinkv(&paths)
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
    pub fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let sources: Vec<_> = pairs.iter().map(|(source, _)| source.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        self.lock()?
            .hardlinkv(&sources, &links)
            .map_err(|error| link_error(error, &links, "vhardlink"))
    }

    /// Lazy between directories, with selective no-follow metadata and pruning.
    /// Enter runs before any listing; Leave follows even a pruned directory.
    /// Sorting buffers one bounded directory, not the whole tree. The callback
    /// runs outside the backend lock. Limits include the starting object.
    pub fn walk_events_with_options(
        &self,
        root: impl AsRef<Path>,
        fields: AttrMask,
        options: crate::WalkOptions,
        sort_by_name: bool,
        callback: impl FnMut(&crate::WalkEvent) -> VfResult<crate::WalkControl>,
    ) -> VfResult<TraversalCompletion> {
        let root = root.as_ref();
        let metadata = self.symlink_metadata_with_fields(root, fields | AttrMask::MODE)?;
        crate::walk_events(
            DirEntry::new(root.to_path_buf(), metadata),
            options,
            sort_by_name,
            |path, limits| {
                let mut listings =
                    self.read_dirs_with_options(&[path], fields | AttrMask::MODE, limits)?;
                if listings.len() != 1 {
                    return Err(VfError::transport(None, "invalid directory result count"));
                }
                Ok(listings.remove(0).entries)
            },
            callback,
        )
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
        callback: impl FnMut(&DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<TraversalCompletion> {
        self.visit_walk_with_options(root, self.limits.walk_options(), callback)
    }

    pub fn visit_walk_with_options(
        &self,
        root: impl AsRef<Path>,
        options: crate::WalkOptions,
        callback: impl FnMut(&DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<TraversalCompletion> {
        self.visit_walk_with_fields(root, crate::native::metadata_mask(), options, callback)
    }

    pub fn visit_walk_with_fields(
        &self,
        root: impl AsRef<Path>,
        fields: AttrMask,
        options: crate::WalkOptions,
        callback: impl FnMut(&DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<TraversalCompletion> {
        let root = root.as_ref();
        if !self.symlink_metadata(root)?.is_dir() {
            return Err(VfError::client(0, crate::ERR_NOTDIR).with_context("visit_walk", root));
        }
        visit_walk_pages(
            root,
            options,
            |path, cursor, page_size, max_entries| {
                self.lock()?
                    .read_dir_page_with_fields(path, fields, cursor, page_size, max_entries)
            },
            callback,
        )
    }
    /// Maximum safe cohort size for incremental directory paging.
    pub fn directory_page_batch_size(&self) -> VfResult<usize> {
        Ok(self.lock()?.directory_page_batch_size().clamp(1, 32))
    }
    /// Fetch a bounded vector of directory pages, retaining backend cursors.
    pub fn read_dir_pages_with_fields(
        &self,
        paths: &[&Path],
        fields: AttrMask,
        cursors: Vec<Option<crate::DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<Vec<crate::DirectoryPage>> {
        let pages = self.lock()?.listdir_pages(
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

    /// List several directories with common stat attributes and finite
    /// allocation limits. Use `read_dirs_with_options` for richer fields.
    pub fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<DirectoryListing>> {
        self.read_dirs_with_options(paths, AttrMask::stat(), self.limits.directory_options())
    }
}

type DirectoryPage = (Vec<DirEntry>, Option<crate::DirPageCursor>);

fn visit_walk_pages(
    root: &Path,
    options: crate::WalkOptions,
    mut read_page: impl FnMut(
        &Path,
        Option<crate::DirPageCursor>,
        usize,
        usize,
    ) -> VfResult<DirectoryPage>,
    mut callback: impl FnMut(&DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
) -> VfResult<TraversalCompletion> {
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut count = 0usize;
    let mut path_bytes = root.as_os_str().len();
    if path_bytes > options.path_byte_limit() {
        return Err(VfError::client(0, libc::EFBIG as u32));
    }
    while let Some((path, depth)) = pending.pop() {
        let mut cursor = None;
        let mut children = Vec::new();
        loop {
            let requested = options
                .entry_limit()
                .saturating_sub(count)
                .saturating_add(1);
            let (entries, next) = read_page(&path, cursor, requested.min(128), requested)?;
            if entries.is_empty() && next.is_some() {
                return Err(VfError::transport(None, "directory page made no progress")
                    .with_context("visit_walk", &path));
            }
            for entry in entries {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| VfError::client(0, libc::EFBIG as u32))?;
                path_bytes = path_bytes
                    .checked_add(entry.path().as_os_str().len())
                    .ok_or_else(|| VfError::client(0, libc::EFBIG as u32))?;
                if count > options.entry_limit() || path_bytes > options.path_byte_limit() {
                    return Err(
                        VfError::client(0, libc::EFBIG as u32).with_context("visit_walk", &path)
                    );
                }
                if callback(&entry)?.is_break() {
                    return Ok(TraversalCompletion::Stopped);
                }
                if entry.metadata().is_dir() {
                    if depth >= options.depth_limit() {
                        if !options.truncates_at_depth_limit() {
                            return Err(VfError::client(0, libc::EFBIG as u32)
                                .with_context("visit_walk", entry.path()));
                        }
                    } else {
                        children.push((entry.path().to_path_buf(), depth + 1));
                    }
                }
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        pending.extend(children.into_iter().rev());
    }
    Ok(TraversalCompletion::Complete)
}

impl<F: VecFs> FsClient<F> {
    /// Recursively enumerate a bounded tree with common stat attributes.
    /// Use `walk_with_options` to select fields or change limits.
    pub fn walk(&self, root: impl AsRef<Path>) -> VfResult<Vec<DirectoryListing>> {
        self.walk_with_options(root, AttrMask::stat(), self.limits.walk_options())
    }

    /// Create `path` if missing, otherwise empty it. Errors if it exists and is
    /// not a directory (a symlink to a directory is not a directory here).
    pub fn ensure_empty_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.ensure_empty_dir(path.as_ref())
    }

    /// Remove a directory tree with explicit error, batching, and retry policy.
    pub fn remove_dir_all_with_options(
        &self,
        path: impl AsRef<Path>,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let path = path.as_ref();
        if !self.removal_metadata(path, "remove_dir_all")?.is_dir() {
            return Err(VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir_all", path));
        }
        self.lock()?
            .rm_with_options(&[path], true, options)
            .map_err(|error| error.with_context("remove_dir_all", path))
    }

    /// Empty a directory while keeping it, with explicit removal policy.
    pub fn remove_dir_contents_with_options(
        &self,
        path: impl AsRef<Path>,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let path = path.as_ref();
        if !self.removal_metadata(path, "remove_dir_contents")?.is_dir() {
            return Err(
                VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir_contents", path)
            );
        }
        self.lock()?
            .rm_contents_with_options(path, options)
            .map_err(|error| error.with_context("remove_dir_contents", path))
    }

    /// Open a *genuine* directory handle for race-resistant, handle-rooted
    /// removal. Backends that only return a path fail instead of silently
    /// losing the handle safety guarantee.
    pub fn open_dir_handle(&self, path: impl AsRef<Path>) -> VfResult<FsDir<F>> {
        let path = path.as_ref();
        let mut backend = self.lock()?;
        let dir = backend.open_dir(path)?;
        if !matches!(dir, VfDir::Descriptor { .. }) {
            let _ = backend.close_dir(&dir);
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
pub struct FsDir<F: VecFs> {
    inner: Arc<SharedBackend<F>>,
    dir: Option<VfDir>,
    path: PathBuf,
}

impl<F: VecFs> fmt::Debug for FsDir<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsDir")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<F: VecFs> FsDir<F> {
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
            .rm_dir_contents_with_options(dir, options)
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
        self.inner.lock().map_err(|_| poisoned())?.close_dir(dir)?;
        self.dir = None;
        Ok(())
    }
}

impl<F: VecFs> Drop for FsDir<F> {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            self.inner.defer(
                CleanupTarget::Directory(dir, std::mem::take(&mut self.path)),
                |backend, target| {
                    let CleanupTarget::Directory(dir, path) = target else {
                        unreachable!()
                    };
                    backend
                        .close_dir(dir)
                        .map_err(|error| error.with_context("close_dir", path))
                },
            );
        }
    }
}

impl<F: LinkFileSystem> FsClient<F> {
    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.symlink_one(target.as_ref(), link.as_ref())
    }

    pub fn hard_link(&self, source: impl AsRef<Path>, link: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.hard_link_one(source.as_ref(), link.as_ref())
    }

    pub fn read_link(&self, path: impl AsRef<Path>) -> VfResult<PathBuf> {
        self.lock()?.read_link_one(path.as_ref())
    }
}

impl<F: CopyFileSystem> FsClient<F> {
    pub fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.copy_one(source.as_ref(), destination.as_ref())
    }
}

impl<F: VecFs> FsClient<F> {
    /// Fetch selected metadata for one path without following its final symlink.
    /// Unavailable fields remain `None` on [`Metadata`].
    pub fn symlink_metadata_with_fields(
        &self,
        path: impl AsRef<Path>,
        fields: AttrMask,
    ) -> VfResult<Metadata> {
        let path = path.as_ref();
        let mut attrs = crate::VfAttrs {
            file: VfFile::from_os_path(path),
            masks: fields | AttrMask::MODE | AttrMask::SIZE,
            ..crate::VfAttrs::default()
        };
        self.lock()?
            .lgetattrsv(std::slice::from_mut(&mut attrs))
            .map_err(|error| error.with_context("symlink_metadata", path))?;
        Ok(vfsi_core::metadata_from_attrs(attrs))
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
        fields: AttrMask,
        options: ReadDirOptions,
    ) -> VfResult<Vec<DirectoryListing>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<&Path> = paths.iter().map(AsRef::as_ref).collect();
        let mut positions: HashMap<PathBuf, Vec<usize>> = HashMap::with_capacity(paths.len());
        let mut unique_paths = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            if let Some(indices) = positions.get_mut(*path) {
                indices.push(index);
            } else {
                positions.insert(path.to_path_buf(), vec![index]);
                unique_paths.push(*path);
            }
        }
        let mut listings: Vec<DirectoryListing> = paths
            .iter()
            .map(|path| DirectoryListing {
                path: path.to_path_buf(),
                entries: Vec::new(),
            })
            .collect();
        let mut entry_count = 0usize;
        let mut path_bytes = 0usize;
        // Bound the number of simultaneous first READDIR pages even when the
        // caller supplies thousands of directory operands.
        const DIRECTORY_COHORT: usize = 32;
        for cohort in unique_paths.chunks(DIRECTORY_COHORT) {
            let mut callback_error = None;
            let requested = options
                .entry_limit()
                .saturating_sub(entry_count)
                .saturating_add(1);
            let result = self.lock()?.listdirv(
                cohort,
                fields | AttrMask::MODE | AttrMask::SIZE,
                requested,
                false,
                &mut |attributes, directory| {
                    let Some(indices) = positions.get(directory) else {
                        callback_error = Some(VfError::transport(
                            None,
                            format!(
                                "read_dirs backend returned an unexpected directory: {}",
                                directory.display()
                            ),
                        ));
                        return false;
                    };
                    if !cohort.contains(&directory) {
                        callback_error = Some(VfError::transport(
                            None,
                            "read_dirs backend returned a directory outside the active cohort",
                        ));
                        return false;
                    }
                    let Some(path) = attributes.file.path() else {
                        callback_error = Some(VfError::transport(
                            None,
                            "read_dirs backend returned an entry without a path",
                        ));
                        return false;
                    };
                    if path.parent() != Some(directory) {
                        callback_error = Some(VfError::transport(
                            None,
                            format!(
                                "read_dirs backend returned an entry outside {}",
                                directory.display()
                            ),
                        ));
                        return false;
                    }
                    for &index in indices {
                        entry_count += 1;
                        path_bytes = path_bytes.saturating_add(path.as_os_str().len());
                        if entry_count > options.entry_limit()
                            || path_bytes > options.path_byte_limit()
                        {
                            callback_error = Some(
                                VfError::failure(index, libc::EFBIG as u32)
                                    .with_context("read_dirs", directory),
                            );
                            return false;
                        }
                        listings[index].entries.push(DirEntry::new(
                            path.to_path_buf(),
                            vfsi_core::metadata_from_attrs(attributes.clone()),
                        ));
                    }
                    true
                },
            );
            if let Some(error) = callback_error {
                return Err(error);
            }
            result.map_err(|error| {
                error.map_index(|index| {
                    cohort
                        .get(index)
                        .and_then(|path| positions.get(*path))
                        .and_then(|indices| indices.first())
                        .copied()
                        .unwrap_or(index)
                })
            })?;
        }
        Ok(listings)
    }

    /// Recursively enumerate directories with selected entry attributes.
    /// The walk is bounded by `options`; sorting and presentation remain the
    /// application's responsibility.
    pub fn walk_with_options(
        &self,
        root: impl AsRef<Path>,
        fields: AttrMask,
        options: crate::WalkOptions,
    ) -> VfResult<Vec<DirectoryListing>> {
        let root = root.as_ref();
        let tree = self.lock()?.walk_with_options(
            root,
            fields | AttrMask::MODE | AttrMask::SIZE,
            options,
            &mut |_, _| {},
        )?;
        tree.into_iter()
            .map(|directory| {
                let entries = directory
                    .entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, attributes)| {
                        let path = attributes
                            .file
                            .path()
                            .ok_or_else(|| {
                                VfError::transport(
                                    None,
                                    format!("walk backend returned entry {index} without a path"),
                                )
                            })?
                            .to_path_buf();
                        Ok(DirEntry::new(
                            path,
                            vfsi_core::metadata_from_attrs(attributes),
                        ))
                    })
                    .collect::<VfResult<Vec<_>>>()?;
                Ok(DirectoryListing {
                    path: directory.path,
                    entries,
                })
            })
            .collect()
    }

    /// Copy whole files in request order. A successful prefix may remain if
    /// a later request fails; this operation does not provide atomicity.
    pub fn copyv<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> VfResult<()> {
        let extents: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                crate::ExtentPair::from_os_paths(from.as_ref(), 0, to.as_ref(), 0, None)
            })
            .collect();
        self.lock()?.copyv(&extents).map_err(|error| {
            error
                .index()
                .and_then(|index| pairs.get(index))
                .map_or(error.clone(), |(_, to)| {
                    error.with_context("copyv", to.as_ref())
                })
        })
    }

    /// Remove paths in request order, optionally recursing into directories.
    /// A successful prefix may remain if a later path fails.
    pub fn remove_paths<P: AsRef<Path>>(&self, paths: &[P], recursive: bool) -> VfResult<()> {
        self.remove_paths_with_options(paths, recursive, RemoveOptions::default())
    }

    /// Remove paths with explicit error, batching, and retry policy.
    pub fn remove_paths_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let paths: Vec<&Path> = paths.iter().map(AsRef::as_ref).collect();
        self.lock()?
            .rm_with_options(&paths, recursive, options)
            .map_err(|error| {
                error
                    .index()
                    .and_then(|index| paths.get(index))
                    .map_or(error.clone(), |path| {
                        error.with_context("remove_paths", path)
                    })
            })
    }

    /// Vector metadata query with explicit fields and final-symlink handling.
    /// Ancestor symlinks follow the backend's normal namespace semantics.
    pub fn metadata_many<P: AsRef<Path>>(
        &self,
        paths: &[P],
        fields: AttrMask,
        follow: bool,
    ) -> VfResult<Vec<Metadata>> {
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
                backend.getattrsv(&mut attrs)
            } else {
                backend.lgetattrsv(&mut attrs)
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
    pub fn symlink_metadatav(&self, paths: &[&Path]) -> VfResult<Vec<Metadata>> {
        let mut attrs: Vec<_> = paths
            .iter()
            .map(|path| crate::VfAttrs {
                file: VfFile::from_os_path(path),
                masks: AttrMask::MODE,
                ..crate::VfAttrs::default()
            })
            .collect();
        self.lock()?.lgetattrsv(&mut attrs).map_err(|error| {
            error
                .index()
                .and_then(|index| paths.get(index))
                .map_or(error.clone(), |path| {
                    error.with_context("symlink_metadatav", path)
                })
        })?;
        Ok(attrs
            .into_iter()
            .map(vfsi_core::metadata_from_attrs)
            .collect())
    }
}

impl<F: VectorFileSystem> FsClient<F> {
    /// Open an ordered vector of files.
    ///
    /// Success returns one RAII handle per request. Failure returns no
    /// handles; VFSI does not promise transactional rollback of other
    /// filesystem effects such as file creation.
    pub fn openv(&self, requests: &[OpenRequest]) -> VfResult<Vec<FsFile<F>>> {
        let mut filesystem = self.lock()?;
        let files = filesystem.open_many(requests).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("openv", &request.path)
                })
        })?;
        if files.len() != requests.len() {
            let error = wrong_result_count("openv", requests.len(), files.len());
            for file in &files {
                let _ = filesystem.close_one(file);
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
    pub fn try_closev<'a>(&self, files: impl IntoIterator<Item = &'a mut FsFile<F>>) -> VfResult<()>
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
        self.lock()?.close_many(&descriptors).map_err(|error| {
            error
                .index()
                .and_then(|index| files.get(index))
                .map_or(error.clone(), |file| {
                    error.with_context("closev", file.path())
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
    /// best-effort scalar cleanup attempts. Use [`try_closev`](Self::try_closev)
    /// to retain the handles after an error.
    pub fn closev(&self, mut files: Vec<FsFile<F>>) -> VfResult<()> {
        self.try_closev(&mut files)
    }

    /// Read an ordered vector with a 16 MiB aggregate request limit.
    /// Use [`readv_with_limit`](Self::readv_with_limit) to tune the limit or
    /// [`readv_into`](Self::readv_into) to provide bounded caller-owned buffers.
    pub fn readv(&self, requests: &[FsRead<'_, F>]) -> VfResult<Vec<FsReadResult>> {
        self.readv_with_limit(requests, self.limits.max_read_bytes)
    }

    /// Read an ordered vector with an explicit aggregate request limit.
    pub fn readv_with_limit(
        &self,
        requests: &[FsRead<'_, F>],
        max_total_bytes: usize,
    ) -> VfResult<Vec<FsReadResult>> {
        self.readv_with_limit_projected(requests, max_total_bytes, |request| request)
    }

    /// Backend adapter for opaque application requests; projection does not allocate.
    /// The projection must return the same embedded request on every invocation.
    pub fn readv_with_limit_projected<'b, T>(
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
                    .with_context("readv", request.file.path()));
            }
        }
        let reads = self.read_ops(requests, &project)?;
        let results = self.lock()?.read_many(&reads).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("readv", request.file.path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count("readv", requests.len(), results.len()));
        }
        validate_read_results("readv", &reads, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("readv", request.file.path())
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

    pub fn readv_into(
        &self,
        requests: &mut [FsReadInto<'_, F>],
    ) -> VfResult<Vec<FsReadIntoResult>> {
        self.readv_into_with_limit(requests, self.limits.max_read_bytes)
    }

    /// Read into caller storage with an explicit aggregate buffer budget.
    /// This also bounds allocation in copying fallback implementations.
    pub fn readv_into_with_limit(
        &self,
        requests: &mut [FsReadInto<'_, F>],
        max_bytes: usize,
    ) -> VfResult<Vec<FsReadIntoResult>> {
        self.readv_into_with_limit_projected(
            requests,
            max_bytes,
            |request| request,
            |request| request,
        )
    }

    /// Project borrowed buffers without an intermediate request allocation.
    /// Both projections must identify the same embedded request, and remain
    /// stable across preflight, dispatch, and result validation.
    pub fn readv_into_with_limit_projected<'b, T>(
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
            self.lock()?.read_many_into(&reads, &mut buffers)
        }
        .map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("readv_into", request.file.path())
                })
        })?;
        validate_read_into_results("readv_into", &reads, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("readv_into", request.file.path())
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

    pub fn writev(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<FsWriteResult>> {
        self.writev_projected(requests, |request| request)
    }

    /// Backend adapter for opaque application requests, preserving borrowed payloads.
    /// The projection must return the same embedded request on every invocation.
    pub fn writev_projected<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        self.writev_mapped(requests, |item| {
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
    pub fn writev_mapped<'b, T>(
        &self,
        requests: &[T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        let writes = self.write_ops(requests, &project)?;
        let results = self.lock()?.write_many(&writes).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("writev", request.file.path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count("writev", requests.len(), results.len()));
        }
        validate_write_results("writev", &writes, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("writev", request.file.path())
                })
        })?;
        Ok(results.into_iter().map(write_result).collect())
    }

    /// Write every byte in each positional request, retrying short writes in
    /// vector waves. Like `writev`, this is not transactional: an error may
    /// follow a successfully written prefix. Overlapping requests through the
    /// same path complete in input order; different paths are presumed
    /// independent (including hard-link aliases).
    pub fn write_allv(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<FsWriteResult>> {
        self.write_allv_projected(requests, |request| request)
    }

    /// Complete projected requests with the same preflight and dependency waves.
    /// The projection must return the same embedded request on every invocation.
    pub fn write_allv_projected<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        self.write_allv_mapped(requests, |item| {
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
    pub fn write_allv_mapped<'b, T>(
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
                    .with_context("write_allv", request.file.path())
            })?;
            let VfOffset::At(offset) = request.offset else {
                return Err(VfError::client(index, crate::ERR_INVAL)
                    .with_context("write_allv", request.file.path()));
            };
            let end = offset
                .checked_add(request.data.len() as u64)
                .ok_or_else(|| {
                    VfError::client(index, libc::EOVERFLOW as u32)
                        .with_context("write_allv", request.file.path())
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
                            .with_context("write_allv", request.file.path()));
                    };
                    let offset = offset.checked_add(totals[index] as u64).ok_or_else(|| {
                        VfError::client(index, libc::EOVERFLOW as u32)
                            .with_context("write_allv", request.file.path())
                    })?;
                    Ok(FsWrite {
                        file: request.file,
                        offset: VfOffset::At(offset),
                        data: &request.data[totals[index]..],
                    })
                })
                .collect::<VfResult<_>>()?;
            let results = self.writev(&wave).map_err(|error| {
                error.map_index(|index| wave_indices.get(index).copied().unwrap_or(index))
            })?;
            for (&index, result) in wave_indices.iter().zip(results) {
                if result.written == 0 {
                    return Err(VfError::client(index, crate::ERR_IO)
                        .with_context("write_allv", project(&requests[index]).file.path()));
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
                        .with_context("write_allv", request.file.path()));
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

impl<F: VectorFileSystem + VecFs> FsClient<F> {
    /// Read several complete files by path using vector READ operations.
    ///
    /// The aggregate returned data is limited to 16 MiB by default. Use
    /// [`read_files_with_options`](Self::read_files_with_options) to choose a
    /// different limit, or stream large files instead. This is not a snapshot
    /// or an atomic operation across files.
    pub fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<Vec<u8>>> {
        self.read_files_with_options(
            paths,
            ReadAllOptions::new().max_total_bytes(self.limits.max_read_bytes),
        )
    }

    /// Read several complete files with an explicit aggregate allocation limit.
    pub fn read_files_with_options<P: AsRef<Path>>(
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
            .read_allv_with_options(&files, options)
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

    /// Replace several files from borrowed buffers using vector OPEN, WRITE,
    /// and CLOSE phases. Identical path spellings are rejected before opening
    /// anything; aliases such as hard links are still the caller's responsibility.
    /// The batch is not transactional: an error may follow files already
    /// created or written. Large inputs should be chunked by the caller rather
    /// than held in memory solely for this convenience method.
    pub fn write_files<P: AsRef<Path>, B: AsRef<[u8]>>(&self, entries: &[(P, B)]) -> VfResult<()> {
        let mut seen = HashSet::with_capacity(entries.len());
        for (index, (path, _)) in entries.iter().enumerate() {
            if !seen.insert(path.as_ref()) {
                return Err(VfError::client(index, crate::ERR_INVAL)
                    .with_context("write_files", path.as_ref()));
            }
        }
        let requests: Vec<_> = entries
            .iter()
            .map(|(path, _)| {
                OpenRequest::new(
                    path.as_ref(),
                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                )
            })
            .collect();
        let files = self.openv(&requests)?;
        let writes: Vec<_> = files
            .iter()
            .zip(entries)
            .map(|(file, (_, data))| file.write_request_at(0, data.as_ref()))
            .collect();
        let result = self.write_allv(&writes);
        drop(writes);
        let close_result = self.closev(files);
        result?;
        close_result
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
            .open_with(OpenRequest::new(path.as_ref(), self.flags).mode(self.mode))
    }
}

impl<F: VectorFileSystem> OpenOptions<'_, F> {
    pub fn openv<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<FsFile<F>>> {
        let requests: Vec<OpenRequest> = paths
            .iter()
            .map(|path| OpenRequest::new(path.as_ref(), self.flags).mode(self.mode))
            .collect();
        self.client.openv(&requests)
    }
}

/// Builder for an atomic metadata update request.
pub struct SetMetadata<'a, F: MetadataFileSystem> {
    client: &'a FsClient<F>,
    path: PathBuf,
    update: MetadataUpdate,
    follow: bool,
}

impl<F: MetadataFileSystem> Clone for SetMetadata<'_, F> {
    fn clone(&self) -> Self {
        Self {
            client: self.client,
            path: self.path.clone(),
            update: self.update.clone(),
            follow: self.follow,
        }
    }
}

impl<F: MetadataFileSystem> fmt::Debug for SetMetadata<'_, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetMetadata")
            .field("path", &self.path)
            .field("update", &self.update)
            .field("follow", &self.follow)
            .finish_non_exhaustive()
    }
}

impl<F: MetadataFileSystem> SetMetadata<'_, F> {
    pub fn permissions(&mut self, permissions: Permissions) -> &mut Self {
        self.update.permissions = Some(permissions);
        self
    }

    pub fn len(&mut self, len: u64) -> &mut Self {
        self.update.len = Some(len);
        self
    }

    pub fn accessed(&mut self, accessed: SystemTime) -> &mut Self {
        self.update.accessed = Some(accessed);
        self
    }

    pub fn modified(&mut self, modified: SystemTime) -> &mut Self {
        self.update.modified = Some(modified);
        self
    }

    pub fn follow_symlinks(&mut self, follow: bool) -> &mut Self {
        self.follow = follow;
        self
    }

    pub fn uid(&mut self, uid: u32) -> &mut Self {
        self.update.uid = Some(uid);
        self
    }
    pub fn gid(&mut self, gid: u32) -> &mut Self {
        self.update.gid = Some(gid);
        self
    }
    pub fn apply(&self) -> VfResult<()> {
        self.client
            .vsetattrs(&[(&self.path, self.update.clone())], self.follow)
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
    pub fn metadata(&self) -> VfResult<Metadata> {
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
            .metadata(MetadataQuery::new(self.raw()?.clone(), attributes))
            .map(vfsi_core::metadata_from_attrs)
            .map_err(|error| error.with_context("metadata", &self.path))
    }

    /// Truncate or extend the open file.
    pub fn truncate(&self, len: u64) -> VfResult<()> {
        self.set_metadata_update(MetadataUpdate::new().len(len))
    }

    /// Change permissions on the open file through the shared vector engine.
    pub fn chmod(&self, permissions: Permissions) -> VfResult<()> {
        self.set_metadata_update(MetadataUpdate::new().permissions(permissions))
    }

    fn set_metadata_update(&self, update: MetadataUpdate) -> VfResult<()> {
        let client = FsClient {
            inner: Arc::clone(&self.inner),
            limits: ResourceLimits::default(),
        };
        client.vsetattrs(&[(vfsi_core::MetadataTarget::File(self), update)], true)
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
            .read_one_into(&request, buffer)
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
            .write_one(WriteOpRef::new(&file, offset, buffer))
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
            .close_one(file)
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
            .seek_one(self.raw()?, position)
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

/// Typed read request for [`FsClient::readv`].
pub struct FsRead<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    length: usize,
}

/// Typed borrowed write request for [`FsClient::writev`].
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
    /// Update paths and open objects with one native attribute vector.
    pub fn vsetattrs<P: vfsi_core::MetadataOperand<FsFile<F>>>(
        &self,
        updates: &[(P, MetadataUpdate)],
        follow_symlinks: bool,
    ) -> VfResult<()> {
        use vfsi_core::MetadataTarget;
        if updates.is_empty() {
            return Ok(());
        }
        let mut paths = Vec::with_capacity(updates.len());
        let mut attrs = Vec::with_capacity(updates.len());
        for (index, (target, update)) in updates.iter().enumerate() {
            let (raw, path) = match target.metadata_target() {
                MetadataTarget::Path(path) => (VfFile::from_os_path(path), path),
                MetadataTarget::File(file) => {
                    if !Arc::ptr_eq(&self.inner, &file.inner) {
                        return Err(VfError::client(index, crate::ERR_INVAL)
                            .with_context("vsetattrs", &file.path));
                    }
                    (
                        file.raw()
                            .map_err(|e| e.with_index(index).with_context("vsetattrs", &file.path))?
                            .clone(),
                        file.path.as_path(),
                    )
                }
            };
            if update.uid == Some(u32::MAX) || update.gid == Some(u32::MAX) {
                return Err(
                    VfError::client(index, crate::ERR_INVAL).with_context("vsetattrs", path)
                );
            }
            paths.push(path);
            let mut attributes = crate::SetAttributes::new(raw);
            attributes.mode = update.permissions.map(crate::Permissions::mode);
            attributes.size = update.len;
            attributes.uid = update.uid;
            attributes.gid = update.gid;
            attributes.atime = update
                .accessed
                .map(crate::native::system_time_parts)
                .transpose()
                .map_err(|e| e.with_index(index).with_context("vsetattrs", path))?;
            attributes.mtime = update
                .modified
                .map(crate::native::system_time_parts)
                .transpose()
                .map_err(|e| e.with_index(index).with_context("vsetattrs", path))?;
            attrs.push(attributes);
        }
        self.lock()?
            .set_attributes_many(attrs, follow_symlinks)
            .map_err(|error| match error.index() {
                Some(index) if index < updates.len() => {
                    error.with_context("vsetattrs", paths[index])
                }
                Some(_) => {
                    VfError::transport(None, "setattrs backend returned an invalid error index")
                }
                None => error,
            })
    }
}

fn link_error(error: VfError, paths: &[&Path], operation: &'static str) -> VfError {
    match error.index() {
        Some(index) if index < paths.len() => error.with_context(operation, paths[index]),
        Some(_) => VfError::transport(None, "link backend returned an invalid error index"),
        None => error,
    }
}

#[cfg(test)]
mod traversal_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The generic listdir_page fallback's cursor owns the full unconsumed
    // listing, not just its next page. Count these retained allocations.
    struct Snapshot {
        remaining: std::vec::IntoIter<DirEntry>,
        live: Arc<AtomicUsize>,
    }
    impl Drop for Snapshot {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }
    fn snapshot_pages(
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    ) -> impl FnMut(&Path, Option<crate::DirPageCursor>, usize, usize) -> VfResult<DirectoryPage>
    {
        move |path, cursor, page_size, max_entries| {
            let mut snapshot = match cursor {
                Some(cursor) => cursor.into_state::<Snapshot>()?,
                None => {
                    let depth = path.components().count();
                    let entries = (0..129)
                        .take(max_entries)
                        .map(|index| {
                            let path = path.join(format!("entry-{index}"));
                            let attrs = crate::VfAttrs {
                                file: VfFile::from_os_path(&path),
                                ftype: if index == 0 && depth < 5 {
                                    crate::VfType::Directory
                                } else {
                                    crate::VfType::Regular
                                },
                                ..Default::default()
                            };
                            DirEntry::new(path, vfsi_core::metadata_from_attrs(attrs))
                        })
                        .collect::<Vec<_>>();
                    peak.fetch_max(live.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    Snapshot {
                        remaining: entries.into_iter(),
                        live: Arc::clone(&live),
                    }
                }
            };
            let entries = snapshot.remaining.by_ref().take(page_size).collect();
            let next = if snapshot.remaining.len() == 0 {
                None
            } else {
                Some(crate::DirPageCursor::new(snapshot))
            };
            Ok((entries, next))
        }
    }
    #[test]
    fn traversal_never_retains_multiple_fallback_snapshots() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut seen = 0;
        let completion = visit_walk_pages(
            Path::new("/tree"),
            crate::WalkOptions::new().max_entries(1000),
            snapshot_pages(Arc::clone(&live), Arc::clone(&peak)),
            |_| {
                seen += 1;
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap();
        assert_eq!(completion, TraversalCompletion::Complete);
        assert_eq!(seen, 4 * 129);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn traversal_drops_fallback_snapshot_on_stop_error_and_limit() {
        for outcome in 0..3 {
            let live = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let result = visit_walk_pages(
                Path::new("/tree"),
                crate::WalkOptions::new().max_entries(if outcome == 2 { 2 } else { 1000 }),
                snapshot_pages(Arc::clone(&live), peak),
                |_| match outcome {
                    0 => Ok(std::ops::ControlFlow::Break(())),
                    1 => Err(VfError::client(0, libc::EIO as u32)),
                    _ => Ok(std::ops::ControlFlow::Continue(())),
                },
            );
            match outcome {
                0 => assert_eq!(result.unwrap(), TraversalCompletion::Stopped),
                1 => assert_eq!(result.unwrap_err().err_no(), libc::EIO as u32),
                _ => assert_eq!(result.unwrap_err().err_no(), libc::EFBIG as u32),
            }
            assert_eq!(live.load(Ordering::SeqCst), 0);
        }
    }
}
