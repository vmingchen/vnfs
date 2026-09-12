//! NFSv4.1 and NFSv4.2 backend for VFSI.

#[cfg(feature = "ffi")]
pub mod client;
#[cfg(feature = "ffi")]
pub mod compound;
#[cfg(feature = "ffi")]
pub mod nfs;
#[cfg(feature = "ffi")]
mod planner;
#[cfg(feature = "ffi")]
pub mod rpc;
#[cfg(feature = "ffi")]
pub mod session;

#[doc(hidden)]
#[cfg(feature = "ffi")]
pub mod error {
    pub use vfsi_core::{RpcError, RpcResult, STATUS_TRANSPORT};
}

#[doc(hidden)]
#[cfg(feature = "ffi")]
pub mod path {
    pub use vfsi_core::path::*;
}

#[doc(hidden)]
#[cfg(feature = "ffi")]
pub mod vecfs {
    pub use vfsi_sync::*;
}

#[cfg(feature = "ffi")]
pub use nfs::{NfsServerCopyStats, NfsVecFs};
