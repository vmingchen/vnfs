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

/// Original filesystem/protocol status, never a synthetic client code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatusCode {
    Errno(u32),
    Nfs(u32),
    Smb(u32),
}

pub(crate) fn status_kind(status: StatusCode) -> std::io::ErrorKind {
    use std::io::ErrorKind as K;
    match status {
        StatusCode::Errno(code) => std::io::Error::from_raw_os_error(code as i32).kind(),
        StatusCode::Nfs(code) => match code {
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
        StatusCode::Smb(code) => match code {
            0xc000_0022 | 0xc000_006d | 0xc000_006a | 0xc000_006e | 0xc000_0071 | 0xc000_0072
            | 0xc000_0234 | 0xc000_006f | 0xc000_0070 | 0xc000_0193 | 0xc000_0224 => {
                K::PermissionDenied
            }
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

pub(crate) fn errno_for(kind: std::io::ErrorKind, status: Option<StatusCode>) -> i32 {
    if let Some(StatusCode::Errno(code)) = status {
        return code as i32;
    }
    use std::io::ErrorKind as K;
    match kind {
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

pub(crate) mod standard;
pub use standard::{native_status, transport_kind};

mod indexed;
pub use indexed::IndexedError;
pub type VfResult<T> = std::io::Result<T>;
pub type VfRes = VfResult<()>;
