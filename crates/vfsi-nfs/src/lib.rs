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

#[cfg(all(feature = "ffi", feature = "rpcsec-gss"))]
pub use nfs::RpcsecGssProtection;
#[cfg(feature = "ffi")]
pub use nfs::{
    NfsAuthentication, NfsClientBuilder, NfsConnectOptions, NfsEvent, NfsObserver,
    NfsRecoveryPolicy, NfsServerCopyStats, NfsVecFs,
};

/// NFS-only negotiated state, kept out of protocol-neutral VFSI traits.
#[cfg(feature = "ffi")]
pub trait NfsExtensions {
    fn nfs_minor_version(&self) -> u32;
    fn server_copy_enabled(&self) -> bool;
    fn server_copy_stats(&self) -> NfsServerCopyStats;
}

#[cfg(feature = "ffi")]
impl NfsExtensions for NfsVecFs {
    fn nfs_minor_version(&self) -> u32 {
        self.minorversion()
    }
    fn server_copy_enabled(&self) -> bool {
        self.server_copy_enabled()
    }
    fn server_copy_stats(&self) -> NfsServerCopyStats {
        self.server_copy_stats()
    }
}
