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
/// index, or a transport / client-side failure (where the index is
/// best-effort: backends report 0 when the failure cannot be attributed).
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
    /// A transport / client-side failure with a human-readable message; there
    /// is no filesystem status ([`err_no`](VfError::err_no) reports
    /// [`VF_ERR_RPC`]). `index` is `None` when the failure cannot be
    /// attributed to any operation.
    Transport {
        index: Option<usize>,
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

    /// A transport / client-side failure. `index` is best-effort; pass `None`
    /// when the failure cannot be attributed to a specific operation.
    pub fn transport(index: impl Into<Option<usize>>, message: impl Into<String>) -> VfError {
        VfError::Transport {
            index: index.into(),
            message: message.into(),
            operation: None,
            path: None,
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
                operation: None,
                path: None,
            }
        } else {
            VfError::Op {
                index: index.into().unwrap_or(0),
                err_no: e.status,
                domain: ErrorDomain::Nfs,
                operation: None,
                path: None,
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
            VfError::Op {
                err_no,
                domain,
                operation,
                path,
                ..
            } => VfError::Op {
                index,
                err_no,
                domain,
                operation,
                path,
            },
            VfError::Transport {
                message,
                operation,
                path,
                ..
            } => VfError::Transport {
                index: Some(index),
                message,
                operation,
                path,
            },
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
            VfError::Op { domain, .. } => *domain,
            VfError::Transport { .. } => ErrorDomain::Transport,
        }
    }

    pub fn status(&self) -> Option<StatusCode> {
        match self {
            VfError::Op {
                err_no,
                domain: ErrorDomain::Nfs,
                ..
            } => Some(StatusCode::Nfs(*err_no)),
            VfError::Op {
                err_no,
                domain: ErrorDomain::Smb,
                ..
            } => Some(StatusCode::Smb(*err_no)),
            VfError::Op {
                err_no,
                domain: ErrorDomain::Client,
                ..
            } => Some(StatusCode::Client(*err_no)),
            VfError::Op { err_no, .. } => Some(StatusCode::Errno(*err_no)),
            VfError::Transport { .. } => None,
        }
    }

    pub fn operation(&self) -> Option<&'static str> {
        match self {
            VfError::Op { operation, .. } | VfError::Transport { operation, .. } => *operation,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            VfError::Op { path, .. } | VfError::Transport { path, .. } => path.as_deref(),
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
                write!(f, " failed: {}", err_no)
            }
            VfError::Transport {
                index: Some(index),
                message,
                operation,
                path,
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
                ..
            } => {
                write!(f, "transport error: {}", message)
            }
        }
    }
}

impl std::error::Error for VfError {}

impl From<VfError> for std::io::Error {
    fn from(error: VfError) -> Self {
        let kind = match error.err_no() {
            ERR_NOENT => std::io::ErrorKind::NotFound,
            ERR_EXIST => std::io::ErrorKind::AlreadyExists,
            ERR_ACCES => std::io::ErrorKind::PermissionDenied,
            ERR_NOTDIR => std::io::ErrorKind::NotADirectory,
            ERR_ISDIR => std::io::ErrorKind::IsADirectory,
            ERR_INVAL | ERR_EBADF => std::io::ErrorKind::InvalidInput,
            _ => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, error)
    }
}

pub type VfResult<T> = Result<T, VfError>;
/// Result of a compound-style operation: `()` on success, or the index and
/// error of the first failing operation.
pub type VfRes = VfResult<()>;

/// Whether the caller can know if a failed operation changed remote state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutcomeCertainty {
    /// The operation completed and its result is known.
    Known,
    /// The server rejected the operation before it took effect.
    KnownNotApplied,
    /// A transport failure happened after dispatch, so reconciliation is
    /// required before retrying a mutation.
    Indeterminate,
}

/// Whether retrying an operation is safe without first reconciling state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetryClass {
    Safe,
    ReconcileFirst,
    Never,
}

impl VfError {
    /// Completion certainty derived from the class of failure.
    pub fn certainty(&self) -> OutcomeCertainty {
        if self.is_transport() {
            OutcomeCertainty::Indeterminate
        } else {
            OutcomeCertainty::KnownNotApplied
        }
    }

    /// Conservative retry guidance. Transport failures may have crossed the
    /// server boundary and therefore require reconciliation for mutations.
    pub fn retry_class(&self) -> RetryClass {
        if self.is_transport() {
            RetryClass::ReconcileFirst
        } else if self.err_no() == VF_ERR_UNSUPPORTED {
            RetryClass::Never
        } else {
            RetryClass::Safe
        }
    }
}

/// Per-operation state returned by the outcome-aware vector API.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpOutcome<T> {
    Success(T),
    /// The operation is known to have completed, but a legacy fail-fast
    /// backend discarded its returned value after a later operation failed.
    Completed,
    Failed(VfError),
    /// The backend stopped before dispatching this operation.
    NotAttempted,
    /// The request may have reached the server, but no authoritative result
    /// was received. Mutating operations must not be blindly replayed.
    Indeterminate(VfError),
}

/// Complete, index-preserving result of an ordered vector request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchOutcome<T> {
    operations: Vec<OpOutcome<T>>,
}

impl<T> BatchOutcome<T> {
    pub fn new(operations: Vec<OpOutcome<T>>) -> Self {
        Self { operations }
    }

    pub fn all_success(values: Vec<T>) -> Self {
        Self::new(values.into_iter().map(OpOutcome::Success).collect())
    }

    pub fn operations(&self) -> &[OpOutcome<T>] {
        &self.operations
    }

    pub fn iter(&self) -> std::slice::Iter<'_, OpOutcome<T>> {
        self.operations.iter()
    }

    pub fn len(&self) -> usize {
        self.operations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn into_operations(self) -> Vec<OpOutcome<T>> {
        self.operations
    }

    pub fn is_complete_success(&self) -> bool {
        self.operations
            .iter()
            .all(|outcome| matches!(outcome, OpOutcome::Success(_)))
    }

    /// Transform successful values without disturbing per-operation failure
    /// or completion state.
    pub fn map<U>(self, mut transform: impl FnMut(T) -> U) -> BatchOutcome<U> {
        self.map_with_index(|_, value| transform(value))
    }

    /// Transform successful values with their original request indices.
    pub fn map_with_index<U>(self, mut transform: impl FnMut(usize, T) -> U) -> BatchOutcome<U> {
        BatchOutcome::new(
            self.operations
                .into_iter()
                .enumerate()
                .map(|(index, outcome)| match outcome {
                    OpOutcome::Success(value) => OpOutcome::Success(transform(index, value)),
                    OpOutcome::Completed => OpOutcome::Completed,
                    OpOutcome::Failed(error) => OpOutcome::Failed(error),
                    OpOutcome::NotAttempted => OpOutcome::NotAttempted,
                    OpOutcome::Indeterminate(error) => OpOutcome::Indeterminate(error),
                })
                .collect(),
        )
    }

    /// Enrich errors while preserving every outcome and request index.
    pub fn map_errors(mut self, mut transform: impl FnMut(usize, VfError) -> VfError) -> Self {
        for (index, outcome) in self.operations.iter_mut().enumerate() {
            match outcome {
                OpOutcome::Failed(error) | OpOutcome::Indeterminate(error) => {
                    *error = transform(index, error.clone());
                }
                _ => {}
            }
        }
        self
    }

    pub fn first_error(&self) -> Option<&VfError> {
        self.operations.iter().find_map(|outcome| match outcome {
            OpOutcome::Failed(error) | OpOutcome::Indeterminate(error) => Some(error),
            _ => None,
        })
    }

    pub fn indeterminate_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.operations
            .iter()
            .enumerate()
            .filter_map(|(index, outcome)| {
                matches!(outcome, OpOutcome::Indeterminate(_)).then_some(index)
            })
    }

    /// Convert to the historical fail-fast shape, returning the first error.
    pub fn into_fail_fast(self) -> VfResult<Vec<T>> {
        let mut values = Vec::with_capacity(self.operations.len());
        for outcome in self.operations {
            match outcome {
                OpOutcome::Success(value) => values.push(value),
                OpOutcome::Completed => {
                    return Err(VfError::transport(
                        None,
                        "completed operation result was not retained",
                    ));
                }
                OpOutcome::Failed(error) | OpOutcome::Indeterminate(error) => return Err(error),
                OpOutcome::NotAttempted => {
                    return Err(VfError::transport(None, "operation was not attempted"));
                }
            }
        }
        Ok(values)
    }

    /// Consume a completely successful batch. This is the ergonomic alias
    /// for compatibility-oriented [`into_fail_fast`](Self::into_fail_fast).
    pub fn into_values(self) -> VfResult<Vec<T>> {
        self.into_fail_fast()
    }
}

impl<T> IntoIterator for BatchOutcome<T> {
    type Item = OpOutcome<T>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.operations.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a BatchOutcome<T> {
    type Item = &'a OpOutcome<T>;
    type IntoIter = std::slice::Iter<'a, OpOutcome<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.operations.iter()
    }
}

impl<T> BatchOutcome<T> {
    /// Adapt a legacy ordered vector result without inventing lost values.
    pub fn from_fail_fast_values(len: usize, result: VfResult<Vec<T>>) -> Self {
        match result {
            Ok(values) if values.len() == len => Self::all_success(values),
            Ok(_) => Self::new(
                (0..len)
                    .map(|_| {
                        OpOutcome::Indeterminate(VfError::transport(
                            None,
                            "backend returned the wrong result count",
                        ))
                    })
                    .collect(),
            ),
            Err(error) if error.is_transport() => {
                let known_prefix = error.index_opt().unwrap_or(0).min(len);
                Self::new(
                    (0..len)
                        .map(|index| {
                            if index < known_prefix {
                                OpOutcome::Completed
                            } else {
                                OpOutcome::Indeterminate(error.clone())
                            }
                        })
                        .collect(),
                )
            }
            Err(error) => {
                let failed = error.index_opt().unwrap_or(len);
                if failed >= len {
                    return Self::new(
                        (0..len)
                            .map(|_| OpOutcome::Indeterminate(error.clone()))
                            .collect(),
                    );
                }
                Self::new(
                    (0..len)
                        .map(|index| {
                            if index < failed {
                                OpOutcome::Completed
                            } else if index == failed {
                                OpOutcome::Failed(error.clone())
                            } else {
                                OpOutcome::NotAttempted
                            }
                        })
                        .collect(),
                )
            }
        }
    }
}

impl BatchOutcome<()> {
    /// Preserve the known prefix of an ordered, fail-fast mutation. A
    /// protocol status proves the prefix completed and the failing operation
    /// did not; a transport failure makes every dispatched outcome
    /// indeterminate.
    pub fn from_fail_fast(len: usize, result: VfRes) -> Self {
        match result {
            Ok(()) => Self::all_success((0..len).map(|_| ()).collect()),
            Err(error) if error.is_transport() => {
                let known_prefix = error.index_opt().unwrap_or(0).min(len);
                Self::new(
                    (0..len)
                        .map(|index| {
                            if index < known_prefix {
                                OpOutcome::Success(())
                            } else {
                                OpOutcome::Indeterminate(error.clone())
                            }
                        })
                        .collect(),
                )
            }
            Err(error) => {
                let failed = error.index_opt().unwrap_or(len);
                if failed >= len {
                    return Self::new(
                        (0..len)
                            .map(|_| OpOutcome::Indeterminate(error.clone()))
                            .collect(),
                    );
                }
                Self::new(
                    (0..len)
                        .map(|index| {
                            if index < failed {
                                OpOutcome::Success(())
                            } else if index == failed {
                                OpOutcome::Failed(error.clone())
                            } else {
                                OpOutcome::NotAttempted
                            }
                        })
                        .collect(),
                )
            }
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

impl OpenFlags {
    /// Translate the typed flags for compatibility backends.
    pub fn to_libc(self) -> VfResult<i32> {
        let writable = self.intersects(Self::WRITE | Self::APPEND);
        let mut raw = match (self.contains(Self::READ), writable) {
            (true, true) => libc::O_RDWR,
            (true, false) => libc::O_RDONLY,
            (false, true) => libc::O_WRONLY,
            (false, false) => return Err(VfError::failure(0, ERR_INVAL)),
        };
        if self.contains(Self::TRUNCATE) && !writable {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        if self.intersects(Self::CREATE | Self::CREATE_NEW) && !writable {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        if self.contains(Self::APPEND) {
            raw |= libc::O_APPEND;
        }
        if self.contains(Self::TRUNCATE) {
            raw |= libc::O_TRUNC;
        }
        if self.intersects(Self::CREATE | Self::CREATE_NEW) {
            raw |= libc::O_CREAT;
        }
        if self.contains(Self::CREATE_NEW) {
            raw |= libc::O_EXCL;
        }
        Ok(raw)
    }
}

/// One complete open request. This replaces three error-prone parallel
/// slices of paths, flags, and modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    pub path: PathBuf,
    pub flags: OpenFlags,
    pub mode: u32,
}

impl OpenRequest {
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

/// Borrowed write request for callers that should not have to allocate an
/// owned `Vec<u8>` merely to submit a vector operation.
#[derive(Debug, Clone, Copy)]
pub struct WriteOpRef<'a> {
    pub file: &'a VfFile,
    pub offset: VfOffset,
    pub data: &'a [u8],
    pub creation: bool,
    pub truncate: bool,
}

impl<'a> WriteOpRef<'a> {
    pub fn new(file: &'a VfFile, offset: VfOffset, data: &'a [u8]) -> Self {
        Self {
            file,
            offset,
            data,
            creation: false,
            truncate: false,
        }
    }
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
pub struct Metadata {
    file_type: VfType,
    len: u64,
    permissions: Permissions,
    modified: Option<std::time::SystemTime>,
    accessed: Option<std::time::SystemTime>,
    changed: Option<std::time::SystemTime>,
    nlink: Option<u32>,
    file_id: Option<u64>,
    change_id: Option<u64>,
    uid: Option<u32>,
    gid: Option<u32>,
}

impl Metadata {
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

/// Rust-native metadata changes used by [`MetadataFileSystem`](https://docs.rs/vfsi-sync).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataUpdate {
    pub permissions: Option<Permissions>,
    pub len: Option<u64>,
    pub accessed: Option<std::time::SystemTime>,
    pub modified: Option<std::time::SystemTime>,
}

impl MetadataUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn permissions(mut self, permissions: Permissions) -> Self {
        self.permissions = Some(permissions);
        self
    }

    pub fn len(mut self, len: u64) -> Self {
        self.len = Some(len);
        self
    }

    pub fn accessed(mut self, accessed: std::time::SystemTime) -> Self {
        self.accessed = Some(accessed);
        self
    }

    pub fn modified(mut self, modified: std::time::SystemTime) -> Self {
        self.modified = Some(modified);
        self
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

impl From<VfAttrs> for Metadata {
    fn from(attributes: VfAttrs) -> Self {
        let returned = attributes.returned;
        Self {
            file_type: attributes.ftype,
            len: attributes.size,
            permissions: Permissions::from_mode(attributes.mode),
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
}

/// One native directory entry with its already-fetched metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    path: PathBuf,
    metadata: Metadata,
}

impl DirEntry {
    pub fn new(path: PathBuf, metadata: Metadata) -> Self {
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

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
}

/// Typed metadata lookup request. Returned fields are still represented by
/// [`VfAttrs`] during the compatibility transition, but request and mutation
/// masks are no longer conflated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataQuery {
    pub file: VfFile,
    pub attributes: AttrMask,
    pub follow_symlinks: bool,
}

impl MetadataQuery {
    pub fn new(file: VfFile, attributes: AttrMask) -> Self {
        Self {
            file,
            attributes,
            follow_symlinks: true,
        }
    }

    pub fn no_follow(mut self) -> Self {
        self.follow_symlinks = false;
        self
    }
}

/// Valid-by-construction metadata mutation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAttributes {
    pub file: VfFile,
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<(i64, u32)>,
    pub mtime: Option<(i64, u32)>,
    pub follow_symlinks: bool,
}

impl SetAttributes {
    pub fn new(file: VfFile) -> Self {
        Self {
            file,
            mode: None,
            size: None,
            atime: None,
            mtime: None,
            follow_symlinks: true,
        }
    }

    pub fn no_follow(mut self) -> Self {
        self.follow_symlinks = false;
        self
    }

    pub fn into_legacy(self) -> VfAttrs {
        let mut attrs = VfAttrs {
            file: self.file,
            ..VfAttrs::default()
        };
        if let Some(mode) = self.mode {
            attrs.masks |= AttrMask::MODE;
            attrs.mode = mode;
        }
        if let Some(size) = self.size {
            attrs.masks |= AttrMask::SIZE;
            attrs.size = size;
        }
        if let Some((sec, nsec)) = self.atime {
            attrs.masks |= AttrMask::ATIME;
            attrs.atime_sec = sec;
            attrs.atime_nsec = nsec;
        }
        if let Some((sec, nsec)) = self.mtime {
            attrs.masks |= AttrMask::MTIME;
            attrs.mtime_sec = sec;
            attrs.mtime_nsec = nsec;
        }
        attrs
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
