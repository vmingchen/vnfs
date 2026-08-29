//! The vectorized filesystem API: the [`VecFs`] trait plus the shared types it
//! operates on.
//!
//! This mirrors the `tc_api.h` vectorized NFSv4 client surface, but as a
//! trait so any filesystem can implement it: the NFSv4.1 client
//! ([`crate::nfs::NfsVecFs`]) and a `std::fs`-backed dummy
//! ([`crate::dummy_vecfs::DummyVecFs`]). Vector operations take Rust slices.
//!
//! # Path and name representation
//!
//! Paths are root-relative strings: an absolute path starts with `/` and is
//! resolved against the filesystem root (for NFS, the export root), while a
//! relative path is resolved against the client's current working directory
//! (see [`VecFs::abs_path`] and [`VecFs::vf_path`]). Component names are
//! UTF-8 `String`s/`PathBuf`s; NFS component names that are not valid UTF-8
//! cannot be represented through this API.

use std::path::{Path, PathBuf};

use crate::error::RpcError;

// ---------------------------------------------------------------------------
// Constants (mirroring tc_api.h)
// ---------------------------------------------------------------------------

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
pub const ERR_ACCES: u32 = 13;

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

/// The failure of one operation in a vectorized call.
///
/// Either a filesystem status failure attributable to a specific operation
/// index, or a transport / client-side failure (where the index is
/// best-effort: backends report 0 when the failure cannot be attributed).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VfError {
    /// A filesystem status failure; `err_no` is errno-style or an NFS4ERR
    /// code, mirroring the C `tc_res` struct.
    Op { index: usize, err_no: u32 },
    /// A transport / client-side failure with a human-readable message; there
    /// is no filesystem status ([`err_no`](VfError::err_no) reports
    /// [`VF_ERR_RPC`]).
    Transport { index: usize, message: String },
}

impl VfError {
    pub fn failure(index: usize, err_no: u32) -> VfError {
        VfError::Op { index, err_no }
    }

    /// A transport / client-side failure. `index` is best-effort (0 when the
    /// failure cannot be attributed to a specific operation).
    pub fn transport(index: usize, message: impl Into<String>) -> VfError {
        VfError::Transport {
            index,
            message: message.into(),
        }
    }

    pub fn unsupported(index: usize) -> VfError {
        VfError::failure(index, VF_ERR_UNSUPPORTED)
    }

    /// The operation index this error refers to (best-effort for transport
    /// failures).
    pub fn index(&self) -> usize {
        match self {
            VfError::Op { index, .. } | VfError::Transport { index, .. } => *index,
        }
    }

    /// The filesystem status code, or [`VF_ERR_RPC`] for transport failures.
    pub fn err_no(&self) -> u32 {
        match self {
            VfError::Op { err_no, .. } => *err_no,
            VfError::Transport { .. } => VF_ERR_RPC,
        }
    }

    /// Whether this is a transport / client-side failure (no filesystem
    /// status).
    pub fn is_transport(&self) -> bool {
        matches!(self, VfError::Transport { .. })
    }

    /// Convert a low-level [`RpcError`] into a `VfError`, attributing the
    /// failure to `index` in the caller's operation array. `index` is the
    /// caller's operation index, which may differ from the compound-internal
    /// op index; for transport failures the index is best-effort. The
    /// transport message (if any) is preserved.
    pub fn from_rpc(e: RpcError, index: usize) -> VfError {
        if e.is_transport() {
            VfError::Transport {
                index,
                message: e.message,
            }
        } else {
            VfError::Op {
                index,
                err_no: e.status,
            }
        }
    }

    /// Like [`from_rpc`](VfError::from_rpc), but trusts `e.op_index` as the
    /// caller-relative operation index. The batched client helpers translate
    /// compound positions to caller indices before returning, so batched
    /// backend calls can use this directly.
    pub fn from_rpc_indexed(e: RpcError) -> VfError {
        if e.is_transport() {
            VfError::Transport {
                index: e.op_index,
                message: e.message,
            }
        } else {
            VfError::Op {
                index: e.op_index,
                err_no: e.status,
            }
        }
    }

    /// Re-attribute this error to a different operation index.
    pub fn with_index(self, index: usize) -> VfError {
        match self {
            VfError::Op { err_no, .. } => VfError::Op { index, err_no },
            VfError::Transport { message, .. } => VfError::Transport { index, message },
        }
    }
}

impl std::fmt::Display for VfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VfError::Op { index, err_no } => write!(f, "op {} failed: {}", index, err_no),
            VfError::Transport { index, message } => {
                write!(f, "op {} transport error: {}", index, message)
            }
        }
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

/// Split `path` into its parent directory path and final component, returning
/// the errno on failure (there is no operation index at this layer; callers
/// attach one). Built on [`Path`] so repeated separators are handled.
pub(crate) fn split_path(path: &str) -> Result<(&str, &str), u32> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Err(ERR_NOENT);
    }
    let p = Path::new(trimmed);
    let name = p.file_name().and_then(|n| n.to_str()).ok_or(ERR_NOENT)?;
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

/// Lexically normalize a root-relative path (no leading `/`): drop `.` and
/// empty components, apply `..` by popping the last component (clamped at the
/// root, so a leading `..` is ignored, matching `/..` == `/`).
pub(crate) fn normalize_root_relative(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
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
#[non_exhaustive]
pub enum SeekFrom {
    Set,
    Cur,
    End,
}

/// A reference to a file: an open descriptor, a path, or a special
/// pseudo-file. This is the Rust-native form of the C `tc_file` tagged
/// struct (`type` + `fd` + `path`), where the variant *is* the tag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum VfFile {
    /// No file.
    #[default]
    Null,
    /// An open file descriptor (backend-assigned).
    Descriptor(i32),
    /// A path; the [`VfPathBase`] records whether it is absolute or relative
    /// to the client's cwd. Note that the raw path (see
    /// [`path`](VfFile::path)) does not itself record the base; use
    /// [`VecFs::vf_path`] to resolve a `VfFile` honoring its base.
    Path { base: VfPathBase, path: PathBuf },
    /// The client's current working directory, optionally with a relative
    /// path below it. `Current(None)` is the cwd itself (usable as a stat
    /// target, but not as a file for read/write).
    Current(Option<PathBuf>),
    /// The saved (previous) directory, mirroring the C sentinel. No current
    /// backend produces or consumes this; operations on it fail.
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

    /// The raw path, for `Path` and `Current(Some(..))`. This does not
    /// resolve `VfPathBase` or the client cwd; use [`VecFs::vf_path`] for the
    /// resolved root-relative form.
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

/// The offset of a read or write operation.
///
/// Unlike the C API's raw `u64` sentinels (`u64::MAX` / `u64::MAX - 1`), a
/// distinct type means an absolute offset can never collide with the special
/// "current position" / "end of file" meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VfOffset {
    /// An absolute file offset.
    At(u64),
    /// The current read/write position of an open descriptor.
    Cur,
    /// The end of the file.
    End,
}

/// One element of a batched read, replacing the C `tc_iovec` (which mixed
/// input and output fields in a single mutable struct).
#[derive(Debug, Clone)]
pub struct ReadOp {
    pub file: VfFile,
    pub offset: VfOffset,
    /// Number of bytes to fetch.
    pub length: usize,
}

impl ReadOp {
    pub fn new(file: VfFile, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp {
            file,
            offset,
            length,
        }
    }

    /// A read at an absolute offset.
    pub fn at(file: VfFile, offset: u64, length: usize) -> ReadOp {
        ReadOp::new(file, VfOffset::At(offset), length)
    }

    pub fn from_path(path: &str, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp::new(VfFile::from_path(path), offset, length)
    }

    /// A read from an open descriptor, typically at [`VfOffset::Cur`]
    /// (sequential) reads.
    pub fn from_fd(fd: i32, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp::new(VfFile::from_fd(fd), offset, length)
    }
}

/// The result of one [`ReadOp`]: the data and whether end-of-file was hit.
/// `file` echoes the request's file reference so results can be paired back
/// to mixed fd/path inputs without reordering assumptions.
#[derive(Debug, Clone)]
pub struct ReadResult {
    pub file: VfFile,
    /// The resolved offset the read actually started at (never `Cur`/`End`
    /// sentinels).
    pub offset: u64,
    pub data: Vec<u8>,
    /// True if the read reached end-of-file (the backend's EOF signal, or a
    /// nonzero-length read that returned fewer bytes than requested). A
    /// backend with a server EOF flag (e.g. NFS) may report `true` even when
    /// the requested length was returned exactly, because the read ended at
    /// EOF. Always false for zero-length reads.
    pub eof: bool,
}

/// One element of a batched write.
#[derive(Debug, Clone)]
pub struct WriteOp {
    pub file: VfFile,
    pub offset: VfOffset,
    pub data: Vec<u8>,
    /// Create the file if it does not exist.
    pub creation: bool,
}

impl WriteOp {
    pub fn new(file: VfFile, offset: VfOffset, data: Vec<u8>) -> WriteOp {
        WriteOp {
            file,
            offset,
            data,
            creation: false,
        }
    }

    /// A write at an absolute offset.
    pub fn at(file: VfFile, offset: u64, data: Vec<u8>) -> WriteOp {
        WriteOp::new(file, VfOffset::At(offset), data)
    }

    pub fn from_path(path: &str, offset: VfOffset, data: Vec<u8>) -> WriteOp {
        WriteOp::new(VfFile::from_path(path), offset, data)
    }

    pub fn from_fd(fd: i32, offset: VfOffset, data: Vec<u8>) -> WriteOp {
        WriteOp::new(VfFile::from_fd(fd), offset, data)
    }

    /// Create the file if it does not exist.
    pub fn with_creation(mut self) -> WriteOp {
        self.creation = true;
        self
    }
}

/// The result of one [`WriteOp`]. `file` echoes the request's file reference.
#[derive(Debug, Clone)]
pub struct WriteResult {
    pub file: VfFile,
    /// The resolved offset the write actually started at (never `Cur`/`End`
    /// sentinels).
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
    /// Requested attributes for a get/set call; also which attributes the
    /// caller cares about.
    pub masks: AttrMask,
    /// Which requested attributes were actually returned (populated by
    /// `getattrsv` / `lgetattrsv`). A field is only meaningful when its bit is
    /// set here; `masks` alone cannot distinguish "not requested" from
    /// "requested but not returned" from "returned as zero".
    pub returned: AttrMask,
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
    /// client's current working directory if it is relative), without a
    /// leading `/`. [`getcwd`](Self::getcwd) is the display form (with a
    /// leading `/`); [`vf_path`](Self::vf_path) resolves a [`VfFile`] the same
    /// way while honoring its [`VfPathBase`].
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
    /// [`AttrMask::MODE`] and [`AttrMask::SIZE`] are supported; requesting any
    /// other bit fails with [`VF_ERR_UNSUPPORTED`] at that index, and an
    /// empty mask is a no-op. Follows symlinks to the target.
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
        sort: &mut dyn FnMut(&str, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        fn rec<F: VecFs + ?Sized>(
            fs: &mut F,
            dir: &str,
            masks: AttrMask,
            sort: &mut dyn FnMut(&str, &mut Vec<VfAttrs>),
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

    /// Resolve a [`VfFile`] to its root-relative path (no leading `/`),
    /// honoring `VfPathBase::Abs`/`VfPathBase::Cwd` and the client's current
    /// working directory. Descriptors, `Null`, and `Saved` are not paths and
    /// fail with [`ERR_INVAL`]. Backends should use this when a method needs
    /// the file's path, so a `VfFile` resolves identically across all trait
    /// methods.
    fn vf_path(&self, file: &VfFile) -> VfResult<String> {
        match file {
            VfFile::Path {
                base: VfPathBase::Abs,
                path,
            } => Ok(normalize_root_relative(
                path.to_string_lossy().trim_start_matches('/'),
            )),
            VfFile::Path {
                base: VfPathBase::Cwd,
                path,
            }
            | VfFile::Current(Some(path)) => Ok(self.abs_path(&path.to_string_lossy())),
            VfFile::Current(None) => Ok(self.abs_path("")),
            VfFile::Descriptor(_) | VfFile::Null | VfFile::Saved => {
                Err(VfError::failure(0, ERR_INVAL))
            }
        }
    }

    /// Open a file by path, `tc_open()`.
    fn open(&mut self, pathname: &str, flags: i32, mode: u32) -> VfResult<VfFile> {
        self.open_by_path(VfPathBase::Cwd, pathname, flags, mode)
    }

    /// Read from a single file at an absolute offset, `tc_read()`.
    fn read(&mut self, file: &VfFile, offset: u64, length: usize) -> VfResult<Vec<u8>> {
        let mut r = self.readv(&[ReadOp::at(file.clone(), offset, length)])?;
        Ok(r.remove(0).data)
    }

    /// Write to a single file at an absolute offset, `tc_write()`.
    fn write(&mut self, file: &VfFile, offset: u64, data: &[u8]) -> VfResult<usize> {
        let mut w = self.writev(&[WriteOp::at(file.clone(), offset, data.to_vec())])?;
        Ok(w.remove(0).written)
    }

    /// Open several files at once, each with its own flags and mode,
    /// `tc_openv()`. `flags`, `modes`, and `paths` must have equal lengths;
    /// a mismatch fails with [`ERR_INVAL`] at index 0.
    fn openv(&mut self, paths: &[&str], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let mut out = Vec::with_capacity(paths.len());
        for (i, ((p, flag), mode)) in paths.iter().zip(flags).zip(modes).enumerate() {
            out.push(self.open(p, *flag, *mode).map_err(|e| e.with_index(i))?);
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
            self.close(f).map_err(|e| e.with_index(i))?;
        }
        Ok(())
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
    /// (e.g. permission denied) that are returned as `Err`. Uses
    /// `lstat` semantics: a dangling symlink exists.
    fn exists(&mut self, path: &str) -> VfResult<bool> {
        match self.lstat(path) {
            Ok(_) => Ok(true),
            Err(e) if e.err_no() == ERR_NOENT => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Return the file type of `path` itself (`lstat` semantics: a symlink
    /// reports [`VfType::Symlink`] rather than its target's type).
    fn file_type(&mut self, path: &str) -> VfResult<VfType> {
        Ok(self.lstat(path)?.ftype)
    }

    /// List directories with a callback, `tc_listdirv()`. Returning `false`
    /// stops the listing early.
    fn listdirv(
        &mut self,
        dirs: &[&str],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &str) -> bool,
    ) -> VfRes {
        for (i, d) in dirs.iter().enumerate() {
            let entries = self
                .listdir(d, masks, max_entries, recursive)
                .map_err(|e| e.with_index(i))?;
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
    fn ensure_dir(&mut self, dir: &str, mode: u32) -> VfResult<()> {
        use std::path::Component;
        let mut so_far = PathBuf::new();
        for comp in Path::new(&self.abs_path(dir)).components() {
            if let Component::Normal(part) = comp {
                so_far.push(part);
                let full = format!("/{}", so_far.to_string_lossy());
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

/// `tc_rm_recursive()`.
pub fn rm_recursive(fs: &mut impl VecFs, dir: &str) -> VfRes {
    fs.rm(&[dir], true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dummy_vecfs::DummyVecFs;
    use crate::error::RpcError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique temporary directory that is removed on drop.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> TempRoot {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "vnfs-vecfs-test-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&p);
            TempRoot(p)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fresh dummy backend rooted at a unique temp directory.
    fn fs(tag: &str) -> (TempRoot, DummyVecFs) {
        let root = TempRoot::new(tag);
        let fs = DummyVecFs::new(root.0.clone());
        (root, fs)
    }

    fn write(fs: &mut DummyVecFs, path: &str, data: &[u8]) {
        fs.writev(&[WriteOp::at(VfFile::from_path(path), 0, data.to_vec()).with_creation()])
            .expect("write");
    }

    // ------------------------------------------------------------------
    // Path helpers
    // ------------------------------------------------------------------

    #[test]
    fn split_path_helpers() {
        assert_eq!(split_path("/a/b").unwrap(), ("a", "b"));
        assert_eq!(split_path("a/b/").unwrap(), ("a", "b"));
        assert_eq!(split_path("a").unwrap(), ("", "a"));
        assert_eq!(split_path("///a//b").unwrap(), ("a", "b"));
        assert_eq!(split_path("/"), Err(ERR_NOENT));
        assert_eq!(split_path(""), Err(ERR_NOENT));
        assert_eq!(join_path("", "x"), "x");
        assert_eq!(join_path("a", "x"), "a/x");
    }

    #[test]
    fn normalize_root_relative_helper() {
        assert_eq!(normalize_root_relative(""), "");
        assert_eq!(normalize_root_relative("a"), "a");
        assert_eq!(normalize_root_relative("./a"), "a");
        assert_eq!(normalize_root_relative("a/./b/"), "a/b");
        assert_eq!(normalize_root_relative("a//b"), "a/b");
        assert_eq!(normalize_root_relative("a/../b"), "b");
        assert_eq!(normalize_root_relative("../a"), "a");
        assert_eq!(normalize_root_relative("../../a/b/../c"), "a/c");
    }

    // ------------------------------------------------------------------
    // VfError
    // ------------------------------------------------------------------

    #[test]
    fn vf_error_preserves_transport_message() {
        let e = VfError::from_rpc(RpcError::transport("connection refused"), 3);
        assert!(e.is_transport());
        assert_eq!(e.index(), 3);
        assert_eq!(e.err_no(), VF_ERR_RPC);
        assert!(e.to_string().contains("connection refused"));

        // Server status errors stay Op errors with the caller-supplied index.
        let e = VfError::from_rpc(RpcError::op(4, ERR_NOENT), 1);
        assert!(!e.is_transport());
        assert_eq!(e.index(), 1);
        assert_eq!(e.err_no(), ERR_NOENT);
    }

    #[test]
    fn vf_error_indexed_and_remap() {
        let e = VfError::from_rpc_indexed(RpcError::op(4, ERR_EXIST));
        assert_eq!((e.index(), e.err_no()), (4, ERR_EXIST));
        assert_eq!(e.with_index(9).index(), 9);
        assert_eq!(VfError::transport(2, "boom").with_index(5).index(), 5);
    }

    // ------------------------------------------------------------------
    // Offsets: no sentinel collision, Cur/End resolution, result offsets
    // ------------------------------------------------------------------

    #[test]
    fn absolute_offset_at_u64_max_minus_one_is_not_cur() {
        let (_root, mut fs) = fs("huge-offset");
        write(&mut fs, "/f", b"abcdefgh");
        let fd = fs.open("/f", 0, 0).unwrap();
        fs.fseek(&fd, 2, SeekFrom::Set).unwrap();

        // Previously u64::MAX - 1 collided with the VF_OFFSET_CUR sentinel and
        // would have read from the current position (2) instead. With the
        // typed offset it is an absolute offset: the platform may reject it
        // (pread beyond i64::MAX) or return an empty read, but never data
        // from the current position.
        match fs.readv(&[ReadOp::new(fd.clone(), VfOffset::At(u64::MAX - 1), 8)]) {
            Err(e) => assert_eq!(e.err_no(), ERR_INVAL),
            Ok(r) => {
                assert!(r[0].data.is_empty());
                assert!(r[0].eof);
            }
        }
        fs.close(&fd).unwrap();
    }

    #[test]
    fn cur_offset_reads_resolve_and_advance() {
        let (_root, mut fs) = fs("cur");
        write(&mut fs, "/f", b"hello world");
        let fd = fs.open("/f", libc::O_RDWR, 0).unwrap();
        assert_eq!(fs.fseek(&fd, 0, SeekFrom::End).unwrap(), 11);

        let w = fs
            .writev(&[WriteOp::new(fd.clone(), VfOffset::Cur, b"XY".to_vec())])
            .unwrap();
        assert_eq!(w[0].offset, 11); // resolved current position
        let w = fs
            .writev(&[WriteOp::new(fd.clone(), VfOffset::Cur, b"Z".to_vec())])
            .unwrap();
        assert_eq!(w[0].offset, 13);

        let r = fs
            .readv(&[ReadOp::new(fd.clone(), VfOffset::Cur, 100)])
            .unwrap();
        assert_eq!(r[0].offset, 14); // resolved, not the Cur sentinel
        assert!(r[0].data.is_empty());
        assert!(r[0].eof);
        fs.close(&fd).unwrap();
    }

    #[test]
    fn end_offset_writes_append_and_reads_at_end() {
        let (_root, mut fs) = fs("end");
        write(&mut fs, "/f", b"hello world");

        let fd = fs.open("/f", libc::O_RDWR, 0).unwrap();
        let w = fs
            .writev(&[WriteOp::new(fd.clone(), VfOffset::End, b"!".to_vec())])
            .unwrap();
        assert_eq!(w[0].offset, 11);
        fs.close(&fd).unwrap();

        // A read positioned at End starts at the file size, so it is at EOF.
        let r = fs
            .readv(&[ReadOp::new(VfFile::from_path("/f"), VfOffset::End, 5)])
            .unwrap();
        assert_eq!(r[0].offset, 12); // the resolved start is the file size
        assert!(r[0].data.is_empty());
        assert!(r[0].eof);

        let r = fs
            .readv(&[ReadOp::new(VfFile::from_path("/f"), VfOffset::End, 100)])
            .unwrap();
        assert!(r[0].eof);
        assert_eq!(fs.stat("/f").unwrap().size, 12);
    }

    #[test]
    fn eof_is_only_true_at_end() {
        let (_root, mut fs) = fs("eof");
        write(&mut fs, "/f", b"abc");

        let r = fs
            .readv(&[ReadOp::at(VfFile::from_path("/f"), 0, 3)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(!r[0].eof);

        let r = fs
            .readv(&[ReadOp::at(VfFile::from_path("/f"), 0, 4)])
            .unwrap();
        assert_eq!(r[0].data, b"abc");
        assert!(r[0].eof);

        // Zero-length reads never report EOF.
        let r = fs
            .readv(&[ReadOp::at(VfFile::from_path("/f"), 0, 0)])
            .unwrap();
        assert!(r[0].data.is_empty());
        assert!(!r[0].eof);
    }

    #[test]
    fn fseek_takes_shared_ref_and_works() {
        let (_root, mut fs) = fs("fseek");
        write(&mut fs, "/f", b"hello world");
        let fd = fs.open("/f", 0, 0).unwrap();

        assert_eq!(fs.fseek(&fd, 6, SeekFrom::Set).unwrap(), 6);
        let r = fs
            .readv(&[ReadOp::new(fd.clone(), VfOffset::Cur, 5)])
            .unwrap();
        assert_eq!(r[0].data, b"world");

        assert_eq!(fs.fseek(&fd, -5, SeekFrom::End).unwrap(), 6);
        assert_eq!(fs.fseek(&fd, 0, SeekFrom::Cur).unwrap(), 6);
        assert_eq!(
            fs.fseek(&fd, -100, SeekFrom::Set).unwrap_err().err_no(),
            ERR_INVAL
        );
        fs.close(&fd).unwrap();
    }

    // ------------------------------------------------------------------
    // VfFile base/cwd resolution is honored by every path-taking method
    // ------------------------------------------------------------------

    #[test]
    fn cwd_relative_unlink_targets_cwd() {
        let (_root, mut fs) = fs("cwd-unlink");
        fs.mkdir("/sub", 0o755).unwrap();
        fs.chdir("sub").unwrap();

        write(&mut fs, "a", b"x"); // cwd-relative write
        fs.unlink("a").unwrap();

        // The file was removed from sub/, not from the root.
        assert!(!fs.exists("a").unwrap());
        assert_eq!(fs.lstat("/a").unwrap_err().err_no(), ERR_NOENT);
    }

    #[test]
    fn renamev_honors_path_base() {
        let (_root, mut fs) = fs("rename-base");
        fs.mkdir("/sub", 0o755).unwrap();
        write(&mut fs, "/src", b"1");
        write(&mut fs, "/sub/src2", b"2");
        fs.chdir("sub").unwrap();

        // base Abs with a non-slash path is root-relative even after chdir.
        let abs_src = VfFile::Path {
            base: VfPathBase::Abs,
            path: PathBuf::from("src"),
        };
        let abs_dst = VfFile::Path {
            base: VfPathBase::Abs,
            path: PathBuf::from("dst"),
        };
        fs.renamev(&[(abs_src, abs_dst)]).unwrap();
        assert!(!fs.exists("/src").unwrap());
        assert!(fs.exists("/dst").unwrap());

        // base Cwd resolves against the cwd.
        let cwd_src = VfFile::Path {
            base: VfPathBase::Cwd,
            path: PathBuf::from("src2"),
        };
        let cwd_dst = VfFile::Path {
            base: VfPathBase::Cwd,
            path: PathBuf::from("dst2"),
        };
        fs.renamev(&[(cwd_src, cwd_dst)]).unwrap();
        assert!(!fs.exists("/sub/src2").unwrap());
        assert!(fs.exists("/sub/dst2").unwrap());
    }

    #[test]
    fn vf_path_rejects_descriptors() {
        let (_root, mut fs) = fs("vf-path");
        write(&mut fs, "/f", b"x");
        let fd = fs.open("/f", 0, 0).unwrap();
        assert_eq!(fs.vf_path(&fd).unwrap_err().err_no(), ERR_INVAL);
        fs.close(&fd).unwrap();
    }

    // ------------------------------------------------------------------
    // Dummy root sandbox: `..` and symlinks cannot escape the root
    // ------------------------------------------------------------------

    #[test]
    fn dummy_root_clamps_dotdot() {
        let (root, mut fs) = fs("sandbox-dotdot");

        // Writing through ".." lands inside the root, not in its parent.
        fs.writev(&[
            WriteOp::at(VfFile::from_path("/../escape"), 0, b"x".to_vec()).with_creation(),
        ])
        .unwrap();
        assert!(fs.exists("/escape").unwrap());
        assert!(!root.0.parent().unwrap().join("escape").exists());

        // "/.." and "/../../x" stay under the root.
        let st = fs.stat("/..").unwrap();
        assert_eq!(st.ftype, VfType::Directory);
        fs.writev(&[
            WriteOp::at(VfFile::from_path("/../sub1/../../sub2"), 0, b"y".to_vec()).with_creation(),
        ])
        .unwrap();
        assert!(fs.exists("/sub2").unwrap());
        assert!(!root.0.parent().unwrap().join("sub2").exists());

        // A lexical "a/../b" path resolves to b.
        write(&mut fs, "/a", b"");
        fs.renamev(&[(VfFile::from_path("/a"), VfFile::from_path("/x/../b"))])
            .unwrap();
        assert!(fs.exists("/b").unwrap());
        assert!(!fs.exists("/x").unwrap());
    }

    #[test]
    fn dummy_root_rejects_symlink_escape() {
        let (root, mut fs) = fs("sandbox-symlink");
        write(&mut fs, "/target", b"inside");

        // Absolute target outside the root: read/write through it is refused.
        let outside = root.0.parent().unwrap().join("outside-target");
        std::fs::write(&outside, b"outside").unwrap();
        fs.symlink(outside.to_str().unwrap(), "/evil").unwrap();

        assert_eq!(
            fs.readv(&[ReadOp::at(VfFile::from_path("/evil"), 0, 8)])
                .unwrap_err()
                .err_no(),
            ERR_ACCES,
            "read through escaping symlink"
        );
        assert_eq!(
            fs.writev(&[WriteOp::at(VfFile::from_path("/evil"), 0, b"x".to_vec())])
                .unwrap_err()
                .err_no(),
            ERR_ACCES,
            "write through escaping symlink"
        );

        // The link itself can still be inspected and removed (no-follow).
        assert_eq!(fs.lstat("/evil").unwrap().ftype, VfType::Symlink);
        fs.readlink("/evil").unwrap();
        fs.unlink("/evil").unwrap();

        // A dangling symlink to an outside absolute path is refused for
        // creation too, instead of creating the target outside the root.
        let dangling = root.0.parent().unwrap().join("never-created");
        fs.symlink(dangling.to_str().unwrap(), "/evil2").unwrap();
        assert_eq!(
            fs.writev(
                &[WriteOp::at(VfFile::from_path("/evil2"), 0, b"x".to_vec()).with_creation()]
            )
            .unwrap_err()
            .err_no(),
            ERR_ACCES
        );
        assert!(!dangling.exists());
        let _ = std::fs::remove_file(&outside);

        // A dangling symlink whose target is inside the root is created
        // through (POSIX O_CREAT semantics).
        fs.symlink("internal-target", "/ok-link").unwrap();
        fs.writev(&[WriteOp::at(VfFile::from_path("/ok-link"), 0, b"z".to_vec()).with_creation()])
            .unwrap();
        assert_eq!(
            fs.read(&VfFile::from_path("/internal-target"), 0, 1)
                .unwrap(),
            b"z"
        );
    }

    #[test]
    fn dummy_open_by_path_abs_is_root_relative() {
        let (_root, mut fs) = fs("open-abs");
        let fd = fs
            .open_by_path(VfPathBase::Abs, "rel", libc::O_CREAT | libc::O_RDWR, 0o644)
            .unwrap();
        fs.writev(&[WriteOp::new(fd.clone(), VfOffset::At(0), b"x".to_vec())])
            .unwrap();
        fs.close(&fd).unwrap();
        assert!(fs.exists("/rel").unwrap());
    }

    #[test]
    fn dummy_reports_special_file_types() {
        let (root, mut fs) = fs("special-types");
        let real = root.0.join("fifo");
        let c = std::ffi::CString::new(real.to_str().unwrap()).unwrap();
        unsafe { libc::mkfifo(c.as_ptr(), 0o644) };

        let sock_path = root.0.join("sock");
        let sock_c = std::ffi::CString::new(sock_path.to_str().unwrap()).unwrap();
        let fd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = sock_c.as_bytes();
            for (i, b) in bytes.iter().take(107).enumerate() {
                addr.sun_path[i] = *b as libc::c_char;
            }
            libc::bind(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            );
            fd
        };
        assert!(fd >= 0);

        assert_eq!(fs.stat("/fifo").unwrap().ftype, VfType::Fifo);
        assert_eq!(fs.lstat("/fifo").unwrap().ftype, VfType::Fifo);
        assert_eq!(fs.stat("/sock").unwrap().ftype, VfType::Socket);
        let listed = fs.listdir("/", AttrMask::default(), 0, false).unwrap();
        assert!(listed.iter().any(|e| e.ftype == VfType::Fifo));
        assert!(listed.iter().any(|e| e.ftype == VfType::Socket));

        unsafe { libc::close(fd) };
    }

    #[test]
    fn dummy_descriptor_sees_external_truncation() {
        let (root, mut fs) = fs("ext-trunc");
        write(&mut fs, "/f", b"0123456789");
        let fd = fs.open("/f", libc::O_RDWR, 0).unwrap();
        let real = root.0.join("f");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&real)
            .unwrap()
            .set_len(3)
            .unwrap();
        let r = fs
            .readv(&[ReadOp::new(fd.clone(), VfOffset::At(0), 10)])
            .unwrap();
        assert_eq!(r[0].data, b"012", "descriptor sees the new size");
        fs.close(&fd).unwrap();
    }

    #[test]
    fn dummy_cwd_dotdot_stays_in_root() {
        let (root, mut fs) = fs("cwd-dotdot");
        fs.mkdir("/a", 0o755).unwrap();
        fs.chdir("/a").unwrap();

        fs.writev(&[WriteOp::at(VfFile::from_path("../x"), 0, b"1".to_vec()).with_creation()])
            .unwrap();
        fs.writev(&[WriteOp::at(VfFile::from_path("a/../y"), 0, b"2".to_vec()).with_creation()])
            .unwrap();
        assert!(fs.exists("/x").unwrap());
        // From cwd /a, "a/../y" resolves to /a/y (the ".." cancels the "a").
        assert!(fs.exists("/a/y").unwrap());
        assert!(!fs.exists("/y").unwrap());
        assert!(!root.0.parent().unwrap().join("x").exists());
        assert!(!root.0.parent().unwrap().join("y").exists());

        // ".." from the root clamps at the root instead of escaping.
        fs.chdir("/").unwrap();
        fs.writev(&[WriteOp::at(VfFile::from_path("../z"), 0, b"3".to_vec()).with_creation()])
            .unwrap();
        assert!(fs.exists("/z").unwrap());
        assert!(!root.0.parent().unwrap().join("z").exists());
    }

    #[test]
    fn dummy_named_attr_detection() {
        use std::ffi::CString;
        let (root, mut fs) = fs("xattr");
        write(&mut fs, "/f", b"x");
        let real = root.0.join("f");
        let real = real.to_string_lossy().into_owned();
        let c = CString::new(real).unwrap();
        let name = CString::new("user.test").unwrap();
        let val = b"v";
        let rc = unsafe {
            libc::setxattr(
                c.as_ptr(),
                name.as_ptr(),
                val.as_ptr() as *const libc::c_void,
                val.len(),
                0,
            )
        };
        assert_eq!(rc, 0, "setxattr");

        let mut a = VfAttrs {
            file: VfFile::from_path("/f"),
            masks: AttrMask::NAMED_ATTR,
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut a)).unwrap();
        assert!(a.has_named_attr);
        assert!(a.returned.contains(AttrMask::NAMED_ATTR));
    }

    // ------------------------------------------------------------------
    // Attributes: returned tracking, strict setattrsv, lsetattrsv
    // ------------------------------------------------------------------

    #[test]
    fn getattrsv_reports_returned_mask() {
        let (_root, mut fs) = fs("returned");
        write(&mut fs, "/f", b"x");

        let a = fs.stat("/f").unwrap();
        assert_eq!(a.returned, AttrMask::stat());
        assert!(a.returned.contains(AttrMask::MODE));

        let mut a = VfAttrs {
            file: VfFile::from_path("/f"),
            masks: AttrMask::MODE | AttrMask::SIZE | AttrMask::MTIME,
            ..VfAttrs::default()
        };
        fs.getattrsv(std::slice::from_mut(&mut a)).unwrap();
        assert_eq!(
            a.returned,
            AttrMask::MODE | AttrMask::SIZE | AttrMask::MTIME
        );
    }

    #[test]
    fn setattrsv_rejects_unsupported_bits() {
        let (_root, mut fs) = fs("setattr-strict");
        write(&mut fs, "/f", b"x");

        let mut a = VfAttrs {
            file: VfFile::from_path("/f"),
            masks: AttrMask::MTIME,
            mtime_sec: 1,
            ..VfAttrs::default()
        };
        assert_eq!(
            fs.setattrsv(std::slice::from_ref(&a)).unwrap_err().err_no(),
            VF_ERR_UNSUPPORTED
        );

        a.masks = AttrMask::MODE | AttrMask::MTIME;
        assert_eq!(
            fs.setattrsv(std::slice::from_ref(&a)).unwrap_err().err_no(),
            VF_ERR_UNSUPPORTED
        );

        // MODE-only still works.
        a.masks = AttrMask::MODE;
        a.mode = 0o640;
        fs.setattrsv(std::slice::from_ref(&a)).unwrap();
        assert_eq!(fs.lstat("/f").unwrap().mode & 0o7777, 0o640);
    }

    #[test]
    fn lsetattrsv_does_not_follow_symlinks() {
        let (_root, mut fs) = fs("lsetattr");
        write(&mut fs, "/target", b"x");
        fs.symlink("/target", "/link").unwrap();

        // No portable lchmod: the dummy backend refuses symlinks instead of
        // silently following them.
        let a = VfAttrs {
            file: VfFile::from_path("/link"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        assert_eq!(
            fs.lsetattrsv(std::slice::from_ref(&a))
                .unwrap_err()
                .err_no(),
            VF_ERR_UNSUPPORTED
        );

        // Regular files are set normally.
        let a = VfAttrs {
            file: VfFile::from_path("/target"),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        fs.lsetattrsv(std::slice::from_ref(&a)).unwrap();
        assert_eq!(fs.lstat("/target").unwrap().mode & 0o7777, 0o600);
    }

    // ------------------------------------------------------------------
    // exists / file_type use lstat semantics
    // ------------------------------------------------------------------

    #[test]
    fn exists_and_file_type_use_lstat_semantics() {
        let (_root, mut fs) = fs("lstat");
        write(&mut fs, "/f", b"x");
        fs.symlink("missing-target", "/dangling").unwrap();

        assert!(fs.exists("/dangling").unwrap());
        assert_eq!(fs.file_type("/dangling").unwrap(), VfType::Symlink);
        assert_eq!(fs.file_type("/f").unwrap(), VfType::Regular);
    }

    // ------------------------------------------------------------------
    // openv length contract, listdir limits, walk via dyn VecFs
    // ------------------------------------------------------------------

    #[test]
    fn openv_rejects_mismatched_lengths() {
        let (_root, mut fs) = fs("openv");
        use libc::O_CREAT;
        let e = fs.openv(&["/a", "/b"], &[O_CREAT], &[0o644]).unwrap_err();
        assert_eq!((e.index(), e.err_no()), (0, ERR_INVAL));
    }

    #[test]
    fn listdir_zero_max_count_is_unlimited() {
        let (_root, mut fs) = fs("listdir");
        fs.mkdir("/d", 0o755).unwrap();
        write(&mut fs, "/d/a", b"1");
        write(&mut fs, "/d/b", b"2");

        let all = fs.listdir("/d", AttrMask::default(), 0, false).unwrap();
        assert_eq!(all.len(), 2);
        let one = fs.listdir("/d", AttrMask::default(), 1, false).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn walk_works_through_dyn_vecfs() {
        let (_root, mut fs) = fs("walk-dyn");
        fs.mkdir("/sub", 0o755).unwrap();
        write(&mut fs, "/sub/a", b"1");

        let mut dyn_fs: Box<dyn VecFs> = Box::new(fs);
        let mut visited: Vec<String> = Vec::new();
        let entries = dyn_fs
            .walk("", AttrMask::stat(), &mut |dir, _| {
                visited.push(dir.to_string())
            })
            .unwrap();
        assert_eq!(visited.len(), 2); // root + /sub
        assert_eq!(entries.len(), 2);
        let sub = entries.iter().find(|w| w.path.ends_with("sub")).unwrap();
        assert_eq!(sub.entries.len(), 1);
        assert_eq!(sub.entries[0].ftype, VfType::Regular);
    }

    #[test]
    fn lcopyv_copies_symlinks_as_symlinks() {
        let (_root, mut fs) = fs("lcopyv");
        write(&mut fs, "/target", b"data");
        fs.symlink("target", "/link").unwrap();

        let pair = ExtentPair::new("/link", 0, "/link-copy", 0, u64::MAX);
        fs.lcopyv(std::slice::from_ref(&pair)).unwrap();
        assert_eq!(fs.file_type("/link-copy").unwrap(), VfType::Symlink);
        assert_eq!(
            fs.readlink("/link-copy").unwrap(),
            fs.readlink("/link").unwrap()
        );

        // dupv copies the target's data instead.
        let pair = ExtentPair::new("/link", 0, "/link-dup", 0, u64::MAX);
        fs.dupv(std::slice::from_ref(&pair)).unwrap();
        assert_eq!(fs.file_type("/link-dup").unwrap(), VfType::Regular);
        assert_eq!(
            fs.read(&VfFile::from_path("/link-dup"), 0, 4).unwrap(),
            b"data"
        );
    }
}
