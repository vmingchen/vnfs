use super::*;
use vfsi_core::internal::ManyResults;

/// Default payload limit for APIs that allocate and return complete contents.
pub const DEFAULT_READ_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Default aggregate payload limit for [`VecFs::read_allv`].
pub const DEFAULT_READ_ALLV_MAX_TOTAL_BYTES: usize = DEFAULT_READ_MAX_BYTES;

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
    /// leading `/`. [`getcwd`](Self::getcwd) is the display form (with a
    /// leading `/`); [`vf_path`](Self::vf_path) resolves a [`VfFile`] the same
    /// way while honoring its [`VfPathBase`].
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
        // Explicit stack (pre-order, subdirectories visited in the order the
        // sort callback produced) so deep trees cannot overflow the call
        // stack.
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut entries = self.listdir(&dir, masks, 0, false)?;
            sort(dir.as_path(), &mut entries);
            let subdirs: Vec<PathBuf> = entries
                .iter()
                .filter(|e| e.ftype == VfType::Directory)
                .filter_map(|e| e.file.path().map(|p| p.to_path_buf()))
                .collect();
            for s in subdirs.into_iter().rev() {
                stack.push(s);
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
                error.index_opt().map_or(error.clone(), |local_index| {
                    batch_indices
                        .get(local_index)
                        .copied()
                        .map_or(error.clone(), |index| error.with_index(index))
                })
            })?;
            validate_read_results("read_streamv", &reads, &results).map_err(|error| {
                error.index_opt().map_or(error.clone(), |local_index| {
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
    fn rm(&mut self, objs: &[&Path], recursive: bool) -> VfRes;

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
    /// stops the listing early.
    fn listdirv(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> VfRes {
        for (i, d) in dirs.iter().enumerate() {
            let entries = self
                .listdir(d, masks, max_entries, recursive)
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
        let v = self.readlinkv(std::slice::from_ref(&path))?;
        Ok(v.into_iter().next().expect("one result"))
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
}

impl<T: VecFs + ?Sized> VecFsExt for T {}

/// `tc_rm_recursive()`.
pub fn rm_recursive(fs: &mut impl VecFs, dir: &Path) -> VfRes {
    fs.rm(&[dir], true)
}

#[cfg(test)]
mod contract_tests {
    use super::*;

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
        assert!(validate_read_results("test", &requests, &[result()]).is_err());
        assert!(validate_read_results("test", &requests[..1], &[result(), result()]).is_err());
    }

    #[test]
    fn read_result_identity_offset_progress_and_size_are_checked() {
        let request = request();
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
            assert!(
                validate_read_results("test", std::slice::from_ref(&request), &[malformed])
                    .is_err()
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
        assert!(validate_write_results("test", &[request], &[]).is_err());
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
            assert!(validate_write_results("test", &[request], &[malformed]).is_err());
        }
    }
}
