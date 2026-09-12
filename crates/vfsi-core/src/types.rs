//! Shared operation and result types used by the VFSI interface facets.
//!
//! # Path and name representation
//!
//! Paths are native Unix [`Path`]s. An absolute path starts with `/` and is
//! resolved against the filesystem root (for NFS, the export root), while a
//! relative path is resolved against the client's current working directory
//! `PathBuf`/`OsStr` preserve arbitrary filename bytes; UTF-8 conversion is a
//! convenience for callers that need it.

use std::path::{Path, PathBuf};

use crate::error::RpcError;
#[cfg(unix)]
use crate::path::path_from_bytes;

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
    /// [`VF_ERR_RPC`]). `index` is `None` when the failure cannot be
    /// attributed to any operation.
    Transport {
        index: Option<usize>,
        message: String,
    },
}

impl VfError {
    pub fn failure(index: usize, err_no: u32) -> VfError {
        VfError::Op { index, err_no }
    }

    /// A transport / client-side failure. `index` is best-effort; pass `None`
    /// when the failure cannot be attributed to a specific operation.
    pub fn transport(index: impl Into<Option<usize>>, message: impl Into<String>) -> VfError {
        VfError::Transport {
            index: index.into(),
            message: message.into(),
        }
    }

    pub fn unsupported(index: usize) -> VfError {
        VfError::failure(index, VF_ERR_UNSUPPORTED)
    }

    /// The operation index this error refers to (best-effort for transport
    /// failures; 0 when unknown).
    pub fn index(&self) -> usize {
        match self {
            VfError::Op { index, .. } => *index,
            VfError::Transport { index, .. } => index.unwrap_or(0),
        }
    }

    /// The operation index as an `Option`; `None` only for a transport
    /// failure that cannot be attributed to any operation.
    pub fn index_opt(&self) -> Option<usize> {
        match self {
            VfError::Op { index, .. } => Some(*index),
            VfError::Transport { index, .. } => *index,
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
    pub fn from_rpc(e: RpcError, index: impl Into<Option<usize>>) -> VfError {
        if e.is_transport() {
            VfError::Transport {
                index: index.into(),
                message: e.message,
            }
        } else {
            VfError::Op {
                index: index.into().unwrap_or(0),
                err_no: e.status,
            }
        }
    }

    /// Like [`from_rpc`](VfError::from_rpc), but trusts `e.op_index` as the
    /// caller-relative operation index. The batched client helpers translate
    /// compound positions to caller indices before returning, so batched
    /// backend calls can use this directly.
    pub fn from_rpc_indexed(e: RpcError) -> VfError {
        let idx = e.op_index;
        VfError::from_rpc(e, Some(idx))
    }

    /// Re-attribute this error to a different operation index.
    pub fn with_index(self, index: usize) -> VfError {
        match self {
            VfError::Op { err_no, .. } => VfError::Op { index, err_no },
            VfError::Transport { message, .. } => VfError::Transport {
                index: Some(index),
                message,
            },
        }
    }
}

impl std::fmt::Display for VfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VfError::Op { index, err_no } => write!(f, "op {} failed: {}", index, err_no),
            VfError::Transport {
                index: Some(index),
                message,
            } => write!(f, "op {} transport error: {}", index, message),
            VfError::Transport {
                index: None,
                message,
            } => {
                write!(f, "transport error: {}", message)
            }
        }
    }
}

impl std::error::Error for VfError {}

pub type VfResult<T> = Result<T, VfError>;
/// Result of a compound-style operation: `()` on success, or the index and
/// error of the first failing operation.
pub type VfRes = VfResult<()>;

/// Callback used by vectorized streaming reads.
pub type ReadStreamCallback<'a> = dyn FnMut(usize, u64, &[u8], bool) -> bool + 'a;

/// An open file descriptor (backend-assigned), the Rust spelling of the C
/// `int` fd.
pub type Fd = std::os::fd::RawFd;

/// Insert an open object using a positive descriptor, wrapping safely and
/// skipping live descriptors instead of overwriting them.
#[doc(hidden)]
pub fn insert_fd<T>(
    next_fd: &mut Fd,
    open_files: &mut std::collections::HashMap<Fd, T>,
    open: T,
) -> VfResult<Fd> {
    for _ in 0..i32::MAX {
        let candidate = next_fd.checked_add(1).unwrap_or(1);
        *next_fd = candidate;
        if let std::collections::hash_map::Entry::Vacant(entry) = open_files.entry(candidate) {
            entry.insert(open);
            return Ok(candidate);
        }
    }
    Err(VfError::failure(0, libc::EMFILE as u32))
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

/// How filesystem seek operations interpret their offset, mirroring `SEEK_SET` /
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
    /// The client's current working directory (a stat target, but not a file
    /// for read/write). Also the `Default` placeholder.
    #[default]
    Cwd,
    /// An open file descriptor (backend-assigned).
    Descriptor(Fd),
    /// A path; the [`VfPathBase`] records whether it is absolute or relative
    /// to the client's cwd. Note that the raw path (see
    /// [`path`](VfFile::path)) does not itself record the base; the filesystem
    /// resolves it while honoring this field.
    Path { base: VfPathBase, path: PathBuf },
    /// A path relative to the client's current working directory.
    CwdPath(PathBuf),
    /// The saved (previous) directory, mirroring the C sentinel. No current
    /// backend produces or consumes this; operations on it fail.
    Saved,
}

impl VfFile {
    pub fn from_path(path: &str) -> VfFile {
        Self::from_path_bytes(path.as_bytes())
    }

    /// Construct from arbitrary Unix path bytes.
    #[cfg(unix)]
    pub fn from_path_bytes(path: &[u8]) -> VfFile {
        let base = if path.starts_with(b"/") {
            VfPathBase::Abs
        } else {
            VfPathBase::Cwd
        };
        VfFile::Path {
            base,
            path: path_from_bytes(path),
        }
    }

    /// Construct from a native Unix path.
    pub fn from_os_path(path: &Path) -> VfFile {
        Self::from_path_bytes(crate::path::path_bytes(path))
    }

    pub fn from_fd(fd: Fd) -> VfFile {
        VfFile::Descriptor(fd)
    }

    /// VF_FILE_CURRENT: the client's current working directory itself.
    pub fn cwd() -> VfFile {
        VfFile::Cwd
    }

    /// A path relative to the client's current working directory.
    pub fn cwd_path(path: impl Into<PathBuf>) -> VfFile {
        VfFile::CwdPath(path.into())
    }

    pub fn saved() -> VfFile {
        VfFile::Saved
    }

    /// Whether this references an open descriptor.
    pub fn is_descriptor(&self) -> bool {
        matches!(self, VfFile::Descriptor(_))
    }

    /// The open descriptor, if this is a `Descriptor`.
    pub fn fd(&self) -> Option<Fd> {
        match self {
            VfFile::Descriptor(fd) => Some(*fd),
            _ => None,
        }
    }

    /// The raw path, for `Path` and `CwdPath`. This does not resolve
    /// `VfPathBase` or the client working directory.
    pub fn path(&self) -> Option<&Path> {
        match self {
            VfFile::Path { path, .. } => Some(path),
            VfFile::CwdPath(p) => Some(p),
            _ => None,
        }
    }

    /// The raw bytes of this path, if path-backed.
    #[cfg(unix)]
    pub fn path_bytes(&self) -> Option<&[u8]> {
        self.path().map(crate::path::path_bytes)
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

    /// Construct a read from a native path.
    pub fn from_os_path(path: &Path, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp::new(VfFile::from_os_path(path), offset, length)
    }

    /// A read from an open descriptor, typically at [`VfOffset::Cur`]
    /// (sequential) reads.
    pub fn from_fd(fd: Fd, offset: VfOffset, length: usize) -> ReadOp {
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
    /// Truncate the file to zero before writing (POSIX `O_TRUNC` semantics
    /// fused into the same round trip as the write).
    pub truncate: bool,
}

impl WriteOp {
    pub fn new(file: VfFile, offset: VfOffset, data: Vec<u8>) -> WriteOp {
        WriteOp {
            file,
            offset,
            data,
            creation: false,
            truncate: false,
        }
    }

    /// A write at an absolute offset.
    pub fn at(file: VfFile, offset: u64, data: Vec<u8>) -> WriteOp {
        WriteOp::new(file, VfOffset::At(offset), data)
    }

    pub fn from_path(path: &str, offset: VfOffset, data: Vec<u8>) -> WriteOp {
        WriteOp::new(VfFile::from_path(path), offset, data)
    }

    /// Construct a write from a native path.
    pub fn from_os_path(path: &Path, offset: VfOffset, data: Vec<u8>) -> WriteOp {
        WriteOp::new(VfFile::from_os_path(path), offset, data)
    }

    pub fn from_fd(fd: Fd, offset: VfOffset, data: Vec<u8>) -> WriteOp {
        WriteOp::new(VfFile::from_fd(fd), offset, data)
    }

    /// Create the file if it does not exist.
    pub fn with_creation(mut self) -> WriteOp {
        self.creation = true;
        self
    }

    /// Truncate the file to zero before writing, in the same round trip.
    pub fn with_truncate(mut self) -> WriteOp {
        self.truncate = true;
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
    pub src_path: PathBuf,
    pub dst_path: PathBuf,
    pub src_offset: u64,
    pub dst_offset: u64,
    /// Bytes to copy; `None` means "from src_offset to end of file".
    pub length: Option<u64>,
}

impl ExtentPair {
    /// `tc_fill_extent_pair()`.
    pub fn new(
        src_path: &str,
        src_offset: u64,
        dst_path: &str,
        dst_offset: u64,
        length: Option<u64>,
    ) -> ExtentPair {
        ExtentPair {
            src_path: PathBuf::from(src_path),
            dst_path: PathBuf::from(dst_path),
            src_offset,
            dst_offset,
            length,
        }
    }

    /// Construct from native paths.
    pub fn from_os_paths(
        src_path: &Path,
        src_offset: u64,
        dst_path: &Path,
        dst_offset: u64,
        length: Option<u64>,
    ) -> ExtentPair {
        ExtentPair {
            src_path: src_path.to_path_buf(),
            dst_path: dst_path.to_path_buf(),
            src_offset,
            dst_offset,
            length,
        }
    }
}

/// An Application Data Block (ADB) pattern, mirroring `struct tc_adb`.
#[derive(Debug, Clone)]
pub struct Adb {
    pub path: PathBuf,
    pub adb_offset: u64,
    pub adb_block_size: u64,
    /// Blocks to write.
    pub adb_block_count: usize,
    /// Relative offset within a block to write the ADBN; `None` = no ADBN.
    pub adb_reloff_blocknum: Option<u64>,
    /// ADBN of the first ADB.
    pub adb_block_num: u64,
    /// Relative offset within a block to write the pattern; `None` = no
    /// pattern. The pattern bytes are `adb_pattern_data`.
    pub adb_reloff_pattern: Option<u64>,
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
            path: PathBuf::from(path),
            adb_offset: offset,
            adb_block_size: block_size,
            adb_block_count: block_count,
            adb_reloff_blocknum: Some(reloff_blocknum),
            adb_block_num: first_adbn,
            adb_reloff_pattern: None,
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
        /// Request the backend's monotonic file change attribute when one is
        /// available (NFS FATTR4_CHANGE). This is stronger than timestamps for
        /// cache invalidation because rapid, same-size writes still change it.
        const CHANGE = 1 << 12;
    }
}

impl AttrMask {
    /// The attributes a scalar stat operation needs (mode, size, links, fileid).
    pub fn stat() -> AttrMask {
        AttrMask::MODE | AttrMask::SIZE | AttrMask::NLINK | AttrMask::FILEID
    }
}

/// A directory and its entries, as returned by a filesystem walk. `path` is the
/// directory's root-relative path; `entries` are its immediate children.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    pub path: PathBuf,
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
    /// Backend file-version token. NFS supplies FATTR4_CHANGE; other
    /// backends leave this absent by not setting [`AttrMask::CHANGE`] in
    /// `returned`.
    pub change: u64,
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

/// Capability bit returned by a backend's capability query when it will
/// currently attempt a server-side copy operation (NFSv4.2 COPY or SMB
/// FSCTL_SRV_COPYCHUNK).
pub const VF_CAP_SERVER_COPY: u64 = 1 << 0;
/// The backend reports and honors Unix mode/ownership/link-count metadata.
pub const VF_CAP_POSIX_METADATA: u64 = 1 << 1;
/// The backend can create, inspect, and copy symbolic links without following
/// them.
pub const VF_CAP_SYMLINKS: u64 = 1 << 2;
/// The backend can create hard links and report their shared identity.
pub const VF_CAP_HARDLINKS: u64 = 1 << 3;
/// The backend accepts paths containing arbitrary non-UTF-8 Unix bytes.
pub const VF_CAP_NON_UTF8_PATHS: u64 = 1 << 4;
/// The backend implements no-follow metadata operations (`lstat` semantics).
pub const VF_CAP_LSTAT: u64 = 1 << 5;

/// Capabilities shared by the complete Unix-like NFS and dummy backends.
pub const VF_CAP_UNIX_SEMANTICS: u64 = VF_CAP_POSIX_METADATA
    | VF_CAP_SYMLINKS
    | VF_CAP_HARDLINKS
    | VF_CAP_NON_UTF8_PATHS
    | VF_CAP_LSTAT;
