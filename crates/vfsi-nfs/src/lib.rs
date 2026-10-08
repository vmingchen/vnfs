//! NFSv4.1 and NFSv4.2 backend for VFSI.

#[cfg(feature = "ffi")]
pub mod client;
#[cfg(feature = "ffi")]
pub mod compound;
#[cfg(feature = "ffi")]
mod identity;
#[cfg(all(feature = "ffi", target_os = "linux"))]
pub mod mount;
#[cfg(feature = "ffi")]
pub mod nfs;
#[cfg(feature = "ffi")]
mod planner;
#[cfg(feature = "ffi")]
pub mod rpc;
#[cfg(feature = "ffi")]
pub mod session;

/// Stable entry points used by the out-of-tree libFuzzer targets.
#[doc(hidden)]
#[cfg(feature = "fuzzing")]
pub mod fuzzing;

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

#[cfg(all(feature = "ffi", feature = "rpcsec-gss"))]
pub use nfs::RpcsecGssProtection;
#[cfg(feature = "ffi")]
pub use nfs::{
    NfsAuthentication, NfsClientBuilder, NfsConnectOptions, NfsEvent, NfsObserver, NfsReadPool,
    NfsReadPoolOptions, NfsRecoveryPolicy, NfsServerCopyStats, NfsVecFs,
};
