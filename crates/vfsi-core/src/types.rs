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
///
/// Construct errors through the typed factories and inspect them through methods;
/// status domains and transport provenance cannot be modified independently.
///
/// ```compile_fail
/// let mut error = vfsi_core::VfError::failure(0, 2);
/// error.index = Some(9);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfError {
    index: Option<usize>,
    detail: ErrorDetail,
    operation: Option<&'static str>,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ErrorDetail {
    Status(StatusCode),
    Transport {
        kind: TransportKind,
        message: String,
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
    /// A typed status, with an optional logical request index.
    pub fn from_status(index: impl Into<Option<usize>>, status: StatusCode) -> Self {
        Self {
            index: index.into(),
            detail: ErrorDetail::Status(status),
            operation: None,
            path: None,
        }
    }

    pub fn failure(index: usize, err_no: u32) -> VfError {
        Self::from_status(index, StatusCode::Errno(err_no))
    }

    pub fn client(index: usize, code: u32) -> VfError {
        Self::from_status(index, StatusCode::Client(code))
    }

    /// A status returned by an NFS server.
    pub fn nfs(index: usize, status: u32) -> VfError {
        Self::from_status(index, StatusCode::Nfs(status))
    }

    /// A status returned by an SMB server.
    pub fn smb(index: usize, status: u32) -> VfError {
        Self::from_status(index, StatusCode::Smb(status))
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
        Self {
            index: index.into(),
            detail: ErrorDetail::Transport {
                kind,
                message: message.into(),
            },
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
        self.index
    }

    /// Raw code in the error's status domain, or [`VF_ERR_RPC`] for transport.
    /// Application code should use [`kind`](Self::kind) or typed
    /// [`status`](Self::status), not compare this value to errno constants.
    pub fn raw_code(&self) -> u32 {
        match self.status() {
            Some(
                StatusCode::Errno(code)
                | StatusCode::Nfs(code)
                | StatusCode::Smb(code)
                | StatusCode::Client(code),
            ) => code,
            None => VF_ERR_RPC,
        }
    }

    /// Whether this is a transport failure with no filesystem status.
    pub fn is_transport(&self) -> bool {
        matches!(&self.detail, ErrorDetail::Transport { .. })
    }

    /// Transport provenance only; this never authorizes mutation replay.
    pub fn transport_kind(&self) -> Option<TransportKind> {
        match &self.detail {
            ErrorDetail::Transport { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// The backend's transport diagnostic, without formatted index/path context.
    pub fn transport_message(&self) -> Option<&str> {
        match &self.detail {
            ErrorDetail::Transport { message, .. } => Some(message),
            _ => None,
        }
    }

    /// Re-attribute this error to a different operation index.
    pub fn with_index(mut self, index: usize) -> VfError {
        self.index = Some(index);
        self
    }

    /// Transform a known request index while preserving an unattributable
    /// failure as `None`.
    pub fn map_index(mut self, map: impl FnOnce(usize) -> usize) -> VfError {
        self.index = self.index.map(map);
        self
    }

    /// Attach operation and path context without string parsing.
    pub fn with_context(mut self, operation: &'static str, path: impl Into<PathBuf>) -> Self {
        self.operation = Some(operation);
        self.path = Some(path.into());
        self
    }

    pub fn domain(&self) -> ErrorDomain {
        match self.status() {
            Some(StatusCode::Errno(_)) => ErrorDomain::Filesystem,
            Some(StatusCode::Nfs(_)) => ErrorDomain::Nfs,
            Some(StatusCode::Smb(_)) => ErrorDomain::Smb,
            Some(StatusCode::Client(_)) => ErrorDomain::Client,
            None => ErrorDomain::Transport,
        }
    }

    pub fn status(&self) -> Option<StatusCode> {
        match &self.detail {
            ErrorDetail::Status(status) => Some(*status),
            ErrorDetail::Transport { .. } => None,
        }
    }

    pub fn operation(&self) -> Option<&'static str> {
        self.operation
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl std::fmt::Display for VfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.index {
            Some(index) => {
                write!(f, "op {index}")?;
                if let Some(operation) = self.operation {
                    write!(f, " ({operation})")?;
                }
                if let Some(path) = &self.path {
                    write!(f, " for {}", path.display())?;
                }
            }
            None if self.is_transport() => {
                if let Some(operation) = self.operation {
                    write!(f, "{operation} ")?;
                }
                if let Some(path) = &self.path {
                    write!(f, "for {} ", path.display())?;
                }
            }
            None => {
                write!(f, "{}", self.operation.unwrap_or("operation"))?;
                if let Some(path) = &self.path {
                    write!(f, " for {}", path.display())?;
                }
            }
        }
        if let Some(message) = self.transport_message() {
            if self.index.is_some() {
                write!(f, " ")?;
            }
            write!(f, "transport error: {message}")?;
        } else {
            write!(
                f,
                " failed: {} ({:?} status {}",
                std::io::Error::from(self.kind()),
                self.domain(),
                self.raw_code(),
            )?;
            if self.index.is_none() {
                write!(f, "; operation index unknown")?;
            }
            write!(f, ")")?;
        }
        match self.status() {
            Some(StatusCode::Nfs(10019)) => {
                write!(f, "; {}", crate::error::NFS_REFERRAL_UNSUPPORTED)
            }
            Some(StatusCode::Smb(0xc000_0257)) => write!(
                f,
                "; SMB DFS referrals are unsupported; connect directly to the destination server/share"
            ),
            _ => Ok(()),
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
        VfError::transport_with_kind(
            index,
            e.transport_kind.unwrap_or(TransportKind::Other),
            e.message,
        )
    } else {
        VfError::from_status(index, StatusCode::Nfs(e.status))
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
        if self.transport_kind() == Some(TransportKind::Auth) {
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
                10004 | 10019 => K::Unsupported,
                10008 => K::WouldBlock,
                _ => K::Other,
            },
            Some(StatusCode::Smb(code)) => match code {
                0xc000_0022 => K::PermissionDenied,
                0xc000_0034 | 0xc000_003a => K::NotFound,
                0xc000_0035 => K::AlreadyExists,
                0xc000_000d => K::InvalidInput,
                0xc000_007f => K::StorageFull,
                0xc000_00bb | 0xc000_0257 => K::Unsupported,
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

/// Backend-assigned numeric File Descriptor, represented as a C `int`.
///
/// Local backends can use OS descriptors; remote backends use client-side
/// slots. The number is scoped to its owning backend and does not own the open
/// instance or identify the underlying filesystem object.
pub type Fd = std::os::fd::RawFd;

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
    if flags.bits() & !OpenFlags::all().bits() != 0 {
        return Err(VfError::failure(0, ERR_INVAL));
    }
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
#[must_use = "configuration and requests have no effect until used"]
pub struct OpenOp<'a> {
    path: std::borrow::Cow<'a, Path>,
    flags: OpenFlags,
    mode: u32,
}

impl<'a> OpenOp<'a> {
    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }
    /// Extract an owned path. Borrowed paths are copied; owned storage is moved.
    pub fn into_path(self) -> PathBuf {
        self.path.into_owned()
    }
    /// Retain this request independently of the source pathname.
    /// An already-owned request transfers its allocation without copying.
    pub fn into_owned(self) -> OpenOp<'static> {
        OpenOp {
            path: std::borrow::Cow::Owned(self.path.into_owned()),
            flags: self.flags,
            mode: self.mode,
        }
    }
    pub const fn flags(&self) -> OpenFlags {
        self.flags
    }
    /// Requested creation permissions, before applying the backend's umask.
    pub const fn creation_mode(&self) -> u32 {
        self.mode
    }
    /// Borrow a pathname without allocating. Use into_owned() for a stored plan
    /// that must outlive its source paths.
    pub fn new<P: AsRef<Path> + ?Sized>(path: &'a P, flags: OpenFlags) -> Self {
        Self {
            path: std::borrow::Cow::Borrowed(path.as_ref()),
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

/// Native backend operand: a File Descriptor, path, or special directory target.
///
/// This selects an operation target without owning its open instance. Cloning
/// a File Descriptor operand neither duplicates the open instance nor acquires
/// cleanup ownership. Application callers use [`crate::api::Target`] instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileOperand {
    /// The client's current working directory (a stat target, but not a file
    /// for read/write). Also the `Default` placeholder.
    #[default]
    Cwd,
    /// Backend-assigned File Descriptor referring to an existing open instance.
    Fd(Fd),
    /// A path; the [`VfPathBase`] records whether it is absolute or relative
    /// to the client's cwd. Note that the raw path (see
    /// [`path`](FileOperand::path)) does not itself record the base; the filesystem
    /// resolves it while honoring this field.
    Path { base: VfPathBase, path: PathBuf },
    /// A path relative to the client's current working directory.
    CwdPath(PathBuf),
    /// The saved (previous) directory, mirroring the C sentinel. No current
    /// backend produces or consumes this; operations on it fail.
    Saved,
}

impl FileOperand {
    pub fn from_path(path: &str) -> FileOperand {
        Self::from_path_bytes(path.as_bytes())
    }

    /// Construct from arbitrary Unix path bytes.
    #[cfg(unix)]
    pub fn from_path_bytes(path: &[u8]) -> FileOperand {
        let base = if path.starts_with(b"/") {
            VfPathBase::Abs
        } else {
            VfPathBase::Cwd
        };
        FileOperand::Path {
            base,
            path: path_from_bytes(path),
        }
    }

    /// Construct from a native Unix path.
    pub fn from_os_path(path: &Path) -> FileOperand {
        Self::from_path_bytes(crate::path::path_bytes(path))
    }

    pub fn from_fd(fd: Fd) -> FileOperand {
        FileOperand::Fd(fd)
    }

    /// VF_FILE_CURRENT: the client's current working directory itself.
    pub fn cwd() -> FileOperand {
        FileOperand::Cwd
    }

    /// A path relative to the client's current working directory.
    pub fn cwd_path(path: impl Into<PathBuf>) -> FileOperand {
        FileOperand::CwdPath(path.into())
    }

    pub fn saved() -> FileOperand {
        FileOperand::Saved
    }

    /// Whether this references an open File Descriptor.
    pub fn is_fd(&self) -> bool {
        matches!(self, FileOperand::Fd(_))
    }

    /// The File Descriptor, if this operand is [`Self::Fd`].
    pub fn fd(&self) -> Option<Fd> {
        match self {
            FileOperand::Fd(fd) => Some(*fd),
            _ => None,
        }
    }

    /// The raw path, for `Path` and `CwdPath`. This does not resolve
    /// `VfPathBase` or the client working directory.
    pub fn path(&self) -> Option<&Path> {
        match self {
            FileOperand::Path { path, .. } => Some(path),
            FileOperand::CwdPath(p) => Some(p),
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
    pub file: FileOperand,
    pub offset: VfOffset,
    /// Number of bytes to fetch.
    pub length: usize,
}

impl ReadOp {
    pub fn new(file: FileOperand, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp {
            file,
            offset,
            length,
        }
    }

    /// A read at an absolute offset.
    pub fn at(file: FileOperand, offset: u64, length: usize) -> ReadOp {
        ReadOp::new(file, VfOffset::At(offset), length)
    }

    pub fn from_path(path: &str, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp::new(FileOperand::from_path(path), offset, length)
    }

    /// Construct a read from a native path.
    pub fn from_os_path(path: &Path, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp::new(FileOperand::from_os_path(path), offset, length)
    }

    /// A read from an open descriptor, typically at [`VfOffset::Cur`]
    /// (sequential) reads.
    pub fn from_fd(fd: Fd, offset: VfOffset, length: usize) -> ReadOp {
        ReadOp::new(FileOperand::from_fd(fd), offset, length)
    }
}

/// The result of one [`ReadOp`]: the data and whether end-of-file was hit.
/// `file` echoes the request's file reference so results can be paired back
/// to mixed fd/path inputs without reordering assumptions.
#[derive(Debug, Clone)]
pub struct ReadResult {
    pub file: FileOperand,
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
    pub file: FileOperand,
    pub offset: u64,
    pub bytes_read: usize,
    pub eof: bool,
}

/// The result of one [`crate::WriteOp`]. `file` echoes the request's file reference.
#[derive(Debug, Clone)]
pub struct WriteResult {
    pub file: FileOperand,
    /// The resolved offset the write actually started at (never `Cur`/`End`
    /// sentinels).
    pub offset: u64,
    pub bytes_written: usize,
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
        /// Request the backend's filesystem identity (POSIX st_dev or the
        /// NFS filesystem ID). Pair with FILEID to identify an object.
        const FS_ID = 1 << 13;
    }
}

impl AttrMask {
    /// The attributes a scalar stat operation needs, including a filesystem-
    /// scoped object identity when the backend provides one.
    pub fn stat() -> AttrMask {
        AttrMask::MODE | AttrMask::SIZE | AttrMask::NLINK | AttrMask::FILEID | AttrMask::FS_ID
    }

    /// Request both parts of an underlying filesystem object identity.
    pub fn identity() -> AttrMask {
        AttrMask::FILEID | AttrMask::FS_ID
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
    pub file: FileOperand,
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
    /// Opaque identity for the containing filesystem. Compare together with
    /// fileid, and only within the backend's identity namespace. This is not
    /// a mount ID or the special-file device number stored in `rdev`.
    pub fs_id: u128,
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

/// Filesystem-scoped identifier for an underlying filesystem object.
///
/// This combines a filesystem ID and that filesystem's file ID. It is suitable
/// for comparing objects reported by the same backend identity namespace; it
/// is not a global ID across servers or unrelated backend clients. IDs can be
/// reused after deletion, so this is not a durable globally unique identifier.
/// It does not identify an open instance; see [`crate::api::FileHandle`] for that.
///
/// Request both components before comparing underlying objects:
///
/// ```
/// use vfsi_core::{api::AttrsOptions, AttrMask, FsObjId, VfResult, Vfsi, VfsiExt};
///
/// fn object_id<C: Vfsi>(fs: &C) -> VfResult<Option<FsObjId>> {
///     let attrs = fs.attrs_with_options(
///         "/file",
///         AttrsOptions::new().with_fields(AttrMask::identity()),
///     )?;
///     Ok(attrs.fs_obj_id())
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FsObjId {
    fs_id: u128,
    file_id: u64,
}

impl FsObjId {
    /// Construct a pair obtained from one backend/server identity namespace.
    /// This does not make numeric IDs globally unique across unrelated servers.
    pub const fn new(fs_id: u128, file_id: u64) -> Self {
        Self { fs_id, file_id }
    }

    /// Filesystem namespace component; Linux st_dev or packed NFS FSID.
    /// This is independent of the mount used to reach the object.
    pub const fn fs_id(self) -> u128 {
        self.fs_id
    }

    /// File-number component, meaningful within [`Self::fs_id`] and its
    /// backend/server identity namespace (Linux st_ino or NFS FILEID).
    pub const fn file_id(self) -> u64 {
        self.file_id
    }
}

/// Idiomatic metadata returned by the Rust-native path API.
#[derive(Clone, PartialEq, Eq)]
pub struct Attrs {
    file_type: VfType,
    len: u64,
    permissions: Permissions,
    returned: AttrMask,
    // Integer presence is already encoded by `returned`. Keep niche-optimized
    // timestamp/bool Options: a returned timestamp can still fail conversion.
    mode: u32,
    blocks: u64,
    device_id: u64,
    has_named_attributes: Option<bool>,
    modified: Option<std::time::SystemTime>,
    accessed: Option<std::time::SystemTime>,
    changed: Option<std::time::SystemTime>,
    nlink: u32,
    fs_id: u128,
    file_id: u64,
    change_id: u64,
    uid: u32,
    gid: u32,
}

impl Attrs {
    /// Attribute fields actually returned by the backend, not merely requested.
    ///
    /// Optional accessors remain `None` when a field was not requested or the
    /// backend could not provide it. Check this mask before projecting the
    /// metadata into an API that requires concrete values (such as `stat`).
    pub fn present_mask(&self) -> AttrMask {
        self.returned
    }

    /// Fields in `requested` that are absent from [`Self::present_mask`].
    /// Unknown requested bits are preserved in the result.
    pub fn absent_mask(&self, requested: AttrMask) -> AttrMask {
        requested.difference(self.returned)
    }

    /// Whether every field in `requested` is present, not merely any field.
    /// An empty mask is always contained.
    pub fn contains(&self, requested: AttrMask) -> bool {
        self.absent_mask(requested).is_empty()
    }

    /// Full POSIX mode, including the file type bits, when returned.
    pub fn mode(&self) -> Option<u32> {
        self.returned.contains(AttrMask::MODE).then_some(self.mode)
    }

    /// Number of allocated 512-byte blocks, when returned.
    pub fn blocks(&self) -> Option<u64> {
        self.returned
            .contains(AttrMask::BLOCKS)
            .then_some(self.blocks)
    }

    /// Device identifier for a special file, when returned.
    pub fn device_id(&self) -> Option<u64> {
        self.returned
            .contains(AttrMask::RDEV)
            .then_some(self.device_id)
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

    /// Logical size, when SIZE was returned. Missing is distinct from zero.
    pub fn len(&self) -> Option<u64> {
        self.returned.contains(AttrMask::SIZE).then_some(self.len)
    }

    pub fn is_empty(&self) -> Option<bool> {
        self.len().map(|len| len == 0)
    }

    /// Permission bits, when MODE was returned. Missing is distinct from mode zero.
    pub fn permissions(&self) -> Option<Permissions> {
        self.returned
            .contains(AttrMask::MODE)
            .then_some(self.permissions)
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
        self.returned
            .contains(AttrMask::NLINK)
            .then_some(self.nlink)
    }

    /// Backend file number, when returned; combine with [`Self::fs_id`]
    /// rather than comparing this number across filesystems.
    pub fn file_id(&self) -> Option<u64> {
        self.returned
            .contains(AttrMask::FILEID)
            .then_some(self.file_id)
    }

    /// Backend filesystem ID, when returned. Pair it with file_id; it is
    /// not globally unique across servers or unrelated backend clients. NFS
    /// packs the FSID major part in the high 64 bits and minor part in the low.
    pub fn fs_id(&self) -> Option<u128> {
        self.returned
            .contains(AttrMask::FS_ID)
            .then_some(self.fs_id)
    }

    /// The underlying filesystem object ID when both components were returned.
    pub fn fs_obj_id(&self) -> Option<FsObjId> {
        Some(FsObjId {
            fs_id: self.fs_id()?,
            file_id: self.file_id()?,
        })
    }

    pub fn change_id(&self) -> Option<u64> {
        self.returned
            .contains(AttrMask::CHANGE)
            .then_some(self.change_id)
    }

    pub fn uid(&self) -> Option<u32> {
        self.returned.contains(AttrMask::UID).then_some(self.uid)
    }

    pub fn gid(&self) -> Option<u32> {
        self.returned.contains(AttrMask::GID).then_some(self.gid)
    }
}

impl std::fmt::Debug for Attrs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attrs")
            .field("file_type", &self.file_type)
            .field("len", &self.len)
            .field("permissions", &self.permissions)
            .field("returned", &self.returned)
            .field("mode", &self.mode())
            .field("blocks", &self.blocks())
            .field("device_id", &self.device_id())
            .field("has_named_attributes", &self.has_named_attributes())
            .field("modified", &self.modified())
            .field("accessed", &self.accessed())
            .field("changed", &self.changed())
            .field("nlink", &self.nlink())
            .field("fs_id", &self.fs_id())
            .field("file_id", &self.file_id())
            .field("change_id", &self.change_id())
            .field("uid", &self.uid())
            .field("gid", &self.gid())
            .finish()
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
    // Keep absent integer payloads canonical so derived equality preserves
    // the old Option semantics, independent of unreturned backend values.
    macro_rules! integer {
        ($mask:ident, $field:ident) => {
            if returned.contains(AttrMask::$mask) {
                attributes.$field
            } else {
                0
            }
        };
    }
    Attrs {
        file_type: attributes.ftype,
        len: attributes.size,
        permissions: Permissions::from_mode(attributes.mode),
        returned,
        mode: integer!(MODE, mode),
        blocks: integer!(BLOCKS, blocks),
        device_id: integer!(RDEV, rdev),
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
        nlink: integer!(NLINK, nlink),
        fs_id: integer!(FS_ID, fs_id),
        file_id: integer!(FILEID, fileid),
        change_id: integer!(CHANGE, change),
        uid: integer!(UID, uid),
        gid: integer!(GID, gid),
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
#[must_use = "configuration and requests have no effect until used"]
pub struct RemoveOptions {
    /// Keep removing after a per-entry failure and report the first error at
    /// the end. When `false`, the first failure aborts.
    flags: RemoveFlags,
    /// Maximum batch size for vectorized removals; `0` lets the backend learn
    /// a safe size from useful work, starting at a conservative default.
    batch: usize,
    /// Bounded retries for retryable per-entry statuses.
    retries: u32,
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
    /// Configured batch size; zero allows backend adaptation.
    pub const fn batch_size(self) -> usize {
        self.batch
    }
    /// Retry cap; zero disables retries.
    pub const fn retries(self) -> u32 {
        self.retries
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_continue_on_error(mut self, value: bool) -> Self {
        self.flags.set_continue_on_error(value);
        self
    }

    /// Whether removal continues after semantic per-entry failures.
    pub const fn continue_on_error(self) -> bool {
        self.flags.continue_on_error()
    }

    pub fn with_batch_size(mut self, value: usize) -> Self {
        self.batch = value;
        self
    }

    pub fn with_retries(mut self, value: u32) -> Self {
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
/// Getters preserve zero and false as known values. Fluent `with_*` setters
/// accept `None` to restore an unknown value.
///
/// ```
/// use vfsi_core::FilesystemStats;
/// let stats = FilesystemStats::default()
///     .with_free_bytes(Some(0))
///     .with_read_only(Some(false));
/// assert_eq!(stats.free_bytes(), Some(0));
/// assert_eq!(stats.read_only(), Some(false));
/// assert_eq!(stats.total_bytes(), None);
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct FilesystemStats {
    total_bytes: u64,
    free_bytes: u64,
    available_bytes: u64,
    total_files: u64,
    free_files: u64,
    available_files: u64,
    block_size: u64,
    fragment_size: u64,
    max_name_len: u64,
    max_path_len: u64,
    max_links: u64,
    max_file_size: u64,
    file_size_bits: u32,
    flags: FilesystemStatsFlags,
}

#[bitfields::bitfield(u32)]
#[derive(PartialEq, Eq)]
struct FilesystemStatsFlags {
    #[bits(15)]
    present: u16,
    read_only: bool,
    no_set_id: bool,
    #[bits(15)]
    _reserved: u16,
}

// One presence bit per logical field. Unknown payloads are always zeroed so
// derived equality is semantic and independent of a value's update history.
macro_rules! filesystem_stat_field {
    ($(#[$doc:meta])* $field:ident, $setter:ident, $ty:ty, $bit:literal) => {
        $(#[$doc])*
        pub const fn $field(&self) -> Option<$ty> {
            if self.flags.present() & (1 << $bit) != 0 {
                Some(self.$field)
            } else {
                None
            }
        }
        #[doc = concat!("Set `", stringify!($field), "`; `None` marks it unknown.")]
        pub fn $setter(mut self, value: Option<$ty>) -> Self {
            self.set_present($bit, value.is_some());
            self.$field = value.unwrap_or_default();
            self
        }
    };
}

impl FilesystemStats {
    fn set_present(&mut self, bit: u32, present: bool) {
        let mask = 1 << bit;
        let bits = self.flags.present() & !mask;
        self.flags
            .set_present(bits | if present { mask } else { 0 });
    }

    filesystem_stat_field!(/// Total capacity in bytes.
        total_bytes, with_total_bytes, u64, 0);
    filesystem_stat_field!(/// Free capacity in bytes, including reserved space.
        free_bytes, with_free_bytes, u64, 1);
    filesystem_stat_field!(/// Space available to an unprivileged caller (may exclude reserved blocks).
        available_bytes, with_available_bytes, u64, 2);
    filesystem_stat_field!(/// Total file count.
        total_files, with_total_files, u64, 3);
    filesystem_stat_field!(/// Free file count.
        free_files, with_free_files, u64, 4);
    filesystem_stat_field!(/// File count available to an unprivileged caller.
        available_files, with_available_files, u64, 5);
    filesystem_stat_field!(/// Preferred I/O block size in bytes.
        block_size, with_block_size, u64, 6);
    filesystem_stat_field!(/// Allocation unit used by capacity counts, in bytes.
        fragment_size, with_fragment_size, u64, 7);
    filesystem_stat_field!(/// Maximum filename length in bytes.
        max_name_len, with_max_name_len, u64, 8);
    filesystem_stat_field!(/// Maximum path length in bytes.
        max_path_len, with_max_path_len, u64, 9);
    filesystem_stat_field!(/// Maximum number of hard links.
        max_links, with_max_links, u64, 10);
    filesystem_stat_field!(/// Maximum file size in bytes.
        max_file_size, with_max_file_size, u64, 11);
    filesystem_stat_field!(/// POSIX FILESIZEBITS; not necessarily the filesystem's file-size limit.
        file_size_bits, with_file_size_bits, u32, 12);

    /// Whether the filesystem is read-only, when known.
    pub const fn read_only(&self) -> Option<bool> {
        if self.flags.present() & (1 << 13) != 0 {
            Some(self.flags.read_only())
        } else {
            None
        }
    }
    /// Set read-only status; `None` marks it unknown.
    pub fn with_read_only(mut self, value: Option<bool>) -> Self {
        self.set_present(13, value.is_some());
        self.flags.set_read_only(value.unwrap_or_default());
        self
    }
    /// Whether set-user-ID and set-group-ID bits are ignored, when known.
    pub const fn no_set_id(&self) -> Option<bool> {
        if self.flags.present() & (1 << 14) != 0 {
            Some(self.flags.no_set_id())
        } else {
            None
        }
    }
    /// Set the no-set-ID status; `None` marks it unknown.
    pub fn with_no_set_id(mut self, value: Option<bool>) -> Self {
        self.set_present(14, value.is_some());
        self.flags.set_no_set_id(value.unwrap_or_default());
        self
    }
}

impl std::fmt::Debug for FilesystemStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemStats")
            .field("total_bytes", &self.total_bytes())
            .field("free_bytes", &self.free_bytes())
            .field("available_bytes", &self.available_bytes())
            .field("total_files", &self.total_files())
            .field("free_files", &self.free_files())
            .field("available_files", &self.available_files())
            .field("block_size", &self.block_size())
            .field("fragment_size", &self.fragment_size())
            .field("max_name_len", &self.max_name_len())
            .field("max_path_len", &self.max_path_len())
            .field("max_links", &self.max_links())
            .field("max_file_size", &self.max_file_size())
            .field("file_size_bits", &self.file_size_bits())
            .field("read_only", &self.read_only())
            .field("no_set_id", &self.no_set_id())
            .finish()
    }
}

#[cfg(test)]
mod filesystem_stats_tests {
    use super::*;

    fn values(stats: &FilesystemStats) -> [Option<u64>; 15] {
        [
            stats.total_bytes(),
            stats.free_bytes(),
            stats.available_bytes(),
            stats.total_files(),
            stats.free_files(),
            stats.available_files(),
            stats.block_size(),
            stats.fragment_size(),
            stats.max_name_len(),
            stats.max_path_len(),
            stats.max_links(),
            stats.max_file_size(),
            stats.file_size_bits().map(u64::from),
            stats.read_only().map(u64::from),
            stats.no_set_id().map(u64::from),
        ]
    }

    #[test]
    fn compact_layout_preserves_all_optional_values_independently() {
        assert_eq!(std::mem::size_of::<FilesystemStatsFlags>(), 4);
        assert_eq!(std::mem::size_of::<FilesystemStats>(), 104);
        let empty = FilesystemStats::default();
        assert_eq!(values(&empty), [None; 15]);
        let full = empty
            .clone()
            .with_total_bytes(Some(1))
            .with_free_bytes(Some(2))
            .with_available_bytes(Some(3))
            .with_total_files(Some(4))
            .with_free_files(Some(5))
            .with_available_files(Some(6))
            .with_block_size(Some(7))
            .with_fragment_size(Some(8))
            .with_max_name_len(Some(9))
            .with_max_path_len(Some(10))
            .with_max_links(Some(11))
            .with_max_file_size(Some(12))
            .with_file_size_bits(Some(13))
            .with_read_only(Some(true))
            .with_no_set_id(Some(false));
        assert_eq!(
            values(&full),
            [
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                Some(8),
                Some(9),
                Some(10),
                Some(11),
                Some(12),
                Some(13),
                Some(1),
                Some(0)
            ]
        );
        macro_rules! check {
            ($setter:ident, $bit:literal, $ty:ty, $($value:expr),+) => {
                for value in [$(Some::<$ty>($value),)+ None] {
                    let mut expected = values(&full);
                    expected[$bit] = value.map(u64::from);
                    assert_eq!(values(&full.clone().$setter(value)), expected);
                    let populated = empty.clone().$setter(value);
                    let mut expected = [None; 15];
                    expected[$bit] = value.map(u64::from);
                    assert_eq!(values(&populated), expected);
                    assert_eq!(populated.$setter(None), empty);
                }
            };
        }
        check!(with_total_bytes, 0, u64, 0, 1, u64::MAX);
        check!(with_free_bytes, 1, u64, 0, 1, u64::MAX);
        check!(with_available_bytes, 2, u64, 0, 1, u64::MAX);
        check!(with_total_files, 3, u64, 0, 1, u64::MAX);
        check!(with_free_files, 4, u64, 0, 1, u64::MAX);
        check!(with_available_files, 5, u64, 0, 1, u64::MAX);
        check!(with_block_size, 6, u64, 0, 1, u64::MAX);
        check!(with_fragment_size, 7, u64, 0, 1, u64::MAX);
        check!(with_max_name_len, 8, u64, 0, 1, u64::MAX);
        check!(with_max_path_len, 9, u64, 0, 1, u64::MAX);
        check!(with_max_links, 10, u64, 0, 1, u64::MAX);
        check!(with_max_file_size, 11, u64, 0, 1, u64::MAX);
        check!(with_file_size_bits, 12, u32, 0, 1, u32::MAX);
        check!(with_read_only, 13, bool, false, true);
        check!(with_no_set_id, 14, bool, false, true);
    }

    #[test]
    fn debug_reports_logical_optional_values_not_storage() {
        let stats = FilesystemStats::default()
            .with_free_bytes(Some(0))
            .with_read_only(Some(false));
        let debug = format!("{stats:?}");
        assert!(debug.contains("total_bytes: None"));
        assert!(debug.contains("free_bytes: Some(0)"));
        assert!(debug.contains("read_only: Some(false)"));
        assert!(!debug.contains("flags"));
    }
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
#[must_use = "configuration and requests have no effect until used"]
pub struct CopyOptions {
    flags: CopyFlags,
}
impl CopyOptions {
    pub fn new() -> Self {
        Self::default()
    }
    /// Follow final source symlinks when true; otherwise recreate the links.
    pub fn with_follow_source_symlinks(mut self, follow: bool) -> Self {
        self.flags.set_follow_source_symlinks(follow);
        self
    }
    pub fn follow_source_symlinks(self) -> bool {
        self.flags.follow_source_symlinks()
    }
}

#[cfg(test)]
mod copy_option_tests {
    use super::CopyOptions;
    #[test]
    fn follows_source_symlinks_by_default_and_builder_can_disable_it() {
        assert!(CopyOptions::default().follow_source_symlinks());
        let preserve = CopyOptions::new().with_follow_source_symlinks(false);
        assert!(!preserve.follow_source_symlinks());
        assert!(
            preserve
                .with_follow_source_symlinks(true)
                .follow_source_symlinks()
        );
    }
}

#[cfg(test)]
mod sparse_metadata_tests {
    use super::*;
    #[test]
    fn compact_attrs_preserve_each_presence_bit_zero_and_maximum_values() {
        assert!(std::mem::size_of::<Attrs>() <= 144);
        let fields = [
            AttrMask::MODE,
            AttrMask::SIZE,
            AttrMask::NLINK,
            AttrMask::FILEID,
            AttrMask::BLOCKS,
            AttrMask::UID,
            AttrMask::GID,
            AttrMask::RDEV,
            AttrMask::ATIME,
            AttrMask::MTIME,
            AttrMask::CTIME,
            AttrMask::NAMED_ATTR,
            AttrMask::CHANGE,
            AttrMask::FS_ID,
        ];
        for value in [0, u64::MAX] {
            for returned in [AttrMask::empty(), AttrMask::all()]
                .into_iter()
                .chain(fields)
            {
                let attrs = metadata_from_attrs(VfAttrs {
                    returned,
                    ftype: VfType::Symlink,
                    size: value,
                    mode: value as u32,
                    nlink: value as u32,
                    fileid: value,
                    fs_id: if value == 0 { 0 } else { u128::MAX },
                    change: value,
                    uid: value as u32,
                    gid: value as u32,
                    rdev: value,
                    blocks: value,
                    atime_sec: -1,
                    atime_nsec: 500_000_000,
                    mtime_sec: 1,
                    mtime_nsec: 250_000_000,
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    has_named_attr: value != 0,
                    ..VfAttrs::default()
                });
                macro_rules! check {
                    ($getter:ident, $mask:ident, $expected:expr) => {
                        assert_eq!(
                            attrs.$getter(),
                            returned.contains(AttrMask::$mask).then_some($expected),
                            "{} {returned:?}",
                            stringify!($getter)
                        );
                    };
                }
                assert_eq!(attrs.present_mask(), returned);
                assert!(attrs.is_symlink());
                check!(len, SIZE, value);
                check!(is_empty, SIZE, value == 0);
                check!(mode, MODE, value as u32);
                check!(permissions, MODE, Permissions::from_mode(value as u32));
                check!(nlink, NLINK, value as u32);
                check!(file_id, FILEID, value);
                check!(fs_id, FS_ID, if value == 0 { 0 } else { u128::MAX });
                check!(change_id, CHANGE, value);
                check!(blocks, BLOCKS, value);
                check!(device_id, RDEV, value);
                check!(uid, UID, value as u32);
                check!(gid, GID, value as u32);
                check!(has_named_attributes, NAMED_ATTR, value != 0);
                check!(
                    accessed,
                    ATIME,
                    std::time::UNIX_EPOCH - std::time::Duration::from_millis(500)
                );
                check!(
                    modified,
                    MTIME,
                    std::time::UNIX_EPOCH + std::time::Duration::from_millis(1250)
                );
                check!(changed, CTIME, std::time::UNIX_EPOCH);
                assert_eq!(
                    attrs.fs_obj_id().map(|id| (id.fs_id(), id.file_id())),
                    returned
                        .contains(AttrMask::identity())
                        .then_some((if value == 0 { 0 } else { u128::MAX }, value))
                );
            }
        }
    }

    #[test]
    fn absent_integer_payloads_do_not_change_equality_or_debug_output() {
        let empty = metadata_from_attrs(VfAttrs::default());
        // These unreturned fields previously became None regardless of payload.
        let dirty = metadata_from_attrs(VfAttrs {
            blocks: u64::MAX,
            rdev: u64::MAX,
            nlink: u32::MAX,
            fs_id: u128::MAX,
            fileid: u64::MAX,
            change: u64::MAX,
            uid: u32::MAX,
            gid: u32::MAX,
            has_named_attr: true,
            atime_sec: 1,
            mtime_sec: 2,
            ctime_sec: 3,
            ..VfAttrs::default()
        });
        assert_eq!(dirty, empty);
        assert_eq!(format!("{dirty:?}"), format!("{empty:?}"));
        assert!(format!("{dirty:?}").contains("fs_id: None"));
    }

    #[test]
    fn timestamp_conversion_failure_does_not_erase_the_returned_mask() {
        let returned = AttrMask::ATIME | AttrMask::MTIME | AttrMask::CTIME;
        let attrs = metadata_from_attrs(VfAttrs {
            returned,
            atime_sec: i64::MAX,
            atime_nsec: u32::MAX,
            mtime_sec: i64::MAX,
            mtime_nsec: u32::MAX,
            ctime_sec: i64::MAX,
            ctime_nsec: u32::MAX,
            ..VfAttrs::default()
        });
        let expected =
            std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(i64::MAX as u64, u32::MAX));
        assert_eq!(attrs.accessed(), expected);
        assert_eq!(attrs.modified(), expected);
        assert_eq!(attrs.changed(), expected);
        assert_eq!(attrs.present_mask(), returned);
        assert!(attrs.contains(returned));
        #[cfg(target_os = "linux")]
        assert_eq!(expected, None);
    }

    #[test]
    fn omitted_fields_are_distinct_from_real_zero_values() {
        let missing = metadata_from_attrs(VfAttrs {
            size: 123,
            mode: 0o777,
            ..VfAttrs::default()
        });
        assert_eq!(missing.len(), None);
        assert_eq!(missing.is_empty(), None);
        assert_eq!(missing.permissions(), None);
        let zero = metadata_from_attrs(VfAttrs {
            returned: AttrMask::SIZE | AttrMask::MODE,
            size: 0,
            mode: 0,
            ..VfAttrs::default()
        });
        assert_eq!(zero.len(), Some(0));
        assert_eq!(zero.is_empty(), Some(true));
        assert_eq!(zero.permissions().unwrap().mode(), 0);
        let size_only = metadata_from_attrs(VfAttrs {
            returned: AttrMask::SIZE,
            size: 123,
            mode: 0o777,
            ..VfAttrs::default()
        });
        assert_eq!(size_only.len(), Some(123));
        assert_eq!(size_only.is_empty(), Some(false));
        assert_eq!(size_only.permissions(), None);
    }
}

#[cfg(test)]
mod open_request_ownership_tests {
    use super::*;
    #[test]
    fn borrowed_open_requests_and_their_clones_reuse_path_storage() {
        let path = PathBuf::from("/borrowed-file");
        let pointer = path.as_os_str().as_encoded_bytes().as_ptr();
        let request = OpenOp::new(&path, OpenFlags::READ);
        assert_eq!(
            request.path().as_os_str().as_encoded_bytes().as_ptr(),
            pointer
        );
        assert_eq!(
            request
                .clone()
                .path()
                .as_os_str()
                .as_encoded_bytes()
                .as_ptr(),
            pointer
        );
    }
    #[test]
    fn owned_open_requests_outlive_inputs_and_transfer_their_storage() {
        let request = {
            let path = PathBuf::from("/retained-plan");
            OpenOp::new(&path, OpenFlags::WRITE | OpenFlags::CREATE_NEW)
                .mode(0o640)
                .into_owned()
        };
        let pointer = request.path().as_os_str().as_encoded_bytes().as_ptr();
        assert_eq!(request.creation_mode(), 0o640);
        assert_eq!(request.flags(), OpenFlags::WRITE | OpenFlags::CREATE_NEW);
        let path = request.into_path();
        assert_eq!(path, Path::new("/retained-plan"));
        assert_eq!(path.as_os_str().as_encoded_bytes().as_ptr(), pointer);
    }
}
