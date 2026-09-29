//! Vectorized, synchronous NFSv4 for Rust applications.
//!
//! Start with `Nfs::connect` or `Nfs::builder`, then use scalar methods or
//! vector calls such as `NfsClient::openv`, `NfsClient::read_files`, and
//! `NfsClient::write_files`. Protocol internals live in [`backend`].

/// Application-facing result type for the Rust-native API.
pub type Result<T> = vfsi_core::VfResult<T>;

/// Application-facing error type for the Rust-native API.
pub type Error = vfsi_core::VfError;

/// Backend implementer and protocol-construction APIs. Most applications
/// need only the crate root; these are also available from the `vfsi-*` crates.
pub mod backend {
    pub use vfsi_core::*;
    pub use vfsi_sync::{
        CopyFileSystem, DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
        DEFAULT_READV_MAX_TOTAL_BYTES, DEFAULT_WALK_MAX_DEPTH, DirPageCursor, DirectoryFileSystem,
        FileSystem, FsRead, FsReadInto, FsWrite, LinkFileSystem, MetadataFileSystem,
        NamespaceFileSystem, NativeFileSystem, SetMetadata, VecFs, VecFsExt, VectorFileSystem,
        VfFileHandle, VfOpenOptions, rm_recursive,
    };

    #[cfg(feature = "dummy")]
    pub use vfsi_local::DummyVecFs;
    #[cfg(feature = "nfs")]
    pub use vfsi_nfs::{NfsVecFs, client, compound, nfs, rpc, session};
}

/// Aggregate NFS transport counters for optional application diagnostics.
/// These counters are process-wide, not per client, and may include other
/// concurrent NFS clients in the same process.
#[cfg(feature = "nfs")]
pub mod diagnostics {
    /// Snapshot of process-wide NFS compound and RPC activity.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Snapshot {
        pub compounds: u64,
        pub operations: u64,
        pub compound_bytes: u64,
        pub max_operations: u64,
        pub rpc_calls: u64,
        pub rpc_micros: u64,
    }

    pub fn snapshot() -> Snapshot {
        let (compounds, operations, compound_bytes, max_operations) =
            vfsi_nfs::compound::compound_stats();
        let (rpc_calls, rpc_micros) = vfsi_nfs::compound::rpc_stats();
        Snapshot {
            compounds,
            operations,
            compound_bytes,
            max_operations,
            rpc_calls,
            rpc_micros,
        }
    }
}

/// Linux-mounted path routing with conservative automatic direct NFSv4 selection.
#[cfg(all(feature = "auto", target_os = "linux"))]
mod auto;
#[cfg(all(feature = "auto", target_os = "linux"))]
pub use auto::{Auto, AutoClient, AutoFile, AutoRead, AutoRoute, AutoWrite, Mounted};

/// Common application imports.
pub mod prelude {
    pub use crate::MetadataFields;
    #[cfg(all(feature = "auto", target_os = "linux"))]
    pub use crate::{Auto, Mounted};
    #[cfg(feature = "nfs")]
    pub use crate::{Nfs, NfsAuthentication, NfsBuilder, NfsClient, NfsFile};
    pub use vfsi_core::{OpenFlags, OpenRequest, RemoveOptions};
    pub use vfsi_sync::{ReadAllOptions, ReadDirOptions, ReadStreamOptions, WalkOptions};
}

/// Attribute selection for metadata queries, directory listings, and walks.
pub use vfsi_core::AttrMask as MetadataFields;
pub use vfsi_core::{
    Capabilities, DirEntry, Metadata, OpenFlags, OpenRequest, Permissions, ReadResult,
    RemoveOptions, VfError, VfResult, VfType, WriteResult,
};
pub use vfsi_sync::{
    DirectoryListing, FsClient, FsDir, FsFile, OpenOptions, ReadAllOptions, ReadDirOptions,
    ReadStreamOptions, WalkOptions,
};
#[cfg(feature = "nfs")]
mod native_nfs;
#[cfg(feature = "nfs")]
pub use native_nfs::{Nfs, NfsBuilder, NfsClient, NfsClientPool, NfsFile};
#[cfg(all(feature = "nfs", feature = "rpcsec-gss"))]
pub use vfsi_nfs::RpcsecGssProtection;
#[cfg(feature = "nfs")]
pub use vfsi_nfs::{
    NfsAuthentication, NfsEvent, NfsObserver, NfsReadPool, NfsReadPoolOptions, NfsRecoveryPolicy,
};
