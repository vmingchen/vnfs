//! Portable filesystem errors, native status preservation, and RPC diagnostics.

use std::fmt;

/// NFS4 status used for transport-level failures (there is no NFS status).
pub const STATUS_TRANSPORT: u32 = u32::MAX;

pub(crate) const NFS_REFERRAL_UNSUPPORTED: &str = "NFSv4 referrals and filesystem migration are unsupported; connect directly to the destination server/export";

/// Transport provenance, not a guarantee that an operation is safe to retry.
/// Unknown failures remain `Other`; messages are never parsed to classify them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportKind {
    Timeout,
    Connection,
    InvalidReply,
    Auth,
    Other,
}

/// An error from the low-level client: either an NFS4ERR_* status reported by
/// the server for a compound operation, or a transport/RPC-level failure.
///
/// Callers can inspect [`status`](RpcError::status) directly instead of
/// parsing error strings.
///
/// `op_index` is the index of the failing operation in the caller's request
/// slice: the batched compound helpers (`vread_native`, `vwrite_native`, `getattr_many`,
/// ...) translate compound positions to caller-relative indices before
/// returning. For single-operation helpers it is the position within the
/// compound (typically 2: `SEQUENCE`, `PUTFH`, op). Transport failures always
/// report `op_index == 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// Index of the failing operation within the compound.
    pub op_index: usize,
    /// The failing op's NFS4ERR_* status, or [`STATUS_TRANSPORT`].
    pub status: u32,
    /// Human-readable context.
    pub message: String,
    /// Present only for transport/RPC failures.
    pub transport_kind: Option<TransportKind>,
}

impl RpcError {
    /// An NFS status from a specific compound operation.
    pub fn op(op_index: usize, status: u32) -> RpcError {
        RpcError {
            op_index,
            status,
            message: String::new(),
            transport_kind: None,
        }
    }

    /// A transport / RPC-level failure (no NFS status available).
    pub fn transport(message: impl Into<String>) -> RpcError {
        Self::transport_with_kind(TransportKind::Other, message)
    }

    pub fn transport_with_kind(kind: TransportKind, message: impl Into<String>) -> RpcError {
        RpcError {
            op_index: 0,
            status: STATUS_TRANSPORT,
            message: message.into(),
            transport_kind: Some(kind),
        }
    }

    /// Preserve classification at an I/O boundary without inspecting its text.
    pub fn from_io(error: std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let kind = match error.kind() {
            K::TimedOut => TransportKind::Timeout,
            K::ConnectionRefused
            | K::ConnectionReset
            | K::ConnectionAborted
            | K::NotConnected
            | K::BrokenPipe => TransportKind::Connection,
            _ => TransportKind::Other,
        };
        Self::transport_with_kind(kind, error.to_string())
    }

    /// Whether this is a transport failure rather than an NFS status.
    pub fn is_transport(&self) -> bool {
        self.status == STATUS_TRANSPORT
    }

    /// Re-attribute this error to a different operation index (used by the
    /// batched helpers to translate compound positions to caller indices).
    pub fn with_op_index(self, index: usize) -> RpcError {
        RpcError {
            op_index: index,
            ..self
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_transport() {
            write!(f, "transport error: {}", self.message)
        } else {
            write!(f, "op {} failed: status {}", self.op_index, self.status)?;
            if self.status == 10019 {
                write!(f, "; {NFS_REFERRAL_UNSUPPORTED}")?;
            }
            Ok(())
        }
    }
}

impl std::error::Error for RpcError {}

/// Convenience alias used throughout the low-level client.
pub type RpcResult<T> = Result<T, RpcError>;

/// Failure of a logical request. An index identifies an input, not completed
/// progress or retry safety. Native statuses remain available independently
/// from the portable category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfError {
    index: Option<usize>,
    detail: ErrorDetail,
    // Keep strings, paths and source ownership off the common status-only path.
    context: Option<Box<ErrorContext>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorDetail {
    Status(StatusCode),
    Client(std::io::ErrorKind),
    Transport(TransportKind),
}

#[derive(Debug, Clone, Default)]
struct ErrorContext {
    operation: Option<&'static str>,
    path: Option<std::path::PathBuf>,
    message: Option<std::borrow::Cow<'static, str>>,
    source: Option<std::sync::Arc<std::io::Error>>,
}

impl PartialEq for ErrorContext {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.path == other.path
            && self.message == other.message
            && match (&self.source, &other.source) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    std::sync::Arc::ptr_eq(a, b)
                        || (a.kind() == b.kind()
                            && a.raw_os_error() == b.raw_os_error()
                            && a.to_string() == b.to_string())
                }
                _ => false,
            }
    }
}
impl Eq for ErrorContext {}

/// Namespace of a native status or a failure without a native status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorDomain {
    Filesystem,
    Nfs,
    Smb,
    Transport,
    Client,
}

/// Original filesystem/protocol status, never a synthetic client code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatusCode {
    Errno(u32),
    Nfs(u32),
    Smb(u32),
}

impl VfError {
    pub fn from_status(index: impl Into<Option<usize>>, status: StatusCode) -> Self {
        Self {
            index: index.into(),
            detail: ErrorDetail::Status(status),
            context: None,
        }
    }

    /// An actual OS errno, or an errno-style validation failure.
    pub fn errno(index: impl Into<Option<usize>>, code: u32) -> Self {
        Self::from_status(index, StatusCode::Errno(code))
    }

    /// A client failure without an OS or protocol status.
    pub fn from_kind(index: impl Into<Option<usize>>, kind: std::io::ErrorKind) -> Self {
        Self {
            index: index.into(),
            detail: ErrorDetail::Client(kind),
            context: None,
        }
    }

    pub fn nfs(index: impl Into<Option<usize>>, status: u32) -> Self {
        Self::from_status(index, StatusCode::Nfs(status))
    }

    pub fn smb(index: impl Into<Option<usize>>, status: u32) -> Self {
        Self::from_status(index, StatusCode::Smb(status))
    }

    /// Preserve an I/O error's category, actual errno and original source.
    pub fn from_io(index: impl Into<Option<usize>>, source: std::io::Error) -> Self {
        let mut error = match source.raw_os_error() {
            Some(code) => Self::errno(index, code as u32),
            None => Self::from_kind(index, source.kind()),
        };
        error.context_mut().source = Some(std::sync::Arc::new(source));
        error
    }

    /// Unknown transport provenance; use a typed category when it is known.
    pub fn transport(
        index: impl Into<Option<usize>>,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self::transport_with_kind(index, TransportKind::Other, message)
    }

    pub fn transport_with_kind(
        index: impl Into<Option<usize>>,
        kind: TransportKind,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self {
            index: index.into(),
            detail: ErrorDetail::Transport(kind),
            context: None,
        }
        .with_message(message)
    }

    /// A malformed wire response or backend contract violation.
    pub fn invalid_reply(
        index: impl Into<Option<usize>>,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self::transport_with_kind(index, TransportKind::InvalidReply, message)
    }

    pub fn unsupported(index: impl Into<Option<usize>>) -> Self {
        Self::from_kind(index, std::io::ErrorKind::Unsupported)
    }

    /// Logical input index, not a completion boundary.
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn status(&self) -> Option<StatusCode> {
        match self.detail {
            ErrorDetail::Status(status) => Some(status),
            _ => None,
        }
    }

    pub fn domain(&self) -> ErrorDomain {
        match self.detail {
            ErrorDetail::Status(StatusCode::Errno(_)) => ErrorDomain::Filesystem,
            ErrorDetail::Status(StatusCode::Nfs(_)) => ErrorDomain::Nfs,
            ErrorDetail::Status(StatusCode::Smb(_)) => ErrorDomain::Smb,
            ErrorDetail::Client(_) => ErrorDomain::Client,
            ErrorDetail::Transport(_) => ErrorDomain::Transport,
        }
    }

    /// Compatibility code only. Inspect `status()` or `kind()` in Rust.
    /// Synthetic unsupported/RPC sentinels are confined to this accessor.
    pub fn raw_code(&self) -> u32 {
        match self.detail {
            ErrorDetail::Status(StatusCode::Errno(n) | StatusCode::Nfs(n) | StatusCode::Smb(n)) => {
                n
            }
            ErrorDetail::Transport(_) => crate::VF_ERR_RPC,
            ErrorDetail::Client(std::io::ErrorKind::Unsupported) => crate::VF_ERR_UNSUPPORTED,
            ErrorDetail::Client(_) => self.to_errno() as u32,
        }
    }

    pub fn is_transport(&self) -> bool {
        matches!(self.detail, ErrorDetail::Transport(_))
    }
    /// Provenance only; never permission to replay mutations.
    pub fn transport_kind(&self) -> Option<TransportKind> {
        match self.detail {
            ErrorDetail::Transport(kind) => Some(kind),
            _ => None,
        }
    }
    pub fn transport_message(&self) -> Option<&str> {
        if self.is_transport() {
            self.message()
        } else {
            None
        }
    }
    pub fn message(&self) -> Option<&str> {
        self.context.as_deref().and_then(|c| c.message.as_deref())
    }
    pub fn operation(&self) -> Option<&'static str> {
        self.context.as_deref().and_then(|c| c.operation)
    }
    pub fn path(&self) -> Option<&std::path::Path> {
        self.context.as_deref().and_then(|c| c.path.as_deref())
    }
    pub fn with_index(mut self, index: usize) -> Self {
        self.index = Some(index);
        self
    }
    /// Preserve unknown attribution rather than inventing input zero.
    pub fn map_index(mut self, map: impl FnOnce(usize) -> usize) -> Self {
        self.index = self.index.map(map);
        self
    }
    pub fn with_context(
        mut self,
        operation: &'static str,
        path: impl Into<std::path::PathBuf>,
    ) -> Self {
        let context = self.context_mut();
        context.operation = Some(operation);
        context.path = Some(path.into());
        self
    }
    pub fn with_message(mut self, message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        self.context_mut().message = Some(message.into());
        self
    }
    fn context_mut(&mut self) -> &mut ErrorContext {
        self.context.get_or_insert_with(Default::default)
    }

    /// Portable category, deliberately not retry guidance.
    pub fn kind(&self) -> std::io::ErrorKind {
        use std::io::ErrorKind as K;
        match self.detail {
            ErrorDetail::Client(kind) => kind,
            ErrorDetail::Transport(TransportKind::Timeout) => K::TimedOut,
            ErrorDetail::Transport(TransportKind::InvalidReply) => K::InvalidData,
            ErrorDetail::Transport(TransportKind::Auth) => K::PermissionDenied,
            ErrorDetail::Transport(_) => K::Other,
            ErrorDetail::Status(StatusCode::Errno(code)) => {
                std::io::Error::from_raw_os_error(code as i32).kind()
            }
            ErrorDetail::Status(StatusCode::Nfs(code)) => match code {
                1 | 13 | 10016 => K::PermissionDenied,
                2 => K::NotFound,
                17 => K::AlreadyExists,
                18 => K::CrossesDevices,
                20 => K::NotADirectory,
                21 => K::IsADirectory,
                22 => K::InvalidInput,
                27 => K::FileTooLarge,
                28 => K::StorageFull,
                30 => K::ReadOnlyFilesystem,
                31 => K::TooManyLinks,
                63 => K::InvalidFilename,
                66 => K::DirectoryNotEmpty,
                69 => K::QuotaExceeded,
                70 => K::StaleNetworkFileHandle,
                10004 | 10019 | 10032 => K::Unsupported,
                10008 => K::WouldBlock,
                _ => K::Other,
            },
            ErrorDetail::Status(StatusCode::Smb(code)) => match code {
                0xc000_0022 | 0xc000_006d | 0xc000_006a | 0xc000_006e | 0xc000_0071
                | 0xc000_0072 | 0xc000_0234 | 0xc000_006f | 0xc000_0070 | 0xc000_0193
                | 0xc000_0224 => K::PermissionDenied,
                0xc000_000f | 0xc000_0034 | 0xc000_003a | 0xc000_00cc => K::NotFound,
                0xc000_0035 => K::AlreadyExists,
                0xc000_000d => K::InvalidInput,
                0xc000_0033 | 0xc000_0106 => K::InvalidFilename,
                0xc000_0043 | 0xc000_0054 => K::ResourceBusy,
                0xc000_007f => K::StorageFull,
                0xc000_0044 => K::QuotaExceeded,
                0xc000_00a2 => K::ReadOnlyFilesystem,
                0xc000_0002 | 0xc000_0010 | 0xc000_00bb | 0xc000_0257 => K::Unsupported,
                0xc000_0101 => K::DirectoryNotEmpty,
                0xc000_0103 => K::NotADirectory,
                0xc000_00ba => K::IsADirectory,
                0xc000_00d4 => K::CrossesDevices,
                _ => K::Other,
            },
        }
    }

    /// Lossy OS errno for language/legacy boundaries. Native status stays intact.
    pub fn to_errno(&self) -> i32 {
        if let Some(StatusCode::Errno(code)) = self.status() {
            return code as i32;
        }
        use std::io::ErrorKind as K;
        match self.kind() {
            K::NotFound => libc::ENOENT,
            K::PermissionDenied => libc::EACCES,
            K::AlreadyExists => libc::EEXIST,
            K::NotADirectory => libc::ENOTDIR,
            K::IsADirectory => libc::EISDIR,
            K::DirectoryNotEmpty => libc::ENOTEMPTY,
            K::InvalidInput => libc::EINVAL,
            K::InvalidData => libc::EBADMSG,
            K::InvalidFilename => libc::ENAMETOOLONG,
            K::Unsupported => libc::EOPNOTSUPP,
            K::TimedOut => libc::ETIMEDOUT,
            K::WouldBlock => libc::EAGAIN,
            K::Interrupted => libc::EINTR,
            K::StorageFull => libc::ENOSPC,
            K::QuotaExceeded => libc::EDQUOT,
            K::StaleNetworkFileHandle => libc::ESTALE,
            K::FileTooLarge => libc::EFBIG,
            K::ReadOnlyFilesystem => libc::EROFS,
            K::ResourceBusy => libc::EBUSY,
            K::CrossesDevices => libc::EXDEV,
            K::TooManyLinks => libc::EMLINK,
            K::OutOfMemory => libc::ENOMEM,
            K::BrokenPipe => libc::EPIPE,
            K::ConnectionRefused => libc::ECONNREFUSED,
            K::ConnectionReset => libc::ECONNRESET,
            K::ConnectionAborted => libc::ECONNABORTED,
            K::NotConnected => libc::ENOTCONN,
            _ => libc::EIO,
        }
    }
}

impl std::fmt::Display for VfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.index {
            Some(index) => {
                write!(f, "op {index}")?;
                if let Some(operation) = self.operation() {
                    write!(f, " ({operation})")?;
                }
                if let Some(path) = self.path() {
                    write!(f, " for {}", path.display())?;
                }
            }
            None if self.is_transport() => {
                if let Some(operation) = self.operation() {
                    write!(f, "{operation} ")?;
                }
                if let Some(path) = self.path() {
                    write!(f, "for {} ", path.display())?;
                }
            }
            None => {
                write!(f, "{}", self.operation().unwrap_or("operation"))?;
                if let Some(path) = self.path() {
                    write!(f, " for {}", path.display())?;
                }
            }
        }
        if self.is_transport() {
            if self.index.is_some() {
                write!(f, " ")?;
            }
            write!(
                f,
                "transport error: {}",
                self.message().unwrap_or("unknown cause")
            )?;
        } else {
            write!(
                f,
                " failed: {} ({:?}",
                std::io::Error::from(self.kind()),
                self.domain()
            )?;
            if self.status().is_some() {
                write!(f, " status {}", self.raw_code())?;
            }
            if self.index.is_none() {
                write!(f, "; operation index unknown")?;
            }
            write!(f, ")")?;
            if let Some(message) = self.message() {
                write!(f, ": {message}")?;
            }
        }
        match self.status() {
            Some(StatusCode::Nfs(10019)) => write!(f, "; {NFS_REFERRAL_UNSUPPORTED}"),
            Some(StatusCode::Smb(0xc000_0257)) => write!(
                f,
                "; SMB DFS referrals are unsupported; connect directly to the destination server/share"
            ),
            _ => Ok(()),
        }
    }
}

impl std::error::Error for VfError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.context.as_deref()?.source.as_deref().map(|e| e as _)
    }
}

/// Convert with an explicitly chosen logical input index, not a wire offset.
pub fn error_from_rpc(error: RpcError, index: impl Into<Option<usize>>) -> VfError {
    if error.is_transport() {
        VfError::transport_with_kind(
            index,
            error.transport_kind.unwrap_or(TransportKind::Other),
            error.message,
        )
    } else {
        let result = VfError::nfs(index, error.status);
        if error.message.is_empty() {
            result
        } else {
            result.with_message(error.message)
        }
    }
}
/// Use only when the RPC helper has already mapped wire offsets to inputs.
pub fn error_from_rpc_indexed(error: RpcError) -> VfError {
    if error.is_transport() {
        return error_from_rpc(error, None);
    }
    let index = error.op_index;
    error_from_rpc(error, index)
}
impl From<VfError> for std::io::Error {
    fn from(error: VfError) -> Self {
        Self::new(error.kind(), error)
    }
}
pub type VfResult<T> = Result<T, VfError>;
pub type VfRes = VfResult<()>;
