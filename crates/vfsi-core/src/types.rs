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

use crate::error::{RpcError, TransportKind};
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
pub const ERR_IO: u32 = 5;
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
/// index, or a transport / client-side failure (where the index is optional
/// because some failures cannot be attributed to one request).
/// An indexed failure does not roll back an already-completed prefix, and a
/// transport failure can make the outcome of an in-flight mutation ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VfError {
    /// A filesystem status failure; `err_no` is errno-style or an NFS4ERR
    /// code, mirroring the C `tc_res` struct.
    Op {
        index: usize,
        err_no: u32,
        domain: ErrorDomain,
        operation: Option<&'static str>,
        path: Option<PathBuf>,
    },
    /// A filesystem / protocol status that cannot be attributed to a specific
    /// operation index (for example a compound-level failure with no per-op
    /// result). Unlike [`Op`](VfError::Op), it does not pretend the failure is
    /// operation 0. [`index`](VfError::index) reports `None`.
    OpUnattributed {
        err_no: u32,
        domain: ErrorDomain,
        operation: Option<&'static str>,
        path: Option<PathBuf>,
    },
    /// A transport / client-side failure with a human-readable message; there
    /// is no filesystem status ([`err_no`](VfError::err_no) reports
    /// [`VF_ERR_RPC`]). `index` is `None` when the failure cannot be
    /// attributed to any operation.
    Transport {
        index: Option<usize>,
        kind: TransportKind,
        message: String,
        operation: Option<&'static str>,
        path: Option<PathBuf>,
    },
}

/// Namespace in which a status code is meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorDomain {
    Filesystem,
    Nfs,
    Smb,
    Transport,
    Client,
}

/// Typed protocol/filesystem status. Transport failures have no status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatusCode {
    Errno(u32),
    Nfs(u32),
    Smb(u32),
    Client(u32),
}

impl VfError {
    pub fn failure(index: usize, err_no: u32) -> VfError {
        VfError::Op {
            index,
            err_no,
            domain: ErrorDomain::Filesystem,
            operation: None,
            path: None,
        }
    }

    pub fn client(index: usize, code: u32) -> VfError {
        VfError::Op {
            index,
            err_no: code,
            domain: ErrorDomain::Client,
            operation: None,
            path: None,
        }
    }

    /// A status returned by an NFS server.
    pub fn nfs(index: usize, status: u32) -> VfError {
        VfError::Op {
            index,
            err_no: status,
            domain: ErrorDomain::Nfs,
            operation: None,
            path: None,
        }
    }

    /// A status returned by an SMB server.
    pub fn smb(index: usize, status: u32) -> VfError {
        VfError::Op {
            index,
            err_no: status,
            domain: ErrorDomain::Smb,
            operation: None,
            path: None,
        }
    }

    /// A transport / client-side failure. `index` is best-effort; pass `None`
    /// when the failure cannot be attributed to a specific operation.
    pub fn transport(index: impl Into<Option<usize>>, message: impl Into<String>) -> VfError {
        Self::transport_with_kind(index, TransportKind::Other, message)
    }

    pub fn transport_with_kind(
        index: impl Into<Option<usize>>,
        kind: TransportKind,
        message: impl Into<String>,
    ) -> VfError {
        VfError::Transport {
            index: index.into(),
            kind,
            message: message.into(),
            operation: None,
            path: None,
        }
    }

    pub fn unsupported(index: usize) -> VfError {
        VfError::failure(index, VF_ERR_UNSUPPORTED)
    }

    /// Logical request index, or `None` when a failure cannot be attributed.
    /// An index is not a completion boundary: this item can itself contain
    /// completed chunks, and other requests can already have changed state.
    pub fn index(&self) -> Option<usize> {
        match self {
            VfError::Op { index, .. } => Some(*index),
            VfError::OpUnattributed { .. } => None,
            VfError::Transport { index, .. } => *index,
        }
    }

    /// Raw compatibility code: errno, NFS status, SMB status, or [`VF_ERR_RPC`].
    /// Application code should use [`kind`](Self::kind) or typed
    /// [`status`](Self::status), not compare this value to errno constants.
    pub fn err_no(&self) -> u32 {
        match self {
            VfError::Op { err_no, .. } | VfError::OpUnattributed { err_no, .. } => *err_no,
            VfError::Transport { .. } => VF_ERR_RPC,
        }
    }

    /// Whether this is a transport / client-side failure (no filesystem
    /// status).
    pub fn is_transport(&self) -> bool {
        matches!(self, VfError::Transport { .. })
    }

    /// Transport provenance only; this never authorizes mutation replay.
    pub fn transport_kind(&self) -> Option<TransportKind> {
        match self {
            Self::Transport { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Re-attribute this error to a different operation index.
    pub fn with_index(self, index: usize) -> VfError {
        match self {
            VfError::Op {
                err_no,
                domain,
                operation,
                path,
                ..
            }
            | VfError::OpUnattributed {
                err_no,
                domain,
                operation,
                path,
            } => VfError::Op {
                index,
                err_no,
                domain,
                operation,
                path,
            },
            VfError::Transport {
                kind,
                message,
                operation,
                path,
                ..
            } => VfError::Transport {
                index: Some(index),
                kind,
                message,
                operation,
                path,
            },
        }
    }

    /// Transform a known request index while preserving an unattributable
    /// transport failure as `None`.
    pub fn map_index(self, map: impl FnOnce(usize) -> usize) -> VfError {
        match self.index() {
            Some(index) => self.with_index(map(index)),
            None => self,
        }
    }

    /// Attach operation and path context without string parsing.
    pub fn with_context(mut self, operation: &'static str, path: impl Into<PathBuf>) -> Self {
        match &mut self {
            VfError::Op {
                operation: op,
                path: p,
                ..
            }
            | VfError::OpUnattributed {
                operation: op,
                path: p,
                ..
            }
            | VfError::Transport {
                operation: op,
                path: p,
                ..
            } => {
                *op = Some(operation);
                *p = Some(path.into());
            }
        }
        self
    }

    pub fn domain(&self) -> ErrorDomain {
        match self {
            VfError::Op { domain, .. } | VfError::OpUnattributed { domain, .. } => *domain,
            VfError::Transport { .. } => ErrorDomain::Transport,
        }
    }

    pub fn status(&self) -> Option<StatusCode> {
        let (err_no, domain) = match self {
            VfError::Op { err_no, domain, .. } | VfError::OpUnattributed { err_no, domain, .. } => {
                (*err_no, *domain)
            }
            VfError::Transport { .. } => return None,
        };
        Some(match domain {
            ErrorDomain::Nfs => StatusCode::Nfs(err_no),
            ErrorDomain::Smb => StatusCode::Smb(err_no),
            ErrorDomain::Client => StatusCode::Client(err_no),
            _ => StatusCode::Errno(err_no),
        })
    }

    pub fn operation(&self) -> Option<&'static str> {
        match self {
            VfError::Op { operation, .. }
            | VfError::OpUnattributed { operation, .. }
            | VfError::Transport { operation, .. } => *operation,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            VfError::Op { path, .. }
            | VfError::OpUnattributed { path, .. }
            | VfError::Transport { path, .. } => path.as_deref(),
        }
    }
}

impl std::fmt::Display for VfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VfError::Op {
                index,
                err_no,
                operation,
                path,
                ..
            } => {
                write!(f, "op {}", index)?;
                if let Some(operation) = operation {
                    write!(f, " ({operation})")?;
                }
                if let Some(path) = path {
                    write!(f, " for {}", path.display())?;
                }
                write!(
                    f,
                    " failed: {} ({:?} status {})",
                    std::io::Error::from(self.kind()),
                    self.domain(),
                    err_no
                )
            }
            VfError::OpUnattributed {
                err_no,
                operation,
                path,
                ..
            } => {
                if let Some(operation) = operation {
                    write!(f, "{operation}")?;
                } else {
                    write!(f, "operation")?;
                }
                if let Some(path) = path {
                    write!(f, " for {}", path.display())?;
                }
                write!(
                    f,
                    " failed: {} ({:?} status {}; operation index unknown)",
                    std::io::Error::from(self.kind()),
                    self.domain(),
                    err_no
                )
            }
            VfError::Transport {
                index: Some(index),
                message,
                operation,
                path,
                ..
            } => {
                write!(f, "op {index}")?;
                if let Some(operation) = operation {
                    write!(f, " ({operation})")?;
                }
                if let Some(path) = path {
                    write!(f, " for {}", path.display())?;
                }
                write!(f, " transport error: {message}")
            }
            VfError::Transport {
                index: None,
                message,
                operation,
                path,
                ..
            } => {
                if let Some(operation) = operation {
                    write!(f, "{operation} ")?;
                }
                if let Some(path) = path {
                    write!(f, "for {} ", path.display())?;
                }
                write!(f, "transport error: {}", message)
            }
        }
    }
}

impl std::error::Error for VfError {}

/// Convert an RPC error using an explicitly chosen logical request index.
/// `None` keeps the failure unattributed; it never invents index zero.
/// Transport errors retain their transport category rather than becoming a
/// filesystem status. The RPC's wire-operation index is not used here.
pub fn error_from_rpc(e: RpcError, index: impl Into<Option<usize>>) -> VfError {
    if e.is_transport() {
        VfError::Transport {
            index: index.into(),
            kind: e.transport_kind.unwrap_or(TransportKind::Other),
            message: e.message,
            operation: None,
            path: None,
        }
    } else {
        match index.into() {
            Some(index) => VfError::Op {
                index,
                err_no: e.status,
                domain: ErrorDomain::Nfs,
                operation: None,
                path: None,
            },
            // A status with no known operation index must not masquerade
            // as operation 0. Preserve it as unattributed.
            None => VfError::OpUnattributed {
                err_no: e.status,
                domain: ErrorDomain::Nfs,
                operation: None,
                path: None,
            },
        }
    }
}

/// Convert a semantic RPC failure using its wire-operation index.
/// Transport failures have no trustworthy operation index and remain
/// unattributed. Callers mapping compound operations to logical requests
/// should instead use `error_from_rpc` with their mapped index.
pub fn error_from_rpc_indexed(e: RpcError) -> VfError {
    if e.is_transport() {
        return crate::error_from_rpc(e, None);
    }
    let idx = e.op_index;
    crate::error_from_rpc(e, Some(idx))
}

impl From<VfError> for std::io::Error {
    fn from(error: VfError) -> Self {
        std::io::Error::new(error.kind(), error)
    }
}

pub type VfResult<T> = Result<T, VfError>;
/// Result of a compound-style operation: `()` on success, or the index and
/// error of the first failing operation.
pub type VfRes = VfResult<()>;

impl VfError {
    /// Portable category; [`Self::status`] retains the original status.
    ///
    /// This is deliberately not retry guidance. Even a semantic failure can
    /// follow successful chunks of the same logical mutation. Reconcile state
    /// before replaying a mutation unless its idempotence is established.
    pub fn kind(&self) -> std::io::ErrorKind {
        use std::io::ErrorKind as K;
        // A general Connection failure does not identify a specific I/O
        // errno; retain Other rather than fabricate Refused/Reset/Aborted.
        if self.transport_kind() == Some(TransportKind::Timeout) {
            return K::TimedOut;
        }
        if self.transport_kind() == Some(TransportKind::InvalidReply) {
            return K::InvalidData;
        }
        if self.transport_kind() == Some(TransportKind::Authentication) {
            return K::PermissionDenied;
        }
        if matches!(
            self.status(),
            Some(StatusCode::Errno(VF_ERR_UNSUPPORTED) | StatusCode::Client(VF_ERR_UNSUPPORTED))
        ) {
            return K::Unsupported;
        }
        match self.status() {
            Some(StatusCode::Errno(code) | StatusCode::Client(code)) => {
                std::io::Error::from_raw_os_error(code as i32).kind()
            }
            Some(StatusCode::Nfs(code)) => match code {
                1 | 13 => K::PermissionDenied,
                2 => K::NotFound,
                17 => K::AlreadyExists,
                20 => K::NotADirectory,
                21 => K::IsADirectory,
                22 => K::InvalidInput,
                27 => K::FileTooLarge,
                28 => K::StorageFull,
                30 => K::ReadOnlyFilesystem,
                63 => K::InvalidFilename,
                66 => K::DirectoryNotEmpty,
                10004 => K::Unsupported,
                10008 => K::WouldBlock,
                _ => K::Other,
            },
            Some(StatusCode::Smb(code)) => match code {
                0xc000_0022 => K::PermissionDenied,
                0xc000_0034 | 0xc000_003a => K::NotFound,
                0xc000_0035 => K::AlreadyExists,
                0xc000_000d => K::InvalidInput,
                0xc000_007f => K::StorageFull,
                0xc000_00bb => K::Unsupported,
                0xc000_0101 => K::DirectoryNotEmpty,
                0xc000_0103 => K::NotADirectory,
                0xc000_00ba => K::IsADirectory,
                _ => K::Other,
            },
            None => K::Other,
        }
    }
}

/// Callback used by vectorized streaming reads.
pub type ReadStreamCallback<'a> = dyn FnMut(usize, u64, &[u8], bool) -> bool + 'a;

/// An open file descriptor (backend-assigned), the Rust spelling of the C
/// `int` fd.
pub type Fd = std::os::fd::RawFd;

/// Opaque identifier for a backend-owned open file.
///
/// New APIs use this instead of exposing `RawFd`, which could be confused
/// with a process file descriptor. The legacy compatibility API continues to
/// use [`Fd`] until its next breaking release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandleId(Fd);

impl HandleId {
    #[doc(hidden)]
    pub fn from_legacy(fd: Fd) -> Self {
        Self(fd)
    }

    #[doc(hidden)]
    pub fn as_legacy(self) -> Fd {
        self.0
    }
}

bitflags::bitflags! {
    /// Typed file-open behavior for the Rust-native API.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct OpenFlags: u32 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const APPEND = 1 << 2;
        const TRUNCATE = 1 << 3;
        const CREATE = 1 << 4;
        const CREATE_NEW = 1 << 5;
    }
}

/// Translate the typed flags for compatibility backends.
pub fn open_flags_to_libc(flags: OpenFlags) -> VfResult<i32> {
    let writable = flags.intersects(OpenFlags::WRITE | OpenFlags::APPEND);
    let mut raw = match (flags.contains(OpenFlags::READ), writable) {
        (true, true) => libc::O_RDWR,
        (true, false) => libc::O_RDONLY,
        (false, true) => libc::O_WRONLY,
        (false, false) => return Err(VfError::failure(0, ERR_INVAL)),
    };
    if flags.contains(OpenFlags::TRUNCATE) && !writable {
        return Err(VfError::failure(0, ERR_INVAL));
    }
    if flags.intersects(OpenFlags::CREATE | OpenFlags::CREATE_NEW) && !writable {
        return Err(VfError::failure(0, ERR_INVAL));
    }
    if flags.contains(OpenFlags::APPEND) {
        raw |= libc::O_APPEND;
    }
    if flags.contains(OpenFlags::TRUNCATE) {
        raw |= libc::O_TRUNC;
    }
    if flags.intersects(OpenFlags::CREATE | OpenFlags::CREATE_NEW) {
        raw |= libc::O_CREAT;
    }
    if flags.contains(OpenFlags::CREATE_NEW) {
        raw |= libc::O_EXCL;
    }
    Ok(raw)
}

/// One complete open request. This replaces three error-prone parallel
/// slices of paths, flags, and modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOp {
    pub path: PathBuf,
    pub flags: OpenFlags,
    pub mode: u32,
}

impl OpenOp {
    pub fn new(path: impl Into<PathBuf>, flags: OpenFlags) -> Self {
        Self {
            path: path.into(),
            flags,
            mode: 0o666,
        }
    }

    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
}

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

pub fn file_type_from_nfs(code: u32) -> VfType {
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
pub fn file_type_to_nfs(file_type: &VfType) -> u32 {
    match file_type {
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

impl From<u64> for VfOffset {
    fn from(offset: u64) -> Self {
        Self::At(offset)
    }
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
    /// True if the backend determined that the read reached end-of-file. A
    /// short read alone does not imply EOF. A
    /// backend with a server EOF flag (e.g. NFS) may report `true` even when
    /// the requested length was returned exactly, because the read ended at
    /// EOF. Always false for zero-length reads.
    pub eof: bool,
}

/// Metadata for a read placed directly into caller-provided storage.
#[derive(Debug, Clone)]
pub struct ReadIntoResult {
    pub file: VfFile,
    pub offset: u64,
    pub read: usize,
    pub eof: bool,
}

/// The result of one [`crate::WriteOp`]. `file` echoes the request's file reference.
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

/// Rust-native permission bits, independent of protocol wire attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Permissions {
    mode: u32,
}

impl Permissions {
    pub fn from_mode(mode: u32) -> Self {
        Self {
            mode: mode & 0o7777,
        }
    }

    pub fn mode(self) -> u32 {
        self.mode
    }

    pub fn readonly(self) -> bool {
        self.mode & 0o222 == 0
    }
}

/// Idiomatic metadata returned by the Rust-native path API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attrs {
    file_type: VfType,
    len: u64,
    permissions: Permissions,
    returned: AttrMask,
    mode: Option<u32>,
    blocks: Option<u64>,
    device_id: Option<u64>,
    has_named_attributes: Option<bool>,
    modified: Option<std::time::SystemTime>,
    accessed: Option<std::time::SystemTime>,
    changed: Option<std::time::SystemTime>,
    nlink: Option<u32>,
    file_id: Option<u64>,
    change_id: Option<u64>,
    uid: Option<u32>,
    gid: Option<u32>,
}

impl Attrs {
    /// Attribute fields actually returned by the backend, not merely requested.
    ///
    /// Optional accessors remain `None` when a field was not requested or the
    /// backend could not provide it. Check this mask before projecting the
    /// metadata into an API that requires concrete values (such as `stat`).
    pub fn returned_attributes(&self) -> AttrMask {
        self.returned
    }

    /// Requested fields that were not returned by the backend.
    pub fn missing_attributes(&self, requested: AttrMask) -> AttrMask {
        requested.difference(self.returned)
    }

    /// Whether every field in `requested` was returned by the backend.
    pub fn has_attributes(&self, requested: AttrMask) -> bool {
        self.missing_attributes(requested).is_empty()
    }

    /// Full POSIX mode, including the file type bits, when returned.
    pub fn mode(&self) -> Option<u32> {
        self.mode
    }

    /// Number of allocated 512-byte blocks, when returned.
    pub fn blocks(&self) -> Option<u64> {
        self.blocks
    }

    /// Device identifier for a special file, when returned.
    pub fn device_id(&self) -> Option<u64> {
        self.device_id
    }

    /// Whether named attributes exist, when the backend supplied this field.
    pub fn has_named_attributes(&self) -> Option<bool> {
        self.has_named_attributes
    }

    pub fn file_type(&self) -> VfType {
        self.file_type
    }

    pub fn is_file(&self) -> bool {
        self.file_type == VfType::Regular
    }

    pub fn is_dir(&self) -> bool {
        self.file_type == VfType::Directory
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type == VfType::Symlink
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn permissions(&self) -> Permissions {
        self.permissions
    }

    pub fn modified(&self) -> Option<std::time::SystemTime> {
        self.modified
    }

    pub fn accessed(&self) -> Option<std::time::SystemTime> {
        self.accessed
    }

    /// POSIX status-change time; this is not a creation timestamp.
    pub fn changed(&self) -> Option<std::time::SystemTime> {
        self.changed
    }

    pub fn nlink(&self) -> Option<u32> {
        self.nlink
    }

    pub fn file_id(&self) -> Option<u64> {
        self.file_id
    }

    pub fn change_id(&self) -> Option<u64> {
        self.change_id
    }

    pub fn uid(&self) -> Option<u32> {
        self.uid
    }

    pub fn gid(&self) -> Option<u32> {
        self.gid
    }
}

fn system_time(seconds: i64, nanos: u32) -> Option<std::time::SystemTime> {
    if seconds < 0 {
        std::time::UNIX_EPOCH
            .checked_sub(std::time::Duration::from_secs(seconds.unsigned_abs()))?
            .checked_add(std::time::Duration::from_nanos(u64::from(nanos)))
    } else {
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(seconds as u64, nanos))
    }
}

/// Backend attribute conversion; absent fields remain absent.
pub fn metadata_from_attrs(attributes: VfAttrs) -> Attrs {
    let returned = attributes.returned;
    Attrs {
        file_type: attributes.ftype,
        len: attributes.size,
        permissions: Permissions::from_mode(attributes.mode),
        returned,
        mode: returned.contains(AttrMask::MODE).then_some(attributes.mode),
        blocks: returned
            .contains(AttrMask::BLOCKS)
            .then_some(attributes.blocks),
        device_id: returned.contains(AttrMask::RDEV).then_some(attributes.rdev),
        has_named_attributes: returned
            .contains(AttrMask::NAMED_ATTR)
            .then_some(attributes.has_named_attr),
        modified: returned
            .contains(AttrMask::MTIME)
            .then(|| system_time(attributes.mtime_sec, attributes.mtime_nsec))
            .flatten(),
        accessed: returned
            .contains(AttrMask::ATIME)
            .then(|| system_time(attributes.atime_sec, attributes.atime_nsec))
            .flatten(),
        changed: returned
            .contains(AttrMask::CTIME)
            .then(|| system_time(attributes.ctime_sec, attributes.ctime_nsec))
            .flatten(),
        nlink: returned
            .contains(AttrMask::NLINK)
            .then_some(attributes.nlink),
        file_id: returned
            .contains(AttrMask::FILEID)
            .then_some(attributes.fileid),
        change_id: returned
            .contains(AttrMask::CHANGE)
            .then_some(attributes.change),
        uid: returned.contains(AttrMask::UID).then_some(attributes.uid),
        gid: returned.contains(AttrMask::GID).then_some(attributes.gid),
    }
}

/// One native directory entry with its already-fetched metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    path: PathBuf,
    metadata: Attrs,
}

impl DirEntry {
    pub fn new(path: PathBuf, metadata: Attrs) -> Self {
        Self { path, metadata }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_name(&self) -> Option<&std::ffi::OsStr> {
        self.path.file_name()
    }

    pub fn file_type(&self) -> VfType {
        self.metadata.file_type()
    }

    pub fn attrs(&self) -> &Attrs {
        &self.metadata
    }
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

bitflags::bitflags! {
    /// Typed backend capabilities. Unlike the legacy integer constants, this
    /// rejects accidental mixing with unrelated bit fields.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct Capabilities: u64 {
        const SERVER_COPY = VF_CAP_SERVER_COPY;
        const POSIX_METADATA = VF_CAP_POSIX_METADATA;
        const SYMLINKS = VF_CAP_SYMLINKS;
        const HARDLINKS = VF_CAP_HARDLINKS;
        const NON_UTF8_PATHS = VF_CAP_NON_UTF8_PATHS;
        const LSTAT = VF_CAP_LSTAT;
        const UNIX_SEMANTICS = VF_CAP_UNIX_SEMANTICS;
    }
}

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct RemoveFlags {
    continue_on_error: bool,
    #[bits(7)]
    _reserved: u8,
}

/// Options controlling recursive removal.
///
/// The `continue_on_error`/`retries` behavior was previously hardcoded in the
/// NFS backend; exposing it lets callers choose GNU `rm -r` semantics (delete
/// as much as possible, report the first error) or fail fast. Backends using
/// the generic remover support only the default options and reject others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOptions {
    /// Keep removing after a per-entry failure and report the first error at
    /// the end. When `false`, the first failure aborts.
    flags: RemoveFlags,
    /// Maximum batch size for vectorized removals; `0` lets the backend learn
    /// a safe size from useful work, starting at a conservative default.
    pub batch: usize,
    /// Bounded retries for retryable per-entry statuses.
    pub retries: u32,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self {
            flags: RemoveFlags::new(),
            batch: 0,
            retries: 4,
        }
    }
}

impl RemoveOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn continue_on_error(mut self, value: bool) -> Self {
        self.flags.set_continue_on_error(value);
        self
    }

    /// Whether removal continues after semantic per-entry failures.
    pub const fn continues_on_error(self) -> bool {
        self.flags.continue_on_error()
    }

    pub fn batch(mut self, value: usize) -> Self {
        self.batch = value;
        self
    }

    pub fn retries(mut self, value: u32) -> Self {
        self.retries = value;
        self
    }
}

/// An opaque handle to an open directory.
///
/// It exists so recursive removal can be rooted at an already-resolved
/// directory instead of a path. Path-based entry points re-resolve the path and
/// are therefore subject to the classic entry-point TOCTOU race: a concurrent
/// actor can replace a path component with a symlink between the caller naming
/// the path and the removal starting. A handle removes that race for the root
/// of the removal (the same guarantee `remove_dir_all`'s `RemoveDir` trait
/// gives). Backends without directory handles return [`VfDir::Path`], which
/// carries no such guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VfDir {
    /// A backend-assigned directory handle, bound to the client that opened it.
    Descriptor { fd: Fd, owner: u64 },
    /// A backend without directory handles; the removal re-resolves this path.
    Path(PathBuf),
}

/// Filesystem capacity and limits for the filesystem containing a target.
/// Values are observations, not reservations. `None` means unknown or unsupported.
/// Byte counts use the filesystem allocation unit, not its preferred I/O size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilesystemStats {
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// Space available to an unprivileged caller (may exclude reserved blocks).
    pub available_bytes: Option<u64>,
    pub total_files: Option<u64>,
    pub free_files: Option<u64>,
    pub available_files: Option<u64>,
    /// Preferred I/O block size.
    pub block_size: Option<u64>,
    /// Allocation unit used by capacity counts.
    pub fragment_size: Option<u64>,
    pub max_name_len: Option<u64>,
    pub max_path_len: Option<u64>,
    pub max_links: Option<u64>,
    pub max_file_size: Option<u64>,
    /// POSIX FILESIZEBITS; this is not necessarily the filesystem's file-size limit.
    pub file_size_bits: Option<u32>,
    pub read_only: Option<bool>,
    pub no_set_id: Option<bool>,
}

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct CopyFlags {
    #[bits(default = true)]
    follow_source_symlinks: bool,
    #[bits(7)]
    _reserved: u8,
}

/// Options for whole-file copies through [`crate::api::Vfsi::vcopy`].
/// Source final symlinks are followed by default; ancestor symlinks use normal
/// namespace resolution. This is not a snapshot. When preserving a source
/// symlink, its link text is copied and an existing destination is not replaced.
/// Destination symlinks for data copies retain normal following behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyOption {
    flags: CopyFlags,
}
impl CopyOption {
    pub fn new() -> Self {
        Self::default()
    }
    /// Follow final source symlinks when true; otherwise recreate the links.
    pub fn follow_source_symlinks(mut self, follow: bool) -> Self {
        self.flags.set_follow_source_symlinks(follow);
        self
    }
    pub fn follows_source_symlinks(self) -> bool {
        self.flags.follow_source_symlinks()
    }
}

#[cfg(test)]
mod copy_option_tests {
    use super::CopyOption;
    #[test]
    fn follows_source_symlinks_by_default_and_builder_can_disable_it() {
        assert!(CopyOption::default().follows_source_symlinks());
        let preserve = CopyOption::new().follow_source_symlinks(false);
        assert!(!preserve.follows_source_symlinks());
        assert!(
            preserve
                .follow_source_symlinks(true)
                .follows_source_symlinks()
        );
    }
}
