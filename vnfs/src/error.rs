//! Structured errors for the low-level NFSv4.1 client.

use std::fmt;

/// NFS4 status used for transport-level failures (there is no NFS status).
pub const STATUS_TRANSPORT: u32 = u32::MAX;

/// An error from the low-level client: either an NFS4ERR_* status reported by
/// the server for a compound operation, or a transport/RPC-level failure.
///
/// Callers can inspect [`status`](RpcError::status) directly instead of
/// parsing error strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// Index of the failing operation within the compound.
    pub op_index: usize,
    /// The failing op's NFS4ERR_* status, or [`STATUS_TRANSPORT`].
    pub status: u32,
    /// Human-readable context.
    pub message: String,
}

impl RpcError {
    /// An NFS status from a specific compound operation.
    pub fn op(op_index: usize, status: u32) -> RpcError {
        RpcError {
            op_index,
            status,
            message: String::new(),
        }
    }

    /// A transport / RPC-level failure (no NFS status available).
    pub fn transport(message: impl Into<String>) -> RpcError {
        RpcError {
            op_index: 0,
            status: STATUS_TRANSPORT,
            message: message.into(),
        }
    }

    /// Whether this is a transport failure rather than an NFS status.
    pub fn is_transport(&self) -> bool {
        self.status == STATUS_TRANSPORT
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
