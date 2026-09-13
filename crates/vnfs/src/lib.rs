//! VFSI compatibility facade.
//!
//! The published `vnfs` package retains its historical paths while the
//! implementation is split into interface and backend workspace crates.

/// Shared low-level error compatibility path.
pub mod error {
    pub use vfsi_core::{RpcError, RpcResult, STATUS_TRANSPORT};
}

/// Synchronous vector interface compatibility path.
pub mod vecfs {
    pub use vfsi_sync::*;
}

/// Scalar/singular synchronous API facet.
pub mod sfsi {
    pub use vfsi_sync::sfsi::*;
}

/// Vectorized synchronous API facet.
pub mod vfsi {
    pub use vfsi_sync::vfsi::*;
}

#[cfg(feature = "dummy")]
pub mod dummy_vecfs {
    pub use vfsi_local::DummyVecFs;
}

#[cfg(feature = "nfs")]
pub use vfsi_nfs::{client, compound, nfs, rpc, session};

pub use vfsi_core::*;
#[cfg(feature = "dummy")]
pub use vfsi_local::DummyVecFs;
#[cfg(feature = "nfs")]
pub use vfsi_nfs::{NfsRecoveryPolicy, NfsServerCopyStats, NfsVecFs};
pub use vfsi_sync::{VecFs, VecFsExt, VfFileHandle, VfOpenOptions, rm_recursive};
