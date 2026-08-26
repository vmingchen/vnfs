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

/// NFSv4 wire type codes (NF4*), used to decode/encode [`VfType`].
pub const NF4REG: u32 = 1;
pub const NF4DIR: u32 = 2;
pub const NF4BLK: u32 = 3;
pub const NF4CHR: u32 = 4;
pub const NF4LNK: u32 = 5;
pub const NF4SOCK: u32 = 6;
pub const NF4FIFO: u32 = 7;

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

/// Split `path` into its parent directory path and final component. Built on
/// [`Path`] so repeated separators are handled.
pub(crate) fn split_path(path: &str) -> VfResult<(&str, &str)> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Err(VfError::failure(0, ERR_NOENT));
    }
    let p = Path::new(trimmed);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| VfError::failure(0, ERR_NOENT))?;
    let dir = p.parent().and_then(|d| d.to_str()).unwrap_or("");
    Ok((dir, name))
}

/// Join a directory path and a name with `/`.
pub(crate) fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        PathBuf::from(dir).join(name).to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// File references
// ---------------------------------------------------------------------------

/// The base a path is resolved against, replacing the C `VF_FD_CWD` /
/// `VF_FD_ABS` sentinel descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfPathBase {
    /// Relative to the client's current working directory.
    Cwd,
    /// Absolute.
    Abs,
}

/// How [`VecFs::fseek`] interprets its offset, mirroring `SEEK_SET` /
/// `SEEK_CUR` / `SEEK_END` as an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekFrom {
    Set,
    Cur,
    End,
}

/// A reference to a file: an open descriptor, a path, or a special
/// pseudo-file. This is the Rust-native form of the C `tc_file` tagged
/// struct (`type` + `fd` + `path`), where the variant *is* the tag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum VfFile {
    /// No file.
    #[default]
    Null,
    /// An open file descriptor (backend-assigned).
    Descriptor(i32),
    /// A path, absolute or relative to the client's cwd.
    Path { base: VfPathBase, path: PathBuf },
    /// The client's current working directory, optionally with a relative
    /// path below it.
    Current(Option<PathBuf>),
    /// The saved (previous) directory.
    Saved,
}

impl VfFile {
    pub fn from_path(path: &str) -> VfFile {
        let base = if path.starts_with('/') {
            VfPathBase::Abs
        } else {
            VfPathBase::Cwd
        };
        VfFile::Path {
            base,
            path: PathBuf::from(path),
        }
    }

    pub fn from_fd(fd: i32) -> VfFile {
        VfFile::Descriptor(fd)
    }

    /// VF_FILE_CURRENT, with an optional path relative to the client's
    /// current working directory.
    pub fn current(relpath: Option<&str>) -> VfFile {
        VfFile::Current(relpath.map(PathBuf::from))
    }

    pub fn saved() -> VfFile {
        VfFile::Saved
    }

    /// Whether this references an open descriptor.
    pub fn is_descriptor(&self) -> bool {
        matches!(self, VfFile::Descriptor(_))
    }

    /// The open descriptor, if this is a `Descriptor`.
    pub fn fd(&self) -> Option<i32> {
        match self {
            VfFile::Descriptor(fd) => Some(*fd),
            _ => None,
        }
    }

    /// The path, for `Path` and `Current(Some(..))`.
    pub fn path(&self) -> Option<&Path> {
        match self {
            VfFile::Path { path, .. } => Some(path),
            VfFile::Current(Some(p)) => Some(p),
            _ => None,
        }
    }
}

/// The NFSv4 object type, replacing the raw `NF4*` wire codes in
/// [`VfAttrs::ftype`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VfType {
    #[default]
    Regular,
    Directory,
    Symlink,
    BlockDevice,
    CharDevice,
    Fifo,
    Socket,
    /// Any other (or unknown) NFSv4 type code.
    Other(u32),
}

impl VfType {
    pub fn from_nfs(code: u32) -> VfType {
        match code {
            NF4REG => VfType::Regular,
            NF4DIR => VfType::Directory,
            NF4LNK => VfType::Symlink,
            NF4BLK => VfType::BlockDevice,
            NF4CHR => VfType::CharDevice,
            NF4FIFO => VfType::Fifo,
            NF4SOCK => VfType::Socket,
            other => VfType::Other(other),
        }
    }

    /// The NFSv4 wire type code.
    pub fn as_nfs(&self) -> u32 {
        match self {
            VfType::Regular => NF4REG,
            VfType::Directory => NF4DIR,
            VfType::Symlink => NF4LNK,
            VfType::BlockDevice => NF4BLK,
            VfType::CharDevice => NF4CHR,
            VfType::Fifo => NF4FIFO,
            VfType::Socket => NF4SOCK,
            VfType::Other(code) => *code,
        }
    }
}

// ---------------------------------------------------------------------------
// I/O vectors and attributes
// ---------------------------------------------------------------------------

/// One element of a batched read, replacing the C `tc_iovec` (which mixed
/// input and output fields in a single mutable struct).
#[derive(Debug, Clone)]
pub struct ReadOp {
    pub file: VfFile,
    pub offset: u64,
    /// Number of bytes to fetch.
    pub length: usize,
}

impl ReadOp {
    pub fn new(file: VfFile, offset: u64, length: usize) -> ReadOp {
        ReadOp {
            file,
            offset,
            length,
        }
    }

    pub fn from_path(path: &str, offset: u64, length: usize) -> ReadOp {
        ReadOp::new(VfFile::from_path(path), offset, length)
    }

    /// A read from an open descriptor, for `VF_OFFSET_CUR` (sequential)
    /// reads.
    pub fn from_fd(fd: i32, offset: u64, length: usize) -> ReadOp {
        ReadOp::new(VfFile::from_fd(fd), offset, length)
    }
}

/// The result of one [`ReadOp`]: the data and whether end-of-file was hit.
#[derive(Debug, Clone)]
pub struct ReadResult {
    pub file: VfFile,
    /// The offset the read actually started at.
    pub offset: u64,
    pub data: Vec<u8>,
    /// True if the read reached end-of-file.
    pub eof: bool,
}

/// One element of a batched write.
#[derive(Debug, Clone)]
pub struct WriteOp {
    pub file: VfFile,
    pub offset: u64,
    pub data: Vec<u8>,
    /// Create the file if it does not exist.
    pub creation: bool,
}

impl WriteOp {
    pub fn new(file: VfFile, offset: u64, data: Vec<u8>) -> WriteOp {
        WriteOp {
            file,
            offset,
            data,
            creation: false,
        }
    }

    pub fn from_path(path: &str, offset: u64, data: Vec<u8>) -> WriteOp {
        WriteOp::new(VfFile::from_path(path), offset, data)
    }

    pub fn from_fd(fd: i32, offset: u64, data: Vec<u8>) -> WriteOp {
        WriteOp::new(VfFile::from_fd(fd), offset, data)
    }

    /// Create the file if it does not exist.
    pub fn with_creation(mut self) -> WriteOp {
        self.creation = true;
        self
    }
}

/// The result of one [`WriteOp`].
#[derive(Debug, Clone)]
pub struct WriteResult {
    pub file: VfFile,
    /// The offset the write actually started at.
    pub offset: u64,
    pub written: usize,
    /// Whether the server committed the write to stable storage.
    pub stable: bool,
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
    /// Blocks to write.
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

// Presence mask for `VfAttrs`, controlling which attributes a backend must
// fetch and return. A bitflags set instead of the C `tc_attrs_masks` bool
// struct.
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[doc = "A bitflags set of requested attributes."]
    pub struct AttrMask: u32 {
        const MODE = 1 << 0;
        const SIZE = 1 << 1;
        const NLINK = 1 << 2;
        const FILEID = 1 << 3;
        const BLOCKS = 1 << 4;
        const UID = 1 << 5;
        const GID = 1 << 6;
        const RDEV = 1 << 7;
        const ATIME = 1 << 8;
        const MTIME = 1 << 9;
        const CTIME = 1 << 10;
        /// Request FATTR4_NAMED_ATTR (the per-object "has named attributes"
        /// boolean). Costs the server a per-entry xattr enumeration, so only
        /// request it when the caller needs it (e.g. ls long format).
        const NAMED_ATTR = 1 << 11;
    }
}

impl AttrMask {
    /// The attributes [`VecFs::stat`] needs (mode, size, links, fileid).
    pub fn stat() -> AttrMask {
        AttrMask::MODE | AttrMask::SIZE | AttrMask::NLINK | AttrMask::FILEID
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
    pub ftype: VfType,
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

    /// Open a file by path, similar to `tc_open_by_path(2)`. `base` is
    /// `VfPathBase::Cwd` or `VfPathBase::Abs`. When `O_CREAT` is set, `mode`
    /// is applied to the new file.
    fn open_by_path(
        &mut self,
        base: VfPathBase,
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

    /// Read from one or more files, `tc_readv()`. Returns one result per
    /// request, or fails at the first failing operation.
    fn readv(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>>;

    /// Write to one or more files, `tc_writev()`. Returns one result per
    /// request, or fails at the first failing operation.
    fn writev(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>>;

    /// Reposition the read/write offset of an open file, `tc_fseek()`.
    fn fseek(&mut self, tcf: &mut VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64>;

    /// Get attributes of an array of files, `tc_getattrsv()`. Follows
    /// symlinks to the target.
    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes;

    /// Like [`getattrsv`](Self::getattrsv) but does not follow symlinks:
    /// attributes are for the symlink itself, `tc_lgetattrsv()`.
    fn lgetattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes;

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
    fn walk<F: Fn(&str, &mut Vec<VfAttrs>)>(
        &mut self,
        root: &str,
        masks: AttrMask,
        sort: F,
    ) -> VfResult<Vec<WalkEntry>> {
        fn rec<F: VecFs + ?Sized, S: Fn(&str, &mut Vec<VfAttrs>)>(
            fs: &mut F,
            dir: &str,
            masks: AttrMask,
            sort: &S,
            out: &mut Vec<WalkEntry>,
        ) -> VfResult<()> {
            let mut entries = fs.listdir(dir, masks, 0, false)?;
            sort(dir, &mut entries);
            let subdirs: Vec<String> = entries
                .iter()
                .filter(|e| e.ftype == VfType::Directory)
                .filter_map(|e| e.file.path().map(|p| p.to_string_lossy().into_owned()))
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
        rec(self, root, masks, &sort, &mut out)?;
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

    /// Write Application Data Blocks, `tc_write_adb()`. Returns the number
    /// of blocks written for each ADB.
    fn write_adb(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>>;

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
        self.open_by_path(VfPathBase::Cwd, pathname, flags, mode)
    }

    /// Read from a single file, `tc_read()`.
    fn read(&mut self, file: &VfFile, offset: u64, length: usize) -> VfResult<Vec<u8>> {
        let mut r = self.readv(&[ReadOp::new(file.clone(), offset, length)])?;
        Ok(r.remove(0).data)
    }

    /// Write to a single file, `tc_write()`.
    fn write(&mut self, file: &VfFile, offset: u64, data: &[u8]) -> VfResult<usize> {
        let mut w = self.writev(&[WriteOp::new(file.clone(), offset, data.to_vec())])?;
        Ok(w.remove(0).written)
    }

    /// Open several files at once, each with its own flags and mode,
    /// `tc_openv()`.
    fn openv(&mut self, paths: &[&str], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
        let mut out = Vec::with_capacity(paths.len());
        for (i, ((p, flag), mode)) in paths.iter().zip(flags).zip(modes).enumerate() {
            out.push(self.open(p, *flag, *mode).map_err(|mut e| {
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

    /// `tc_lsetattrsv()`.
    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.setattrsv(attrs)
    }

    /// Stat a path, `tc_stat()`. Follows symlinks to the target.
    fn stat(&mut self, path: &str) -> VfResult<VfAttrs> {
        let mut a = VfAttrs {
            file: VfFile::from_path(path),
            masks: AttrMask::stat(),
            ..VfAttrs::default()
        };
        self.getattrsv(std::slice::from_mut(&mut a))?;
        Ok(a)
    }

    /// `tc_lstat()`: like [`stat`](Self::stat) but does not follow symlinks.
    fn lstat(&mut self, path: &str) -> VfResult<VfAttrs> {
        let mut a = VfAttrs {
            file: VfFile::from_path(path),
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
    /// (e.g. permission denied) that are returned as `Err`.
    fn exists(&mut self, path: &str) -> VfResult<bool> {
        match self.lstat(path) {
            Ok(_) => Ok(true),
            Err(e) if e.err_no == ERR_NOENT => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Return the file type of `path`.
    fn file_type(&mut self, path: &str) -> VfResult<VfType> {
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
            masks: AttrMask::MODE,
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

    /// Create a directory and all its ancestors, `tc_ensure_dir()`. Uses
    /// `mkdir` and accepts an existing directory (`EEXIST`) instead of an
    /// exists-then-mkdir check, avoiding the race between the two.
    fn ensure_dir(&mut self, dir: &str, mode: u32) -> VfResult<()> {
        use std::path::Component;
        let mut so_far = PathBuf::new();
        for comp in Path::new(&self.abs_path(dir)).components() {
            if let Component::Normal(part) = comp {
                so_far.push(part);
                let full = format!("/{}", so_far.to_string_lossy());
                match self.mkdir(&full, mode) {
                    Ok(()) => {}
                    Err(e) if e.err_no == ERR_EXIST => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }
}

/// `tc_rm_recursive()`.
pub fn rm_recursive(fs: &mut impl VecFs, dir: &str) -> VfRes {
    fs.rm(&[dir], true)
}
