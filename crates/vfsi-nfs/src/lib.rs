//! NFSv4.1 and NFSv4.2 backend for VFSI.
//!
//! Shared requests and errors belong to `vfsi-core`; native backend contracts
//! and helpers belong to `vfsi-sync`. This crate exposes NFS-specific APIs.

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

#[cfg(feature = "ffi")]
pub mod telemetry;

/// Stable entry points used by the out-of-tree libFuzzer targets.
#[doc(hidden)]
#[cfg(feature = "fuzzing")]
pub mod fuzzing;

#[cfg(all(feature = "ffi", feature = "rpcsec-gss"))]
pub use nfs::RpcsecGssProtection;
#[cfg(feature = "ffi")]
pub use nfs::{
    NfsAuth, NfsClientBuilder, NfsConnectOptions, NfsEvent, NfsObserver, NfsReadPool,
    NfsReadPoolOptions, NfsRecoveryPolicy, NfsServerCopyStats, NfsVecFs,
};

#[cfg(feature = "ffi")]
#[doc(hidden)]
mod shared_contract_boundary {
    //! Shared types and helpers have one owner instead of protocol aliases.
    //!
    //! ```compile_fail,E0432
    //! use vfsi_nfs::vecfs;
    //! ```
    //!
    //! ```compile_fail,E0432
    //! use vfsi_nfs::path;
    //! ```
    //!
    //! ```compile_fail,E0432
    //! use vfsi_nfs::error;
    //! ```
    //!
    //! ```compile_fail,E0603
    //! use vfsi_nfs::nfs::FsClient;
    //! ```
}
