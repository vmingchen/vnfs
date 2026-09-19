//! VFSI compatibility facade.
//!
//! The published `vnfs` package retains its historical paths while the
//! implementation is split into interface and backend workspace crates.

/// Application-facing result type for the Rust-native API.
pub type Result<T> = vfsi_core::VfResult<T>;

/// Application-facing error type for the Rust-native API.
pub type Error = vfsi_core::VfError;

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
    #[cfg(feature = "nfs")]
    pub use crate::{Nfs, NfsBuilder, NfsClient, NfsFile};
    pub use vfsi_core::{
        Capabilities, DirEntry, Metadata, OpenFlags, OpenRequest, Permissions, ReadResult, VfError,
        VfResult, WriteResult,
    };
    pub use vfsi_sync::{
        CopyFileSystem, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES,
        DirectoryFileSystem, FileSystem, FsClient, FsFile, LinkFileSystem, MetadataFileSystem,
        NamespaceFileSystem, NativeFileSystem, OpenOptions, ReadAllOptions, ReadDirOptions,
        SetMetadata, VectorFileSystem, WalkOptions,
    };
}

pub use vfsi_core::*;
#[cfg(feature = "nfs")]
mod native_nfs;
#[cfg(feature = "nfs")]
pub use native_nfs::{Nfs, NfsBuilder, NfsClient, NfsFile};
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
    CopyFileSystem, DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
    DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_WALK_MAX_DEPTH,
    DirectoryFileSystem, FileSystem, FsClient, FsFile, FsRead, FsReadInto, FsWrite, LinkFileSystem,
    MetadataFileSystem, NamespaceFileSystem, NativeFileSystem, OpenOptions, ReadAllOptions,
    ReadDirOptions, SetMetadata, VecFs, VecFsExt, VectorFileSystem, VfFileHandle, VfOpenOptions,
    WalkOptions, rm_recursive,
};
