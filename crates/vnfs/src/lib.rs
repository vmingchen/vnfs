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

/// Compatibility and protocol-construction APIs. New applications should use
/// the curated crate root or [`prelude`] instead.
pub mod legacy {
    pub use vfsi_core::*;
    pub use vfsi_sync::{VecFs, VecFsExt, VfFileHandle, VfOpenOptions, rm_recursive};

    #[cfg(feature = "nfs")]
    pub use vfsi_nfs::{client, compound, nfs, rpc, session};
}

/// Common Rust-native imports without protocol or FFI internals.
pub mod prelude {
    pub use vfsi_core::{
        BatchOutcome, Capabilities, MetadataQuery, OpOutcome, OpenFlags, OpenRequest, ReadOp,
        ReadResult, SetAttributes, VfError, VfFile, VfOffset, VfResult, WriteOpRef, WriteResult,
    };
    #[cfg(feature = "nfs")]
    pub use vfsi_nfs::{NfsClientBuilder, NfsEvent, NfsExtensions, NfsObserver, NfsVecFs};
    pub use vfsi_sync::{
        FileSystem, FsClient, FsFile, FsRead, FsReadInto, FsWrite, VectorFileSystem,
    };
}

pub use vfsi_core::*;
#[cfg(feature = "dummy")]
pub use vfsi_local::DummyVecFs;
#[cfg(all(feature = "nfs", feature = "rpcsec-gss"))]
pub use vfsi_nfs::RpcsecGssProtection;
#[cfg(feature = "nfs")]
pub use vfsi_nfs::{
    NfsAuthentication, NfsClientBuilder, NfsConnectOptions, NfsEvent, NfsExtensions, NfsObserver,
    NfsRecoveryPolicy, NfsServerCopyStats, NfsVecFs,
};
pub use vfsi_sync::{
    FileSystem, FsClient, FsFile, FsRead, FsReadInto, FsWrite, VecFs, VecFsExt, VectorFileSystem,
    VfFileHandle, VfOpenOptions, rm_recursive,
};
