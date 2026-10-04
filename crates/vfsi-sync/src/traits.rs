use super::*;
use vfsi_core::internal::ManyResults;

/// Default payload limit for APIs that allocate and return complete contents.
pub const DEFAULT_READ_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Default size of each chunk delivered by the bounded single-file stream API.
pub const DEFAULT_READ_STREAM_CHUNK_BYTES: usize = 1024 * 1024;

/// Default aggregate payload limit for [`VecFs::read_allv`].
pub const DEFAULT_READ_ALLV_MAX_TOTAL_BYTES: usize = DEFAULT_READ_MAX_BYTES;

/// Tuning options for bounded single-file streaming reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadStreamOptions {
    chunk_size: usize,
}

impl ReadStreamOptions {
    pub const fn new() -> Self {
        Self {
            chunk_size: DEFAULT_READ_STREAM_CHUNK_BYTES,
        }
    }

    /// Set the maximum bytes delivered to the callback at once.
    ///
    /// NFS and other backends may return smaller chunks due to negotiated
    /// protocol limits. A larger setting does not bypass those limits.
    pub const fn chunk_size(mut self, bytes: usize) -> Self {
        self.chunk_size = bytes;
        self
    }

    pub const fn chunk_size_bytes(self) -> usize {
        self.chunk_size
    }
}

impl Default for ReadStreamOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Backend-owned continuation state for a paged directory visit.
///
/// The state is dropped automatically when the caller stops or an error
/// occurs, so backends do not need an explicit cursor-close operation.
#[doc(hidden)]
pub struct DirPageCursor(Box<dyn std::any::Any + Send>);

impl DirPageCursor {
    pub fn new<T: std::any::Any + Send>(state: T) -> Self {
        Self(Box::new(state))
    }

    pub fn is<T: std::any::Any + Send>(&self) -> bool {
        self.0.is::<T>()
    }

    pub fn into_state<T: std::any::Any + Send>(self) -> VfResult<T> {
        self.0
            .downcast::<T>()
            .map(|state| *state)
            .map_err(|_| VfError::client(0, ERR_INVAL))
    }
}

/// Backend page plus optional child seeds aligned with its entries.
/// A seed starts a child's first page relative to its observed parent.
#[doc(hidden)]
pub type BackendDirectoryPage = (
    Vec<VfAttrs>,
    Option<DirPageCursor>,
    Vec<Option<DirPageCursor>>,
);

/// Owned application page and traversal-scoped anchored child seeds.
#[doc(hidden)]
pub type DirectoryPage = (
    crate::DirectoryListing,
    Option<DirPageCursor>,
    Vec<(std::path::PathBuf, DirPageCursor)>,
);

/// Default maximum number of entries returned by allocating directory APIs.
pub const DEFAULT_DIRECTORY_MAX_ENTRIES: usize = 100_000;

/// Default combined path-storage budget for allocating directory APIs.
pub const DEFAULT_DIRECTORY_MAX_PATH_BYTES: usize = 16 * 1024 * 1024;

/// Default recursion depth for [`VecFs::walk`].
pub const DEFAULT_WALK_MAX_DEPTH: usize = 128;

/// Resource limits for one allocating directory listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadDirOptions {
    max_entries: usize,
    max_path_bytes: usize,
}

impl ReadDirOptions {
    pub const fn new() -> Self {
        Self {
            max_entries: DEFAULT_DIRECTORY_MAX_ENTRIES,
            max_path_bytes: DEFAULT_DIRECTORY_MAX_PATH_BYTES,
        }
    }

    /// Explicitly opt out of the default allocation limits.
    pub const fn unlimited() -> Self {
        Self {
            max_entries: usize::MAX,
            max_path_bytes: usize::MAX,
        }
    }

    pub const fn max_entries(mut self, entries: usize) -> Self {
        self.max_entries = entries;
        self
    }

    pub const fn max_path_bytes(mut self, bytes: usize) -> Self {
        self.max_path_bytes = bytes;
        self
    }

    pub const fn entry_limit(self) -> usize {
        self.max_entries
    }

    pub const fn path_byte_limit(self) -> usize {
        self.max_path_bytes
    }
}

impl Default for ReadDirOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Resource limits for an allocating recursive directory walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkOptions {
    directory: ReadDirOptions,
    max_depth: usize,
    truncate_at_max_depth: bool,
}

impl WalkOptions {
    pub const fn new() -> Self {
        Self {
            directory: ReadDirOptions::new(),
            max_depth: DEFAULT_WALK_MAX_DEPTH,
            truncate_at_max_depth: false,
        }
    }

    /// Explicitly opt out of the default allocation and recursion limits.
    pub const fn unlimited() -> Self {
        Self {
            directory: ReadDirOptions::unlimited(),
            max_depth: usize::MAX,
            truncate_at_max_depth: false,
        }
    }

    pub const fn max_entries(mut self, entries: usize) -> Self {
        self.directory = self.directory.max_entries(entries);
        self
    }

    pub const fn max_path_bytes(mut self, bytes: usize) -> Self {
        self.directory = self.directory.max_path_bytes(bytes);
        self
    }

    pub const fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Stop descending at `max_depth` instead of treating a deeper subtree
    /// as a safety-limit violation. Intended for caller-requested shallow walks.
    pub const fn truncate_at_max_depth(mut self, truncate: bool) -> Self {
        self.truncate_at_max_depth = truncate;
        self
    }

    pub const fn entry_limit(self) -> usize {
        self.directory.entry_limit()
    }

    pub const fn path_byte_limit(self) -> usize {
        self.directory.path_byte_limit()
    }

    pub const fn depth_limit(self) -> usize {
        self.max_depth
    }

    pub const fn truncates_at_depth_limit(self) -> bool {
        self.truncate_at_max_depth
    }
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Resource limits for reading multiple complete files into memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadAllOptions {
    max_total_bytes: usize,
}

impl ReadAllOptions {
    pub const fn new() -> Self {
        Self {
            max_total_bytes: DEFAULT_READ_ALLV_MAX_TOTAL_BYTES,
        }
    }

    /// Set the maximum combined size of all returned buffers.
    pub const fn max_total_bytes(mut self, bytes: usize) -> Self {
        self.max_total_bytes = bytes;
        self
    }

    pub const fn total_byte_limit(self) -> usize {
        self.max_total_bytes
    }
}

impl Default for ReadAllOptions {
    fn default() -> Self {
        Self::new()
    }
}

fn contract_error(
    operation: &str,
    index: Option<usize>,
    detail: impl std::fmt::Display,
) -> VfError {
    VfError::transport(
        index,
        format!("{operation} backend contract violation: {detail}"),
    )
}

fn take_single_result<T>(operation: &str, mut results: Vec<T>) -> VfResult<T> {
    if results.len() != 1 {
        return Err(contract_error(
            operation,
            None,
            format!("returned {} results for one request", results.len()),
        ));
    }
    Ok(results.pop().expect("validated one result"))
}

pub(crate) fn validate_read_results(
    operation: &str,
    requests: &[ReadOp],
    results: &[ReadResult],
) -> VfResult<()> {
    if results.len() != requests.len() {
        return Err(contract_error(
            operation,
            None,
            format!(
                "returned {} results for {} requests",
                results.len(),
                requests.len()
            ),
        ));
    }
    for (index, (request, result)) in requests.iter().zip(results).enumerate() {
        if result.file != request.file {
            return Err(contract_error(
                operation,
                Some(index),
                "result file does not match request",
            ));
        }
        if result.data.len() > request.length {
            return Err(contract_error(
                operation,
                Some(index),
                format!(
                    "returned {} bytes for a {}-byte read",
                    result.data.len(),
                    request.length
                ),
            ));
        }
        if let VfOffset::At(expected) = request.offset
            && result.offset != expected
        {
            return Err(contract_error(
                operation,
                Some(index),
                format!("result offset {} does not match {expected}", result.offset),
            ));
        }
        if request.length != 0 && result.data.is_empty() && !result.eof {
            return Err(contract_error(
                operation,
                Some(index),
                "read made no progress without reporting EOF",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_read_into_results(
    operation: &str,
    requests: &[ReadOp],
    results: &[ReadIntoResult],
) -> VfResult<()> {
    if results.len() != requests.len() {
        return Err(contract_error(
            operation,
            None,
            format!(
                "returned {} results for {} requests",
                results.len(),
                requests.len()
            ),
        ));
    }
    for (index, (request, result)) in requests.iter().zip(results).enumerate() {
        if result.file != request.file {
            return Err(contract_error(
                operation,
                Some(index),
                "result file does not match request",
            ));
        }
        if result.read > request.length {
            return Err(contract_error(
                operation,
                Some(index),
                format!(
                    "returned {} bytes for a {}-byte read",
                    result.read, request.length
                ),
            ));
        }
        if let VfOffset::At(expected) = request.offset
            && result.offset != expected
        {
            return Err(contract_error(
                operation,
                Some(index),
                format!("result offset {} does not match {expected}", result.offset),
            ));
        }
        if request.length != 0 && result.read == 0 && !result.eof {
            return Err(contract_error(
                operation,
                Some(index),
                "read made no progress without reporting EOF",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_write_results(
    operation: &str,
    requests: &[WriteOpRef<'_>],
    results: &[WriteResult],
) -> VfResult<()> {
    if results.len() != requests.len() {
        return Err(contract_error(
            operation,
            None,
            format!(
                "returned {} results for {} requests",
                results.len(),
                requests.len()
            ),
        ));
    }
    for (index, (request, result)) in requests.iter().zip(results).enumerate() {
        if result.file != *request.file {
            return Err(contract_error(
                operation,
                Some(index),
                "result file does not match request",
            ));
        }
        if result.written > request.data.len() {
            return Err(contract_error(
                operation,
                Some(index),
                format!(
                    "reported {} bytes written for a {}-byte write",
                    result.written,
                    request.data.len()
                ),
            ));
        }
        if let VfOffset::At(expected) = request.offset
            && result.offset != expected
        {
            return Err(contract_error(
                operation,
                Some(index),
                format!("result offset {} does not match {expected}", result.offset),
            ));
        }
    }
    Ok(())
}

/// A vectorized filesystem: many small operations coalesced into as few
/// round trips as the backend supports.
///
/// `VfFile` references files either by descriptor (an open file) or by path
/// (absolute, or relative to the client's current working directory).
///
/// # Partial success
///
/// Vector calls are ordered batches, not transactions. On an indexed error,
/// operations before the failing index may already have completed and later
/// operations may not have been attempted. In particular, a transport error
/// after a mutating request was sent can leave its outcome unknown. Backends
/// must not silently replay such mutations; callers that retry must first
/// reconcile state or use application-level idempotency.
pub trait VecFs {
    /// Finish cleanup after an owned file is dropped. Backends that retain
    /// failed CLOSE state internally should complete their deferred cleanup.
    fn close_deferred(&mut self, file: &VfFile) -> VfResult<()> {
        self.close(file)
    }
    /// Deferred operational notifications. Owned clients deliver these after
    /// unlocking; direct backend users must drain and run them outside any lock.
    fn take_notifications(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        Vec::new()
    }
    /// Negotiated NFS minor version, or `None` for non-NFS backends.
    #[deprecated(note = "use the NFS backend's NfsExtensions trait")]
    fn nfs_minorversion(&self) -> Option<u32> {
        None
    }

    /// Negotiated SMB dialect revision (for example `0x0311` for SMB 3.1.1),
    /// or `None` for non-SMB backends.
    #[deprecated(note = "use the SMB backend's SmbExtensions trait")]
    fn smb_dialect(&self) -> Option<u16> {
        None
    }

    /// Backend capability bits. Capabilities may change after a server
    /// rejects an optional operation and the client installs a fallback.
    fn capabilities(&self) -> u64 {
        0
    }

    /// Strongly typed capability query used by the Rust-native API.
    fn typed_capabilities(&self) -> Capabilities {
        Capabilities::from_bits_retain(self.capabilities())
    }

    // -- required -----------------------------------------------------------

    /// Return the root-relative form of `path` (resolving it against the
    /// client's current working directory if it is relative), without a
    /// leading `/`. "Root" is the backend's application namespace root (the
    /// export root for NFS, the configured root for the local backend), not a
    /// server or host path; for NFS the export prefix is deliberately absent.
    /// [`getcwd`](Self::getcwd) is the display form (with a leading `/`);
    /// [`vf_path`](Self::vf_path) resolves a [`VfFile`] the same way while
    /// honoring its [`VfPathBase`].
    fn abs_path(&self, path: &Path) -> PathBuf;

    /// Open a file by path, similar to `tc_open_by_path(2)`. `base` is
    /// `VfPathBase::Cwd` or `VfPathBase::Abs`. When `O_CREAT` is set, `mode`
    /// is applied to the new file.
    fn open_by_path(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile>;

    /// Close an open file, `tc_close()`.
    fn close(&mut self, tcf: &VfFile) -> VfResult<()>;

    /// Make prior writes to this descriptor durable. Backends whose writes
    /// are always stable still validate the handle before returning success.
    fn sync_data(&mut self, tcf: &VfFile) -> VfResult<()>;

    /// Make file data and metadata durable.
    fn sync_all(&mut self, tcf: &VfFile) -> VfResult<()> {
        self.sync_data(tcf)
    }

    /// Change the client's current directory, `tc_chdir()`.
    fn chdir(&mut self, path: &Path) -> VfResult<()>;

    /// Current working directory, `tc_getcwd()`.
    fn getcwd(&self) -> PathBuf;

    /// Read from one or more files, `tc_readv()`. Returns one result per
    /// request, or fails at the first failing operation.
    fn readv(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>>;

    /// Read into caller-owned buffers without requiring an owned result for
    /// backends that can decode or read directly into those buffers. The
    /// default compatibility implementation copies from [`readv`](Self::readv).
    fn readv_into(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        if reads.len() != buffers.len() {
            return Err(VfError::client(0, ERR_INVAL));
        }
        for (index, (request, buffer)) in reads.iter().zip(buffers.iter()).enumerate() {
            if request.length != buffer.len() {
                return Err(VfError::client(index, ERR_INVAL));
            }
        }
        let results = self.readv(reads)?;
        validate_read_results("readv_into", reads, &results)?;
        Ok(results
            .into_iter()
            .zip(buffers.iter_mut())
            .map(|(result, buffer)| {
                buffer[..result.data.len()].copy_from_slice(&result.data);
                ReadIntoResult {
                    file: result.file,
                    offset: result.offset,
                    read: result.data.len(),
                    eof: result.eof,
                }
            })
            .collect())
    }

    /// Read each file in full from offset 0, `tc_read_allv()`. Returns one
    /// byte buffer per request in input order. The combined result is limited
    /// to [`DEFAULT_READ_ALLV_MAX_TOTAL_BYTES`].
    fn read_allv(&mut self, files: &[VfFile]) -> VfResult<Vec<Vec<u8>>> {
        self.read_allv_with_options(files, ReadAllOptions::default())
    }

    /// Read complete files with a caller-selected aggregate memory limit.
    fn read_allv_with_options(
        &mut self,
        files: &[VfFile],
        options: ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        let mut out: Vec<Vec<u8>> = files.iter().map(|_| Vec::new()).collect();
        let mut total = 0usize;
        let mut limit_error = None;
        let stream_budget = options
            .total_byte_limit()
            .clamp(1, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
        let chunk_size = stream_budget.min(1024 * 1024);
        self.read_streamv(
            files,
            chunk_size,
            stream_budget,
            &mut |index, _, data, _| {
                let Some(next_total) = total.checked_add(data.len()) else {
                    limit_error = Some(index);
                    return false;
                };
                if next_total > options.total_byte_limit() {
                    limit_error = Some(index);
                    return false;
                }
                out[index].extend_from_slice(data);
                total = next_total;
                true
            },
        )?;
        if let Some(index) = limit_error {
            return Err(VfError::failure(index, libc::EFBIG as u32));
        }
        Ok(out)
    }

    /// Write to one or more files, `tc_writev()`. Returns one result per
    /// request, or fails at the first failing operation.
    fn writev(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>>;

    /// Allocation-free input facade. Compatibility backends may copy before
    /// dispatch; native backends should override this method to retain the
    /// borrowed buffers through request encoding.
    fn writev_borrowed(&mut self, writes: &[WriteOpRef<'_>]) -> VfResult<Vec<WriteResult>> {
        let owned: Vec<WriteOp> = writes
            .iter()
            .map(|write| WriteOp {
                file: write.file.clone(),
                offset: write.offset,
                data: write.data.to_vec(),
                creation: write.creation,
                truncate: write.truncate,
            })
            .collect();
        self.writev(&owned)
    }

    /// Reposition the read/write offset of an open file, `tc_fseek()`. The
    /// offset lives in backend state keyed by the descriptor; `tcf` is not
    /// modified (hence `&VfFile`). Returns the new offset. Note that
    /// [`SeekFrom::End`] needs the file size, which is an extra round trip on
    /// backends that do not cache it.
    fn fseek(&mut self, tcf: &VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64>;

    /// Get attributes of an array of files, `tc_getattrsv()`. Follows
    /// symlinks to the target.
    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes;

    /// Like [`getattrsv`](Self::getattrsv) but does not follow symlinks:
    /// attributes are for the symlink itself, `tc_lgetattrsv()`.
    fn lgetattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes;

    /// Set attributes on an array of files, `tc_setattrsv()`. Only
    /// [`AttrMask::MODE`], [`AttrMask::SIZE`], [`AttrMask::ATIME`], and
    /// [`AttrMask::MTIME`] are part of the portable contract; requesting any
    /// other bit fails with [`VF_ERR_UNSUPPORTED`] at that index, and an empty
    /// mask is a no-op. Follows symlinks to the target.
    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes;

    /// Like [`setattrsv`](Self::setattrsv) but does not follow symlinks:
    /// attributes are set on the symlink itself, `tc_lsetattrsv()`. Backends
    /// without a non-following setter (e.g. no `lchmod` on Linux) must fail
    /// with [`VF_ERR_UNSUPPORTED`] for symlinks rather than silently follow.
    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes;

    /// List a directory, `tc_listdir()`. Returns entry paths and attributes.
    ///
    /// `max_count` limits the number of entries returned per directory (0
    /// means no limit). Entry `VfFile`s are absolute (root-relative) paths.
    /// With `recursive`, entries of nested directories are included depth-
    /// first (still subject to `max_count`).
    fn listdir(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>>;

    /// Fetch at most `page_size` entries using backend-owned continuation
    /// state. `max_entries` bounds a generic backend's one-time snapshot;
    /// zero means the caller explicitly requested an unlimited listing.
    ///
    /// Backends with native directory iterators or cookies should override
    /// this default. Dropping the cursor ends enumeration without retaining
    /// backend state.
    fn listdir_page(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        if page_size == 0 {
            return Err(VfError::client(0, ERR_INVAL));
        }
        let mut remaining = match cursor {
            Some(cursor) => cursor.into_state::<std::vec::IntoIter<VfAttrs>>()?,
            None => self.listdir(dir, masks, max_entries, false)?.into_iter(),
        };
        let page: Vec<_> = remaining.by_ref().take(page_size).collect();
        let next = if remaining.len() == 0 {
            None
        } else {
            Some(DirPageCursor::new(remaining))
        };
        Ok((page, next))
    }

    /// Safe cohort size: fallback snapshots must not accumulate across directories.
    fn directory_page_batch_size(&self) -> usize {
        1
    }

    /// Fetch one bounded page per directory in request order.
    fn listdir_pages(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        cursors: Vec<Option<DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<Vec<BackendDirectoryPage>> {
        if dirs.len() != cursors.len() || page_size == 0 {
            return Err(VfError::client(0, ERR_INVAL));
        }
        dirs.iter()
            .zip(cursors)
            .enumerate()
            .map(|(index, (dir, cursor))| {
                self.listdir_page(dir, masks, cursor, page_size, max_entries)
                    .map(|(entries, next)| {
                        let children = (0..entries.len()).map(|_| None).collect();
                        (entries, next, children)
                    })
                    .map_err(|error| error.with_index(index))
            })
            .collect()
    }

    /// Recursively enumerate `root`, returning each directory with its entries.
    ///
    /// `sort` orders a directory's entries the way the caller's presentation
    /// layer would (so subdirectories are visited in the same order the caller
    /// lists them). The default implementation recurses via
    /// [`listdir`](Self::listdir); a backend may override it to batch many
    /// directories into few large compounds.
    fn walk(
        &mut self,
        root: &Path,
        masks: AttrMask,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        self.walk_with_options(root, masks, WalkOptions::default(), sort)
    }

    /// Recursively enumerate `root` with explicit allocation and depth limits.
    fn walk_with_options(
        &mut self,
        root: &Path,
        masks: AttrMask,
        options: WalkOptions,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        // Explicit stack (pre-order, subdirectories visited in the order the
        // sort callback produced) so deep trees cannot overflow the call
        // stack.
        let mut out = Vec::new();
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        let mut entry_count = 0usize;
        let mut path_bytes = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            let remaining = options.entry_limit().saturating_sub(entry_count);
            let request_count = remaining.saturating_add(1);
            let mut entries = self.listdir(&dir, masks, request_count, false)?;
            if entries.len() > remaining {
                return Err(
                    VfError::failure(entry_count, libc::EFBIG as u32).with_context("walk", &dir)
                );
            }
            for entry in &entries {
                let bytes = entry.file.path().map_or(0, |path| path.as_os_str().len());
                path_bytes = path_bytes.checked_add(bytes).ok_or_else(|| {
                    VfError::failure(entry_count, libc::EFBIG as u32).with_context("walk", &dir)
                })?;
                if path_bytes > options.path_byte_limit() {
                    return Err(VfError::failure(entry_count, libc::EFBIG as u32)
                        .with_context("walk", &dir));
                }
                entry_count += 1;
            }
            sort(dir.as_path(), &mut entries);
            let subdirs: Vec<PathBuf> = entries
                .iter()
                .filter(|e| e.ftype == VfType::Directory)
                .filter_map(|e| e.file.path().map(|p| p.to_path_buf()))
                .collect();
            if !subdirs.is_empty()
                && depth >= options.depth_limit()
                && !options.truncates_at_depth_limit()
            {
                return Err(
                    VfError::failure(entry_count, libc::EFBIG as u32).with_context("walk", &dir)
                );
            }
            if depth < options.depth_limit() {
                for s in subdirs.into_iter().rev() {
                    stack.push((s, depth + 1));
                }
            }
            out.push(WalkEntry { path: dir, entries });
        }
        Ok(out)
    }

    /// Rename a list of file pairs, `tc_renamev()`.
    fn renamev(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes;

    /// Remove a list of files (or empty directories), `tc_removev()`.
    fn removev(&mut self, files: &[VfFile]) -> VfRes;

    /// Create one or more directories, `tc_mkdirv()`.
    fn mkdirv(&mut self, dirs: &[VfAttrs]) -> VfRes;

    /// Create a list of symlinks, `tc_symlinkv()`.
    fn symlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes;

    /// Read symlink targets, `tc_readlinkv()`.
    fn readlinkv(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>>;

    /// Create hard links, `tc_hardlinkv()`.
    fn hardlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes;

    /// Copy extents by reading and writing, `tc_dupv()`. Follows symlinks
    /// (copies the target's contents).
    fn dupv(&mut self, pairs: &[ExtentPair]) -> VfRes;

    /// Copy extents without following symlinks, `tc_lcopyv()`: symlinks are
    /// recreated as symlinks with the same target; other objects are copied
    /// by data (like [`dupv`](Self::dupv)).
    fn lcopyv(&mut self, pairs: &[ExtentPair]) -> VfRes;

    /// Write Application Data Blocks, `tc_write_adb()`. Returns the number
    /// of blocks written for each ADB.
    fn write_adb(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>>;

    /// Read files in bounded chunks while retaining vectorized I/O.
    ///
    /// At most `memory_limit` bytes are requested in one batch and no single
    /// request exceeds `chunk_size`. `cb` receives the original file index,
    /// absolute offset, data, and EOF state. Returning `false` cancels the
    /// stream successfully, providing backpressure without buffering whole
    /// files. The callback runs while `self` is borrowed and must not reenter
    /// this filesystem instance.
    fn read_streamv(
        &mut self,
        files: &[VfFile],
        chunk_size: usize,
        memory_limit: usize,
        cb: &mut ReadStreamCallback<'_>,
    ) -> VfRes {
        use std::collections::VecDeque;

        if chunk_size == 0 || memory_limit == 0 {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let mut pending: VecDeque<usize> = (0..files.len()).collect();
        let mut offsets = vec![0u64; files.len()];
        while !pending.is_empty() {
            let mut budget = memory_limit;
            let mut batch_indices = Vec::new();
            let mut reads = Vec::new();
            while budget > 0 && !pending.is_empty() {
                let index = pending.pop_front().expect("pending was non-empty");
                let length = chunk_size.min(budget);
                reads.push(ReadOp::at(files[index].clone(), offsets[index], length));
                batch_indices.push(index);
                budget -= length;
            }
            let results = self.readv(&reads).map_err(|error| {
                error.index().map_or(error.clone(), |local_index| {
                    batch_indices
                        .get(local_index)
                        .copied()
                        .map_or(error.clone(), |index| error.with_index(index))
                })
            })?;
            validate_read_results("read_streamv", &reads, &results).map_err(|error| {
                error.index().map_or(error.clone(), |local_index| {
                    batch_indices
                        .get(local_index)
                        .copied()
                        .map_or(error.clone(), |index| error.with_index(index))
                })
            })?;
            for (batch_index, result) in results.into_iter().enumerate() {
                let index = batch_indices[batch_index];
                let offset = offsets[index];
                let eof = result.eof;
                offsets[index] = offset
                    .checked_add(result.data.len() as u64)
                    .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))?;
                if !cb(index, offset, &result.data, eof) {
                    return Ok(());
                }
                if !eof {
                    pending.push_back(index);
                }
            }
        }
        Ok(())
    }

    /// Remove a list of objects, recursively when `recursive`, `tc_rm()`.
    ///
    /// This is the fail-fast spelling of
    /// [`rm_with_options`](Self::rm_with_options); callers wanting best-effort
    /// semantics should use that.
    ///
    /// # Path entry-point race
    ///
    /// `objs` are paths. Between the caller naming one and the backend starting
    /// work, a concurrent actor may replace a path component with a symbolic
    /// link, causing an unexpected tree to be removed (the classic entry-point
    /// TOCTOU, RUSTSEC-2023-0018). Prefer the handle-rooted
    /// [`open_dir`](Self::open_dir) + [`rm_dir_contents`](Self::rm_dir_contents)
    /// for privileged or attacker-influenced paths.
    ///
    /// The returned error index is always the operand index in `objs`.
    fn rm(&mut self, objs: &[&Path], recursive: bool) -> VfRes {
        self.rm_with_options(objs, recursive, RemoveOptions::default())
    }

    /// Recursive removal with explicit [`RemoveOptions`].
    ///
    /// The generic implementation is fail-fast and uses `objs` as paths;
    /// backends with a native remover (NFS) override this to honor the options,
    /// address directories by handle, and batch protocol operations.
    /// Generic recursive removal requires no-follow metadata (`LSTAT`); a
    /// backend that cannot provide it returns `unsupported` rather than risk
    /// traversing a symbolic link. Non-recursive removal does not require it.
    fn rm_with_options(
        &mut self,
        objs: &[&Path],
        recursive: bool,
        options: RemoveOptions,
    ) -> VfRes {
        if options != RemoveOptions::default() {
            return Err(VfError::unsupported(0));
        }
        if objs.is_empty() {
            return Ok(());
        }
        if !recursive {
            let files: Vec<VfFile> = objs.iter().map(|p| VfFile::from_os_path(p)).collect();
            return self.removev(&files);
        }

        for (root_index, root) in objs.iter().enumerate() {
            self.before_remove_type(root_index)?;
            // Classify the root (no-follow); a non-directory is just removed.
            let mut root_attrs = VfAttrs {
                file: VfFile::from_os_path(root),
                masks: AttrMask::default(),
                ..VfAttrs::default()
            };
            self.lgetattrsv(std::slice::from_mut(&mut root_attrs))
                .map_err(|error| error.map_index(|_| root_index))?;
            if root_attrs.ftype != VfType::Directory {
                self.removev(&[VfFile::from_os_path(root)])
                    .map_err(|error| error.map_index(|_| root_index))?;
                continue;
            }

            // `dir_levels[k]` holds the directories at depth `k`; they are
            // removed in reverse once deeper levels are gone.
            let mut dir_levels: Vec<Vec<PathBuf>> = Vec::new();
            let mut frontier: Vec<PathBuf> = vec![root.to_path_buf()];
            while !frontier.is_empty() {
                let refs: Vec<&Path> = frontier.iter().map(PathBuf::as_path).collect();
                let mut per_dir: Vec<Vec<VfAttrs>> = (0..refs.len()).map(|_| Vec::new()).collect();
                {
                    let index_of: std::collections::HashMap<&Path, usize> =
                        refs.iter().enumerate().map(|(i, p)| (*p, i)).collect();
                    let mut cb = |attrs: &VfAttrs, dir: &Path| {
                        if let Some(&slot) = index_of.get(dir) {
                            per_dir[slot].push(attrs.clone());
                        }
                        true
                    };
                    self.listdirv(&refs, AttrMask::default(), 0, false, &mut cb)
                        .map_err(|error| error.map_index(|_| root_index))?;
                }

                let mut next: Vec<PathBuf> = Vec::new();
                for entries in per_dir {
                    let mut files: Vec<VfFile> = Vec::new();
                    for attrs in entries {
                        if attrs.ftype == VfType::Directory {
                            if let Some(path) = attrs.file.path() {
                                next.push(path.to_path_buf());
                            }
                        } else {
                            files.push(attrs.file);
                        }
                    }
                    if !files.is_empty() {
                        self.removev(&files)
                            .map_err(|error| error.map_index(|_| root_index))?;
                    }
                }
                dir_levels.push(frontier);
                frontier = next;
            }

            for level in dir_levels.iter().rev() {
                let dirs: Vec<VfFile> = level
                    .iter()
                    .map(|path| VfFile::from_os_path(path))
                    .collect();
                self.removev(&dirs)
                    .map_err(|error| error.map_index(|_| root_index))?;
            }
        }
        Ok(())
    }

    // -- directory handles --------------------------------------------------

    /// Open a directory and return a handle suitable for
    /// [`rm_dir_contents`](Self::rm_dir_contents).
    ///
    /// The directory is opened without following a final symbolic link, so a
    /// symlink to a directory is rejected. Backends with native directory
    /// handles (NFS) return [`VfDir::Descriptor`]; the default verifies the
    /// type and returns [`VfDir::Path`], which callers must treat as no more
    /// race-safe than the path API. Release a handle with
    /// [`close_dir`](Self::close_dir).
    fn open_dir(&mut self, path: &Path) -> VfResult<VfDir> {
        let mut attrs = VfAttrs {
            file: VfFile::from_os_path(path),
            masks: AttrMask::default(),
            ..VfAttrs::default()
        };
        self.lgetattrsv(std::slice::from_mut(&mut attrs))?;
        if attrs.ftype != VfType::Directory {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        Ok(VfDir::Path(path.to_path_buf()))
    }

    /// Remove the contents of the directory referenced by `dir`, keeping the
    /// directory itself. Because `dir` is a handle, the tree root cannot be
    /// swapped for a symlink between opening and removal.
    fn rm_dir_contents(&mut self, dir: &VfDir) -> VfRes {
        self.rm_dir_contents_with_options(dir, RemoveOptions::default())
    }

    /// [`rm_dir_contents`](Self::rm_dir_contents) with explicit options.
    fn rm_dir_contents_with_options(&mut self, dir: &VfDir, options: RemoveOptions) -> VfRes {
        match dir {
            VfDir::Path(path) => self.rm_contents_with_options(path, options),
            _ => Err(VfError::unsupported(0)),
        }
    }

    /// Release a handle obtained from [`open_dir`](Self::open_dir).
    fn close_dir(&mut self, _dir: &VfDir) -> VfResult<()> {
        Ok(())
    }

    /// Remove everything inside `dir`, keeping `dir` itself.
    ///
    /// This takes a path and is therefore subject to the entry-point race
    /// described on [`rm`](Self::rm); prefer
    /// [`open_dir`](Self::open_dir) + [`rm_dir_contents`](Self::rm_dir_contents).
    fn rm_contents(&mut self, dir: &Path) -> VfRes {
        self.rm_contents_with_options(dir, RemoveOptions::default())
    }

    /// [`rm_contents`](Self::rm_contents) with explicit options.
    fn rm_contents_with_options(&mut self, dir: &Path, options: RemoveOptions) -> VfRes {
        if options != RemoveOptions::default() {
            return Err(VfError::unsupported(0));
        }
        let entries = self
            .listdir(dir, AttrMask::default(), 0, false)
            .map_err(|error| error.with_index(0))?;
        let mut files: Vec<VfFile> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for attrs in entries {
            if attrs.ftype == VfType::Directory {
                if let Some(path) = attrs.file.path() {
                    dirs.push(path.to_path_buf());
                }
            } else {
                files.push(attrs.file);
            }
        }
        if !files.is_empty() {
            self.removev(&files).map_err(|error| error.with_index(0))?;
        }
        for sub in dirs {
            self.rm(&[sub.as_path()], true)
                .map_err(|error| error.with_index(0))?;
        }
        Ok(())
    }

    /// Make `dir` an empty directory: create it if missing, otherwise empty it.
    ///
    /// Errors if `dir` exists and is not a directory (including a symlink to a
    /// directory). Subject to the same entry-point race as
    /// [`rm_contents`](Self::rm_contents).
    fn ensure_empty_dir(&mut self, dir: &Path) -> VfRes {
        match self.mkdir(dir, 0o777) {
            Ok(()) => Ok(()),
            Err(error) if error.err_no() == ERR_EXIST => {
                let mut attrs = VfAttrs {
                    file: VfFile::from_os_path(dir),
                    masks: AttrMask::default(),
                    ..VfAttrs::default()
                };
                self.lgetattrsv(std::slice::from_mut(&mut attrs))?;
                if attrs.ftype != VfType::Directory {
                    return Err(VfError::failure(0, ERR_NOTDIR));
                }
                self.rm_contents(dir)
            }
            Err(error) => Err(error),
        }
    }

    /// Recursively copy a directory tree, `tc_cp_recursive()`.
    fn cp_recursive(
        &mut self,
        src_dir: &Path,
        dst: &Path,
        symlinks: bool,
        use_server_side_copy: bool,
    ) -> VfRes;

    // -- defaults -----------------------------------------------------------

    /// Resolve a [`VfFile`] to its root-relative path (no leading `/`),
    /// honoring `VfPathBase::Abs`/`VfPathBase::Cwd` and the client's current
    /// working directory. Descriptors, `Null`, and `Saved` are not paths and
    /// fail with [`ERR_INVAL`]. Backends should use this when a method needs
    /// the file's path, so a `VfFile` resolves identically across all trait
    /// methods.
    fn vf_path(&self, file: &VfFile) -> VfResult<PathBuf> {
        match file {
            VfFile::Path {
                base: VfPathBase::Abs,
                path,
            } => Ok(self.abs_path(&Path::new("/").join(path))),
            VfFile::Path {
                base: VfPathBase::Cwd,
                path,
            }
            | VfFile::CwdPath(path) => Ok(self.abs_path(path)),
            VfFile::Cwd => Ok(self.abs_path(Path::new(""))),
            VfFile::Descriptor(_) | VfFile::Saved => Err(VfError::failure(0, ERR_INVAL)),
            _ => Err(VfError::failure(0, ERR_INVAL)),
        }
    }

    /// Open a file by path, `tc_open()`.
    fn open(&mut self, pathname: &Path, flags: i32, mode: u32) -> VfResult<VfFile> {
        self.open_by_path(VfPathBase::Cwd, pathname, flags, mode)
    }

    /// Read from a single file at an absolute offset, `tc_read()`.
    fn read(&mut self, file: &VfFile, offset: u64, length: usize) -> VfResult<Vec<u8>> {
        let requests = [ReadOp::at(file.clone(), offset, length)];
        let mut results = self.readv(&requests)?;
        validate_read_results("read", &requests, &results)?;
        Ok(results.pop().expect("validated one result").data)
    }

    /// Write to a single file at an absolute offset, `tc_write()`.
    fn write(&mut self, file: &VfFile, offset: u64, data: &[u8]) -> VfResult<usize> {
        let owned = WriteOp::at(file.clone(), offset, data.to_vec());
        let requests = [WriteOpRef {
            file: &owned.file,
            offset: owned.offset,
            data: &owned.data,
            creation: owned.creation,
            truncate: owned.truncate,
        }];
        let mut results = self.writev(std::slice::from_ref(&owned))?;
        validate_write_results("write", &requests, &results)?;
        Ok(results.pop().expect("validated one result").written)
    }

    /// Backend implementation seam for ordered opens.
    ///
    /// This is not an application-facing partial-outcome API. Entry `n`
    /// corresponds to request `n`; an ordered backend may stop after adding
    /// the first semantic failure.
    #[doc(hidden)]
    fn open_many(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let mut results = Vec::with_capacity(paths.len());
        for ((path, flag), mode) in paths.iter().zip(flags).zip(modes) {
            match self.open(path, *flag, *mode) {
                Ok(file) => results.push(Ok(file)),
                Err(error) => {
                    results.push(Err(error));
                    break;
                }
            }
        }
        Ok(ManyResults::new(paths.len(), results))
    }

    /// Test seam invoked immediately before a successful handle is cleaned
    /// after a strict `openv` failure.
    #[doc(hidden)]
    fn before_open_cleanup(&mut self, _index: usize, _file: &VfFile) -> VfResult<()> {
        Ok(())
    }

    /// Test seam invoked before recursive removal classifies each operand.
    #[doc(hidden)]
    fn before_remove_type(&mut self, _index: usize) -> VfResult<()> {
        Ok(())
    }

    /// Open several files at once, each with its own flags and mode,
    /// `tc_openv()`. `flags`, `modes`, and `paths` must have equal lengths;
    /// a mismatch fails with [`ERR_INVAL`] at index 0.
    fn openv(&mut self, paths: &[&Path], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
        let results = self.open_many(paths, flags, modes)?;
        results.try_collect_with_cleanup(paths.len(), |index, file| {
            let injected = self.before_open_cleanup(index, file);
            let closed = self.close(file);
            injected.and(closed)
        })
    }

    /// Open several files at once with a shared flags and mode,
    /// `tc_openv_simple()`.
    fn openv_simple(&mut self, paths: &[&Path], flags: i32, mode: u32) -> VfResult<Vec<VfFile>> {
        let flags_v = vec![flags; paths.len()];
        let modes_v = vec![mode; paths.len()];
        self.openv(paths, &flags_v, &modes_v)
    }

    /// Close several files, `tc_closev()`.
    fn closev(&mut self, files: &[VfFile]) -> VfRes {
        for (i, f) in files.iter().enumerate() {
            self.close(f).map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    /// Stat a path, `tc_stat()`. Follows symlinks to the target.
    fn stat(&mut self, path: &Path) -> VfResult<VfAttrs> {
        let mut a = VfAttrs {
            file: VfFile::from_os_path(path),
            masks: AttrMask::stat(),
            ..VfAttrs::default()
        };
        self.getattrsv(std::slice::from_mut(&mut a))?;
        Ok(a)
    }

    /// `tc_lstat()`: like [`stat`](Self::stat) but does not follow symlinks.
    fn lstat(&mut self, path: &Path) -> VfResult<VfAttrs> {
        let mut a = VfAttrs {
            file: VfFile::from_os_path(path),
            masks: AttrMask::stat(),
            ..VfAttrs::default()
        };
        self.lgetattrsv(std::slice::from_mut(&mut a))?;
        Ok(a)
    }

    /// `tc_fstat()`.
    fn fstat(&mut self, tcf: &VfFile) -> VfResult<VfAttrs> {
        let mut a = VfAttrs {
            file: tcf.clone(),
            masks: AttrMask::stat(),
            ..VfAttrs::default()
        };
        self.getattrsv(std::slice::from_mut(&mut a))?;
        Ok(a)
    }

    /// Whether `path` exists, distinguishing "not found" from other errors
    /// (e.g. permission denied) that are returned as `Err`. Uses
    /// `lstat` semantics: a dangling symlink exists.
    fn exists(&mut self, path: &Path) -> VfResult<bool> {
        match self.lstat(path) {
            Ok(_) => Ok(true),
            Err(e) if e.err_no() == ERR_NOENT => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Return the file type of `path` itself (`lstat` semantics: a symlink
    /// reports [`VfType::Symlink`] rather than its target's type).
    fn file_type(&mut self, path: &Path) -> VfResult<VfType> {
        Ok(self.lstat(path)?.ftype)
    }

    /// List directories with a callback, `tc_listdirv()`. Returning `false`
    /// stops the listing early. Callbacks for different directories may
    /// interleave, and an error can leave partial entries from any directory;
    /// callers must not treat an indexed failure as proof that earlier
    /// directory callbacks were complete.
    fn listdirv(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> VfRes {
        let mut count = 0usize;
        for (i, d) in dirs.iter().enumerate() {
            if max_entries != 0 && count >= max_entries {
                break;
            }
            let remaining = if max_entries == 0 {
                0
            } else {
                max_entries - count
            };
            if !recursive {
                let mut stopped = false;
                self.visit_dir(d, masks, remaining, &mut |entry| {
                    count += 1;
                    if !cb(entry, d) {
                        stopped = true;
                        return false;
                    }
                    true
                })
                .map_err(|e| e.with_index(i))?;
                if stopped {
                    return Ok(());
                }
                continue;
            }
            let entries = self
                .listdir(d, masks, remaining, true)
                .map_err(|e| e.with_index(i))?;
            for e in &entries {
                /* A recursive list contains descendants too, so report each
                 * entry's actual parent rather than attributing every row to the
                 * original root. Backend overrides follow the same contract. */
                let entry_dir = if recursive {
                    e.file.path().and_then(Path::parent).unwrap_or(d)
                } else {
                    d
                };
                if !cb(e, entry_dir) {
                    return Ok(());
                }
                count += 1;
            }
        }
        Ok(())
    }

    /// Visit one directory without requiring the application to retain its
    /// complete listing. Backends may override this to fetch entries page by
    /// page; the compatibility fallback materializes a single directory with
    /// [`listdir`](Self::listdir). Backends that need a strict peak-memory bound
    /// should override this method with a streaming implementation.
    /// The callback runs while the backend is borrowed and must not reenter it.
    fn visit_dir(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_entries: usize,
        cb: &mut dyn FnMut(&VfAttrs) -> bool,
    ) -> VfRes {
        let entries = self.listdir(dir, masks, max_entries, false)?;
        for entry in &entries {
            if !cb(entry) {
                break;
            }
        }
        Ok(())
    }

    /// `tc_unlink()`.
    fn unlink(&mut self, pathname: &Path) -> VfResult<()> {
        self.removev(&[VfFile::from_os_path(pathname)])
    }

    /// `tc_unlinkv()`.
    fn unlinkv(&mut self, pathnames: &[&Path]) -> VfRes {
        let files: Vec<VfFile> = pathnames.iter().map(|p| VfFile::from_os_path(p)).collect();
        self.removev(&files)
    }

    /// Create a directory, `tc_mkdir()`.
    fn mkdir(&mut self, path: &Path, mode: u32) -> VfResult<()> {
        let a = VfAttrs {
            file: VfFile::from_os_path(path),
            masks: AttrMask::MODE,
            mode,
            ..VfAttrs::default()
        };
        self.mkdirv(std::slice::from_ref(&a))
    }

    /// Create a symlink, `tc_symlink()`.
    fn symlink(&mut self, oldpath: &Path, newpath: &Path) -> VfResult<()> {
        self.symlinkv(
            std::slice::from_ref(&oldpath),
            std::slice::from_ref(&newpath),
        )
    }

    /// Read a symlink target, `tc_readlink()`.
    fn readlink(&mut self, path: &Path) -> VfResult<Vec<u8>> {
        take_single_result("readlink", self.readlinkv(std::slice::from_ref(&path))?)
    }

    /// `tc_ldupv()`: same read/write extent copy as
    /// [`dupv`](Self::dupv). Retained for C API parity; a backend that
    /// distinguishes a "local" copy should override.
    fn ldupv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.dupv(pairs)
    }

    /// `tc_copyv()`: server-side copy where the backend supports it. The
    /// default implementation performs a client-side read/write copy via
    /// [`dupv`](Self::dupv); backends with a server-side COPY should
    /// override.
    fn copyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.dupv(pairs)
    }

    /// Create a directory and all its ancestors, `tc_ensure_dir()`. Uses
    /// `mkdir` and accepts an existing directory (`EEXIST`) instead of an
    /// exists-then-mkdir check, avoiding the race between the two. `dir` is
    /// resolved against the cwd via [`abs_path`](Self::abs_path) and then
    /// rebuilt as an absolute (root-relative) path.
    fn ensure_dir(&mut self, dir: &Path, mode: u32) -> VfResult<()> {
        use std::path::Component;
        let mut so_far = PathBuf::new();
        for comp in self.abs_path(dir).components() {
            if let Component::Normal(part) = comp {
                so_far.push(part);
                let full = Path::new("/").join(&so_far);
                match self.mkdir(&full, mode) {
                    Ok(()) => {}
                    Err(e) if e.err_no() == ERR_EXIST => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }
}

/// `AsRef<Path>` convenience wrappers for [`VecFs`] methods.
///
/// The object-safe [`VecFs`] trait intentionally takes `&Path`. These
/// wrappers accept any type that can be viewed as a path (`&str`, `String`,
/// `PathBuf`, `&Path`, ...) and are useful for Rust callers that do not need
/// dynamic dispatch.
pub trait VecFsExt: VecFs {
    fn stat_path<P: AsRef<Path>>(&mut self, path: P) -> VfResult<VfAttrs> {
        self.stat(path.as_ref())
    }

    fn lstat_path<P: AsRef<Path>>(&mut self, path: P) -> VfResult<VfAttrs> {
        self.lstat(path.as_ref())
    }

    fn exists_path<P: AsRef<Path>>(&mut self, path: P) -> VfResult<bool> {
        self.exists(path.as_ref())
    }

    fn file_type_path<P: AsRef<Path>>(&mut self, path: P) -> VfResult<VfType> {
        self.file_type(path.as_ref())
    }

    fn open_path<P: AsRef<Path>>(&mut self, path: P, flags: i32, mode: u32) -> VfResult<VfFile> {
        self.open(path.as_ref(), flags, mode)
    }

    fn mkdir_path<P: AsRef<Path>>(&mut self, path: P, mode: u32) -> VfResult<()> {
        self.mkdir(path.as_ref(), mode)
    }

    fn ensure_dir_path<P: AsRef<Path>>(&mut self, path: P, mode: u32) -> VfResult<()> {
        self.ensure_dir(path.as_ref(), mode)
    }

    fn chdir_path<P: AsRef<Path>>(&mut self, path: P) -> VfResult<()> {
        self.chdir(path.as_ref())
    }

    fn unlink_path<P: AsRef<Path>>(&mut self, path: P) -> VfResult<()> {
        self.unlink(path.as_ref())
    }

    fn readlink_path<P: AsRef<Path>>(&mut self, path: P) -> VfResult<Vec<u8>> {
        self.readlink(path.as_ref())
    }

    fn symlink_path<P, Q>(&mut self, oldpath: P, newpath: Q) -> VfResult<()>
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
    {
        self.symlink(oldpath.as_ref(), newpath.as_ref())
    }

    fn listdir_path<P: AsRef<Path>>(
        &mut self,
        path: P,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        self.listdir(path.as_ref(), masks, max_count, recursive)
    }

    fn walk_path<P: AsRef<Path>>(
        &mut self,
        root: P,
        masks: AttrMask,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        self.walk(root.as_ref(), masks, sort)
    }

    fn openv_paths<P: AsRef<Path>>(
        &mut self,
        paths: &[P],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<Vec<VfFile>> {
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_ref()).collect();
        self.openv(&refs, flags, modes)
    }

    fn openv_simple_paths<P: AsRef<Path>>(
        &mut self,
        paths: &[P],
        flags: i32,
        mode: u32,
    ) -> VfResult<Vec<VfFile>> {
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_ref()).collect();
        self.openv_simple(&refs, flags, mode)
    }

    fn unlinkv_paths<P: AsRef<Path>>(&mut self, paths: &[P]) -> VfRes {
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_ref()).collect();
        self.unlinkv(&refs)
    }

    fn rm_paths<P: AsRef<Path>>(&mut self, paths: &[P], recursive: bool) -> VfRes {
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_ref()).collect();
        self.rm(&refs, recursive)
    }

    fn rm_recursive_path<P: AsRef<Path>>(&mut self, path: P) -> VfRes {
        self.rm(&[path.as_ref()], true)
    }

    fn rm_contents_path<P: AsRef<Path>>(&mut self, path: P) -> VfRes {
        self.rm_contents(path.as_ref())
    }

    fn ensure_empty_dir_path<P: AsRef<Path>>(&mut self, path: P) -> VfRes {
        self.ensure_empty_dir(path.as_ref())
    }
}

impl<T: VecFs + ?Sized> VecFsExt for T {}

/// `tc_rm_recursive()`.
pub fn rm_recursive(fs: &mut impl VecFs, dir: &Path) -> VfRes {
    fs.rm(&[dir], true)
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    fn assert_contract_error(error: VfError, index: Option<usize>, detail: &str) {
        assert_eq!(error.domain(), vfsi_core::ErrorDomain::Transport);
        assert_eq!(error.index(), index);
        assert_eq!(error.status(), None);
        let message = error.to_string();
        assert!(message.contains("backend contract violation"), "{message}");
        assert!(message.contains(detail), "{message}");
    }

    fn request() -> ReadOp {
        ReadOp::at(VfFile::from_path("/file"), 0, 1)
    }

    fn result() -> ReadResult {
        ReadResult {
            file: VfFile::from_path("/file"),
            offset: 0,
            data: vec![1],
            eof: true,
        }
    }

    #[test]
    fn read_result_cardinality_is_checked_before_indexing() {
        let requests = [request(), request()];
        validate_read_results("test", &requests, &[result(), result()]).unwrap();
        validate_read_results("test", &[], &[]).unwrap();
        assert_contract_error(
            validate_read_results("test", &requests, &[result()]).unwrap_err(),
            None,
            "returned 1 results for 2 requests",
        );
        assert_contract_error(
            validate_read_results("test", &requests[..1], &[result(), result()]).unwrap_err(),
            None,
            "returned 2 results for 1 requests",
        );
    }

    #[test]
    fn read_result_identity_offset_progress_and_size_are_checked() {
        let request = request();
        validate_read_results("test", std::slice::from_ref(&request), &[result()]).unwrap();
        let larger = ReadOp::at(VfFile::from_path("/file"), 0, 4);
        validate_read_results(
            "test",
            &[larger],
            &[ReadResult {
                eof: false,
                ..result()
            }],
        )
        .unwrap();
        validate_read_results(
            "test",
            std::slice::from_ref(&request),
            &[ReadResult {
                data: Vec::new(),
                eof: true,
                ..result()
            }],
        )
        .unwrap();
        for malformed in [
            ReadResult {
                file: VfFile::from_path("/other"),
                ..result()
            },
            ReadResult {
                offset: 1,
                ..result()
            },
            ReadResult {
                data: vec![1, 2],
                ..result()
            },
            ReadResult {
                data: Vec::new(),
                eof: false,
                ..result()
            },
        ] {
            // Keep a valid prefix so attribution must identify the second
            // request rather than always returning request zero.
            assert_contract_error(
                validate_read_results(
                    "test",
                    &[request.clone(), request.clone()],
                    &[result(), malformed],
                )
                .unwrap_err(),
                Some(1),
                "test",
            );
        }
    }

    #[test]
    fn write_result_identity_offset_size_and_cardinality_are_checked() {
        let file = VfFile::from_path("/file");
        let request = WriteOpRef::new(&file, VfOffset::At(0), b"x");
        let valid = WriteResult {
            file: file.clone(),
            offset: 0,
            written: 1,
            stable: true,
        };
        validate_write_results("test", &[request], std::slice::from_ref(&valid)).unwrap();
        // Partial/zero progress is a valid backend result; write_allv owns
        // the policy for completing it or rejecting a no-progress loop.
        validate_write_results(
            "test",
            &[request],
            &[WriteResult {
                written: 0,
                ..valid.clone()
            }],
        )
        .unwrap();
        validate_write_results("test", &[], &[]).unwrap();
        assert_contract_error(
            validate_write_results("test", &[request], &[]).unwrap_err(),
            None,
            "returned 0 results",
        );
        assert_contract_error(
            validate_write_results("test", &[request], &[valid.clone(), valid.clone()])
                .unwrap_err(),
            None,
            "returned 2 results",
        );
        for malformed in [
            WriteResult {
                file: VfFile::from_path("/other"),
                ..valid.clone()
            },
            WriteResult {
                offset: 1,
                ..valid.clone()
            },
            WriteResult {
                written: 2,
                ..valid.clone()
            },
        ] {
            assert_contract_error(
                validate_write_results("test", &[request, request], &[valid.clone(), malformed])
                    .unwrap_err(),
                Some(1),
                "test",
            );
        }
    }

    #[test]
    fn scalar_result_cardinality_is_checked_without_panicking() {
        assert_contract_error(
            take_single_result::<u8>("readlink", Vec::new()).unwrap_err(),
            None,
            "readlink",
        );
        assert_contract_error(
            take_single_result("readlink", vec![1u8, 2]).unwrap_err(),
            None,
            "readlink",
        );
        assert_eq!(take_single_result("readlink", vec![7u8]).unwrap(), 7);
    }
}
