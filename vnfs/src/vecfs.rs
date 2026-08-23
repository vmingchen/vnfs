//! The vectorized filesystem API: the [`VecFs`] trait plus the shared types it
//! operates on.
//!
//! This mirrors the `tc_api.h` vectorized NFSv4 client surface, but as a
//! trait so any filesystem can implement it: the NFSv4.1 client
//! ([`crate::nfs::NfsVecFs`]) and a `std::fs`-backed dummy
//! ([`crate::dummy_vecfs::DummyVecFs`]). Vector operations take Rust slices.

use std::path::{Path, PathBuf};

use crate::error::RpcError;

// ---------------------------------------------------------------------------
// Constants (mirroring tc_api.h)
// ---------------------------------------------------------------------------

pub const VF_FD_NULL: i32 = -1;
pub const VF_FD_CWD: i32 = -2;
pub const VF_FD_ABS: i32 = -3;

pub const VF_OFFSET_END: u64 = u64::MAX;
pub const VF_OFFSET_CUR: u64 = u64::MAX - 1;

/// Errors that have no filesystem status (transport / client side).
pub const VF_ERR_RPC: u32 = 0xFFFF_FFFF;
/// Requested feature is not implemented by this backend.
pub const VF_ERR_UNSUPPORTED: u32 = 0xFFFF_FFFE;

/// Generic errno-style error codes (they coincide with the NFS4ERR codes for
/// the same conditions, which the NFS backend reports).
pub const ERR_NOENT: u32 = 2;
pub const ERR_EBADF: u32 = 9;
pub const ERR_EXIST: u32 = 17;
pub const ERR_NOTDIR: u32 = 20;
pub const ERR_ISDIR: u32 = 21;
pub const ERR_INVAL: u32 = 22;

/// NFSv4 file type codes used by [`VfAttrs::ftype`] and recursion logic.
pub const NF4REG: u32 = 1;
pub const NF4DIR: u32 = 2;
pub const NF4LNK: u32 = 5;

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Index of the first failed operation plus its error number, mirroring the
/// C `tc_res` struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfError {
    pub index: usize,
    pub err_no: u32,
}

impl VfError {
    pub fn failure(index: usize, err_no: u32) -> VfError {
        VfError { index, err_no }
    }

    /// Convert a low-level [`RpcError`] (which already carries the failing op
    /// index and NFS status) into a `tc` error. `index` is the caller's
    /// operation index, which may differ from the compound-internal op index.
    pub fn from_rpc(index: usize, e: RpcError) -> VfError {
        VfError {
            index,
            err_no: e.status,
        }
    }

    pub fn unsupported(index: usize) -> VfError {
        VfError::failure(index, VF_ERR_UNSUPPORTED)
    }
}

impl std::fmt::Display for VfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "op {} failed: {}", self.index, self.err_no)
    }
}

impl std::error::Error for VfError {}

pub type VfResult<T> = Result<T, VfError>;
/// Result of a compound-style operation: `()` on success, or the index and
/// error of the first failing operation.
pub type VfRes = VfResult<()>;

// ---------------------------------------------------------------------------
// Path helpers (backend-agnostic)
// ---------------------------------------------------------------------------

/// Split `path` into its parent directory path and final component.
pub(crate) fn split_path(path: &str) -> VfResult<(&str, &str)> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Err(VfError::failure(0, ERR_NOENT));
    }
    match trimmed.rfind('/') {
        Some(i) => Ok((&trimmed[..i], &trimmed[i + 1..])),
        None => Ok(("", trimmed)),
    }
}

/// Join a directory path and a name with `/`.
pub(crate) fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dir, name)
    }
}

// ---------------------------------------------------------------------------
// File references
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfFileType {
    Null,
    Descriptor,
    Path,
    Handle,
    Current,
    Saved,
}

/// A reference to a file, mirroring the C `tc_file` struct. Open state is
/// tracked by the backend (keyed by the descriptor), so this is just the
/// descriptor / path / kind.
#[derive(Debug, Clone)]
pub struct VfFile {
    pub ftype: VfFileType,
    /// For `Path`: `VF_FD_CWD` / `VF_FD_ABS` (or an open fd for fd-relative
    /// paths). For `Descriptor`: the backend-assigned file descriptor.
    pub fd: i32,
    /// The path for `Path`/`Current` variants.
    pub path: Option<PathBuf>,
}

impl VfFile {
    pub fn from_path(path: &str) -> VfFile {
        let fd = if path.starts_with('/') {
            VF_FD_ABS
        } else {
            VF_FD_CWD
        };
        VfFile {
            ftype: VfFileType::Path,
            fd,
            path: Some(PathBuf::from(path)),
        }
    }

    pub fn from_fd(fd: i32) -> VfFile {
        VfFile {
            ftype: VfFileType::Descriptor,
            fd,
            path: None,
        }
    }

    /// VF_FILE_CURRENT, with an optional path relative to the client's
    /// current working directory.
    pub fn current(relpath: Option<&str>) -> VfFile {
        VfFile {
            ftype: VfFileType::Current,
            fd: -1,
            path: relpath.map(PathBuf::from),
        }
    }

    pub fn saved() -> VfFile {
        VfFile {
            ftype: VfFileType::Saved,
            fd: -1,
            path: None,
        }
    }
}

impl Default for VfFile {
    fn default() -> VfFile {
        VfFile {
            ftype: VfFileType::Null,
            fd: VF_FD_NULL,
            path: None,
        }
    }
}

// ---------------------------------------------------------------------------
// I/O vectors and attributes
// ---------------------------------------------------------------------------

/// One element of a readv/writev call, mirroring `struct tc_iovec`.
#[derive(Debug, Clone)]
pub struct VfIoVec {
    pub file: VfFile,
    /// IN: read/write offset.
    pub offset: u64,
    /// IN: requested bytes; OUT: bytes read/written.
    pub length: usize,
    /// IN: data to write; OUT: data read.
    pub data: Vec<u8>,
    /// IN: create the file if it does not exist (writev).
    pub is_creation: bool,
    /// OUT: this element failed.
    pub is_failure: bool,
    /// OUT: read reached end-of-file.
    pub is_eof: bool,
    /// IN/OUT: stable write.
    pub is_write_stable: bool,
}

impl VfIoVec {
    pub fn new(file: VfFile, offset: u64, length: usize, data: Vec<u8>) -> VfIoVec {
        VfIoVec {
            file,
            offset,
            length,
            data,
            is_creation: false,
            is_failure: false,
            is_eof: false,
            is_write_stable: true,
        }
    }

    pub fn from_path(path: &str, offset: u64, length: usize, data: Vec<u8>) -> VfIoVec {
        VfIoVec::new(VfFile::from_path(path), offset, length, data)
    }

    /// An iovec for an open file descriptor, for `VF_OFFSET_CUR` reads.
    pub fn from_fd(fd: i32, offset: u64, length: usize, data: Vec<u8>) -> VfIoVec {
        VfIoVec::new(VfFile::from_fd(fd), offset, length, data)
    }
}

/// One extent to copy, mirroring `struct tc_extent_pair`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentPair {
    pub src_path: String,
    pub dst_path: String,
    pub src_offset: u64,
    pub dst_offset: u64,
    /// Bytes to copy; `u64::MAX` means "from src_offset to end of file".
    pub length: u64,
}

impl ExtentPair {
    /// `tc_fill_extent_pair()`.
    pub fn new(
        src_path: &str,
        src_offset: u64,
        dst_path: &str,
        dst_offset: u64,
        length: u64,
    ) -> ExtentPair {
        ExtentPair {
            src_path: src_path.to_string(),
            dst_path: dst_path.to_string(),
            src_offset,
            dst_offset,
            length,
        }
    }
}

/// An Application Data Block (ADB) pattern, mirroring `struct tc_adb`.
#[derive(Debug, Clone)]
pub struct Adb {
    pub path: String,
    pub adb_offset: u64,
    pub adb_block_size: u64,
    /// IN: blocks requested; OUT: blocks written.
    pub adb_block_count: usize,
    /// Relative offset within a block to write the ADBN; `u64::MAX` = none.
    pub adb_reloff_blocknum: u64,
    /// ADBN of the first ADB.
    pub adb_block_num: u64,
    /// Relative offset within a block to write the pattern; `u64::MAX` = none.
    pub adb_reloff_pattern: u64,
    pub adb_pattern_size: usize,
    pub adb_pattern_data: Vec<u8>,
}

impl Adb {
    /// An ADB writing only block numbers at `reloff_blocknum`.
    pub fn blocknum_only(
        path: &str,
        offset: u64,
        block_size: u64,
        block_count: usize,
        reloff_blocknum: u64,
        first_adbn: u64,
    ) -> Adb {
        Adb {
            path: path.to_string(),
            adb_offset: offset,
            adb_block_size: block_size,
            adb_block_count: block_count,
            adb_reloff_blocknum: reloff_blocknum,
            adb_block_num: first_adbn,
            adb_reloff_pattern: u64::MAX,
            adb_pattern_size: 0,
            adb_pattern_data: Vec::new(),
        }
    }
}

/// Presence mask for `VfAttrs`, mirroring `struct tc_attrs_masks`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttrMask {
    pub has_mode: bool,
    pub has_size: bool,
    pub has_nlink: bool,
    pub has_fileid: bool,
    pub has_blocks: bool,
    pub has_uid: bool,
    pub has_gid: bool,
    pub has_rdev: bool,
    pub has_atime: bool,
    pub has_mtime: bool,
    pub has_ctime: bool,
    /// Request FATTR4_NAMED_ATTR (the per-object "has named attributes"
    /// boolean). Costs the server a per-entry xattr enumeration, so only
    /// request it when the caller needs it (e.g. ls long format).
    pub has_named_attr: bool,
}

impl AttrMask {
    pub fn all() -> AttrMask {
        AttrMask {
            has_mode: true,
            has_size: true,
            has_nlink: true,
            has_fileid: true,
            has_blocks: true,
            has_uid: true,
            has_gid: true,
            has_rdev: true,
            has_atime: true,
            has_mtime: true,
            has_ctime: true,
            has_named_attr: true,
        }
    }
}

/// A directory and its entries, as returned by [`VecFs::walk`]. `path` is the
/// directory's root-relative path; `entries` are its immediate children.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    pub path: String,
    pub entries: Vec<VfAttrs>,
}

/// File attributes, mirroring `struct tc_attrs`. `mode` is the full `st_mode`
/// (permission bits plus `S_IFMT` file-type bits); the `mtime/atime/ctime`
/// fields hold seconds and nanoseconds.
#[derive(Debug, Clone, Default)]
pub struct VfAttrs {
    pub file: VfFile,
    pub masks: AttrMask,
    pub ftype: u32,
    pub mode: u32,
    pub size: u64,
    pub nlink: u32,
    pub fileid: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub blocks: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub atime_sec: i64,
    pub atime_nsec: u32,
    pub ctime_sec: i64,
    pub ctime_nsec: u32,
    /// FATTR4_NAMED_ATTR: TRUE iff the object has a non-empty named
    /// attribute directory (i.e. at least one `user.*` xattr).
    pub has_named_attr: bool,
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// A vectorized filesystem: many small operations coalesced into as few
/// round trips as the backend supports.
///
/// `VfFile` references files either by descriptor (an open file) or by path
/// (absolute, or relative to the client's current working directory).
pub trait VecFs {
    // -- required -----------------------------------------------------------

    /// Return the root-relative form of `path` (resolving it against the
    /// client's current working directory if it is relative).
    fn abs_path(&self, path: &str) -> String;

    /// Open a file by path, similar to `tc_open_by_path(2)`. `dirfd` may be
    /// `VF_FD_CWD` or `VF_FD_ABS`. When `O_CREAT` is set, `mode` is applied
    /// to the new file.
    fn open_by_path(
        &mut self,
        dirfd: i32,
        pathname: &str,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile>;

    /// Close an open file, `tc_close()`.
    fn close(&mut self, tcf: &VfFile) -> VfResult<()>;

    /// Change the client's current directory, `tc_chdir()`.
    fn chdir(&mut self, path: &str) -> VfResult<()>;

    /// Current working directory, `tc_getcwd()`.
    fn getcwd(&self) -> String;

    /// Read from one or more files, `tc_readv()`.
    fn readv(&mut self, reads: &mut [VfIoVec]) -> VfRes;

    /// Write to one or more files, `tc_writev()`.
    fn writev(&mut self, writes: &mut [VfIoVec]) -> VfRes;

    /// Reposition the read/write offset of an open file, `tc_fseek()`.
    /// `whence` is `SEEK_SET`, `SEEK_CUR` or `SEEK_END`.
    fn fseek(&mut self, tcf: &mut VfFile, offset: i64, whence: i32) -> VfResult<i64>;

    /// Get attributes of an array of files, `tc_getattrsv()`.
    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes;

    /// Set attributes (mode / size) on an array of files, `tc_setattrsv()`.
    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes;

    /// List a directory, `tc_listdir()`. Returns entry paths and attributes.
    fn listdir(
        &mut self,
        dir: &str,
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
        root: &str,
        masks: AttrMask,
        sort: &dyn Fn(&str, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        fn rec<F: VecFs + ?Sized>(
            fs: &mut F,
            dir: &str,
            masks: AttrMask,
            sort: &dyn Fn(&str, &mut Vec<VfAttrs>),
            out: &mut Vec<WalkEntry>,
        ) -> VfResult<()> {
            let mut entries = fs.listdir(dir, masks, 0, false)?;
            sort(dir, &mut entries);
            let subdirs: Vec<String> = entries
                .iter()
                .filter(|e| e.ftype == NF4DIR)
                .filter_map(|e| {
                    e.file
                        .path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .collect();
            out.push(WalkEntry {
                path: dir.to_string(),
                entries,
            });
            for s in subdirs {
                rec(fs, &s, masks, sort, out)?;
            }
            Ok(())
        }
        let mut out = Vec::new();
        rec(self, root, masks, sort, &mut out)?;
        Ok(out)
    }

    /// Rename a list of file pairs, `tc_renamev()`.
    fn renamev(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes;

    /// Remove a list of files (or empty directories), `tc_removev()`.
    fn removev(&mut self, files: &[VfFile]) -> VfRes;

    /// Create one or more directories, `tc_mkdirv()`.
    fn mkdirv(&mut self, dirs: &[VfAttrs]) -> VfRes;

    /// Create a list of symlinks, `tc_symlinkv()`.
    fn symlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str]) -> VfRes;

    /// Read symlink targets, `tc_readlinkv()`.
    fn readlinkv(&mut self, paths: &[&str]) -> VfResult<Vec<Vec<u8>>>;

    /// Create hard links, `tc_hardlinkv()`.
    fn hardlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str]) -> VfRes;

    /// Copy extents by reading and writing, `tc_dupv()` / `tc_lcopyv()`.
    fn dupv(&mut self, pairs: &[ExtentPair]) -> VfRes;

    /// Write Application Data Blocks, `tc_write_adb()`.
    fn write_adb(&mut self, patterns: &mut [Adb]) -> VfRes;

    /// Remove a list of objects, recursively when `recursive`, `tc_rm()`.
    fn rm(&mut self, objs: &[&str], recursive: bool) -> VfRes;

    /// Recursively copy a directory tree, `tc_cp_recursive()`.
    fn cp_recursive(
        &mut self,
        src_dir: &str,
        dst: &str,
        symlinks: bool,
        use_server_side_copy: bool,
    ) -> VfRes;

    // -- defaults -----------------------------------------------------------

    /// Open a file by path, `tc_open()`.
    fn open(&mut self, pathname: &str, flags: i32, mode: u32) -> VfResult<VfFile> {
        self.open_by_path(VF_FD_CWD, pathname, flags, mode)
    }

    /// Open several files at once, each with its own flags and mode,
    /// `tc_openv()`.
    fn openv(&mut self, paths: &[&str], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let mut out = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            out.push(self.open(p, flags[i], modes[i]).map_err(|mut e| {
                e.index = i;
                e
            })?);
        }
        Ok(out)
    }

    /// Open several files at once with a shared flags and mode,
    /// `tc_openv_simple()`.
    fn openv_simple(&mut self, paths: &[&str], flags: i32, mode: u32) -> VfResult<Vec<VfFile>> {
        let flags_v = vec![flags; paths.len()];
        let modes_v = vec![mode; paths.len()];
        self.openv(paths, &flags_v, &modes_v)
    }

    /// Close several files, `tc_closev()`.
    fn closev(&mut self, files: &[VfFile]) -> VfRes {
        for (i, f) in files.iter().enumerate() {
            self.close(f).map_err(|mut e| {
                e.index = i;
                e
            })?;
        }
        Ok(())
    }

    /// `tc_lgetattrsv()`: like getattrsv but does not follow symlinks.
    fn lgetattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        self.getattrsv(attrs)
    }

    /// `tc_lsetattrsv()`.
    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.setattrsv(attrs)
    }

    /// Stat a path, `tc_stat()`.
    fn stat(&mut self, path: &str) -> VfResult<VfAttrs> {
        let mut a = VfAttrs {
            file: VfFile::from_path(path),
            masks: AttrMask {
                has_mode: true,
                has_size: true,
                has_nlink: true,
                has_fileid: true,
                ..AttrMask::default()
            },
            ..VfAttrs::default()
        };
        self.getattrsv(std::slice::from_mut(&mut a))?;
        Ok(a)
    }

    /// `tc_lstat()`.
    fn lstat(&mut self, path: &str) -> VfResult<VfAttrs> {
        self.stat(path)
    }

    /// `tc_fstat()`.
    fn fstat(&mut self, tcf: &VfFile) -> VfResult<VfAttrs> {
        let mut a = VfAttrs {
            file: tcf.clone(),
            masks: AttrMask {
                has_mode: true,
                has_size: true,
                has_nlink: true,
                has_fileid: true,
                ..AttrMask::default()
            },
            ..VfAttrs::default()
        };
        self.getattrsv(std::slice::from_mut(&mut a))?;
        Ok(a)
    }

    /// `tc_exists()`.
    fn exists(&mut self, path: &str) -> bool {
        self.lstat(path).is_ok()
    }

    /// Return the file type of `path` (a `NF4*` value).
    fn file_type(&mut self, path: &str) -> VfResult<u32> {
        Ok(self.stat(path)?.ftype)
    }

    /// List directories with a callback, `tc_listdirv()`. Returning `false`
    /// stops the listing early.
    fn listdirv<F>(
        &mut self,
        dirs: &[&str],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut F,
    ) -> VfRes
    where
        F: FnMut(&VfAttrs, &str) -> bool,
    {
        for (i, d) in dirs.iter().enumerate() {
            let entries = self
                .listdir(d, masks, max_entries, recursive)
                .map_err(|mut e| {
                    e.index = i;
                    e
                })?;
            for e in &entries {
                if !cb(e, d) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// `tc_unlink()`.
    fn unlink(&mut self, pathname: &str) -> VfResult<()> {
        self.removev(&[VfFile::from_path(pathname)])
    }

    /// `tc_unlinkv()`.
    fn unlinkv(&mut self, pathnames: &[&str]) -> VfRes {
        let files: Vec<VfFile> = pathnames.iter().map(|p| VfFile::from_path(p)).collect();
        self.removev(&files)
    }

    /// Create a directory, `tc_mkdir()`.
    fn mkdir(&mut self, path: &str, mode: u32) -> VfResult<()> {
        let a = VfAttrs {
            file: VfFile::from_path(path),
            masks: AttrMask {
                has_mode: true,
                ..AttrMask::default()
            },
            mode,
            ..VfAttrs::default()
        };
        self.mkdirv(std::slice::from_ref(&a))
    }

    /// Create a symlink, `tc_symlink()`.
    fn symlink(&mut self, oldpath: &str, newpath: &str) -> VfResult<()> {
        self.symlinkv(
            std::slice::from_ref(&oldpath),
            std::slice::from_ref(&newpath),
        )
    }

    /// Read a symlink target, `tc_readlink()`.
    fn readlink(&mut self, path: &str) -> VfResult<Vec<u8>> {
        let mut v = self.readlinkv(std::slice::from_ref(&path))?;
        Ok(v.remove(0))
    }

    /// `tc_ldupv()`.
    fn ldupv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.dupv(pairs)
    }

    /// `tc_copyv()` / `tc_lcopyv()`: server-side copy. No backend provides a
    /// server-side COPY, so this falls back to the read/write copy of
    /// [`dupv`](Self::dupv).
    fn copyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.dupv(pairs)
    }

    /// `tc_lcopyv()`.
    fn lcopyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.dupv(pairs)
    }

    /// Create a directory and all its ancestors, `tc_ensure_dir()`.
    fn ensure_dir(&mut self, dir: &str, mode: u32) -> VfResult<()> {
        let rel = self.abs_path(dir);
        let mut so_far = String::new();
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            so_far = join_path(&so_far, comp);
            let full = format!("/{}", so_far);
            if !self.exists(&full) {
                self.mkdir(&full, mode)?;
            }
        }
        Ok(())
    }
}

/// `tc_rm_recursive()`.
pub fn rm_recursive(fs: &mut impl VecFs, dir: &str) -> VfRes {
    fs.rm(&[dir], true)
}

/// `tc_okay()` helper: true when a VfRes succeeded.
pub fn okay(res: &VfRes) -> bool {
    res.is_ok()
}

/// Convenience: `tc_file_from_path()`.
pub fn file_from_path(path: &str) -> VfFile {
    VfFile::from_path(path)
}

// Keep `Path` referenced for API stability (getcwd / chdir paths).
#[allow(unused)]
fn _path_ref(_p: &Path) {}
