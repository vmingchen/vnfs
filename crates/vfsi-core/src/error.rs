//! Structured errors for the low-level NFSv4.1 client.

use std::fmt;

/// NFS4 status used for transport-level failures (there is no NFS status).
pub const STATUS_TRANSPORT: u32 = u32::MAX;

/// Transport provenance, not a guarantee that an operation is safe to retry.
/// Unknown failures remain `Other`; messages are never parsed to classify them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportKind {
    Timeout,
    Connection,
    InvalidReply,
    Authentication,
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
            write!(f, "op {} failed: status {}", self.op_index, self.status)
        }
    }
}

impl std::error::Error for RpcError {}

/// Convenience alias used throughout the low-level client.
pub type RpcResult<T> = Result<T, RpcError>;
