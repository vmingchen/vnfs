//! Vectorized, synchronous NFSv4 for Rust applications.
//!
//! Start with `Nfs::connect` or `Nfs::builder`, then use scalar methods or
//! vector calls such as `NfsClient::openv`, `NfsClient::read_files`, and
//! `NfsClient::write_files`. Protocol internals live in [`backend`].

/// Application-facing result type for the Rust-native API.
pub type Result<T> = vfsi_core::VfResult<T>;

/// Application-facing error type for the Rust-native API.
pub type Error = vfsi_core::VfError;

mod application;
pub use application::{Client, FileHandle};

#[cfg(any(feature = "nfs", all(feature = "auto", target_os = "linux")))]
mod facade;
/// High-level filesystem workflows built on the application API.
pub mod helpers;
#[cfg(all(feature = "auto", target_os = "linux"))]
pub use facade::{
    Mounted, MountedDir, MountedFile, MountedOpenOptions, MountedRead, MountedReadInto,
    MountedSetMetadata, MountedWrite,
};
#[cfg(feature = "nfs")]
pub use facade::{
    NfsClient, NfsDir, NfsFile, NfsOpenOptions, NfsRead, NfsReadInto, NfsSetMetadata, NfsWrite,
};

/// Backend implementer and protocol-construction APIs. Most applications
/// need only the crate root; these are also available from the `vfsi-*` crates.
pub mod backend {
    pub use vfsi_core::*;
    #[doc(hidden)]
    pub use vfsi_sync::walk_events;
    pub use vfsi_sync::{
        CopyFileSystem, DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
        DEFAULT_READV_MAX_TOTAL_BYTES, DEFAULT_WALK_MAX_DEPTH, DirPageCursor, DirectoryFileSystem,
        FileSystem, FsClient, FsDir, FsFile, FsRead, FsReadInto, FsWrite, LinkFileSystem,
        MetadataFileSystem, NamespaceFileSystem, NativeFileSystem, OpenOptions, SetMetadata, VecFs,
        VecFsExt, VectorFileSystem, VfFileHandle, VfOpenOptions, rm_recursive,
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
    #[non_exhaustive]
    pub struct Snapshot {
        /// Compounds whose RPC call returned successfully; lost replies and
        /// other transport failures are not included.
        pub compounds: u64,
        pub operations: u64,
        /// None unless VNFS_STATS=1 was set before the first compound.
        /// Counts encoded NFS requests, not full transport records.
        pub compound_bytes: Option<u64>,
        pub max_operations: u64,
        pub rpc_calls: u64,
        pub rpc_micros: u64,
    }

    /// Drain process-wide counters. Consumers must coordinate: another call
    /// drains the same counters. Fields are sampled independently, so
    /// concurrent activity can straddle measurement windows.
    pub fn take_and_reset() -> Snapshot {
        let (compounds, operations, compound_bytes, max_operations) =
            vfsi_nfs::compound::compound_stats();
        let (rpc_calls, rpc_micros) = vfsi_nfs::compound::rpc_stats();
        Snapshot {
            compounds,
            operations,
            compound_bytes: vfsi_nfs::compound::compound_byte_stats_enabled()
                .then_some(compound_bytes),
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
pub use auto::{
    Auto, AutoClient, AutoDir, AutoFile, AutoOpenOptions, AutoRead, AutoReadInto, AutoRoute,
    AutoSetMetadata, AutoWrite,
};

/// Common application imports.
pub mod prelude {
    pub use crate::MetadataFields;
    #[cfg(all(feature = "auto", target_os = "linux"))]
    pub use crate::{Auto, Mounted};
    pub use crate::{
        Client, ControlFlow, FileHandle, ResourceLimits, StreamCompletion, TraversalCompletion,
    };
    #[cfg(feature = "nfs")]
    pub use crate::{Nfs, NfsAuthentication, NfsBuilder, NfsClient, NfsFile, NfsVersion};
    pub use vfsi_core::{OpenFlags, OpenRequest, RemoveOptions};
    pub use vfsi_sync::{ReadAllOptions, ReadDirOptions, ReadStreamOptions, WalkOptions};
}

pub use std::io::ErrorKind;
pub use std::ops::ControlFlow;
/// Attribute selection for metadata queries, directory listings, and walks.
pub use vfsi_core::AttrMask as MetadataFields;
pub use vfsi_core::VfType as FileType;
pub use vfsi_core::{
    Capabilities, DirEntry, ErrorDomain, Metadata, OpenFlags, OpenRequest, Permissions,
    RemoveOptions, StatusCode, TransportKind,
};
pub use vfsi_sync::{
    DirectoryListing, ReadAllOptions, ReadDirOptions, ReadStreamOptions, ResourceLimits,
    StreamCompletion, TraversalCompletion, WalkControl, WalkEvent, WalkEventKind, WalkOptions,
};
pub use vfsi_sync::{
    FsReadIntoResult as ReadIntoResult, FsReadResult as ReadResult, FsWriteResult as WriteResult,
};
#[cfg(feature = "nfs")]
mod native_nfs;
#[cfg(all(feature = "nfs", target_os = "linux"))]
pub use native_nfs::NfsMount;
#[cfg(feature = "nfs")]
pub use native_nfs::{Nfs, NfsBuilder, NfsClientPool, NfsVersion};
#[cfg(all(feature = "nfs", feature = "rpcsec-gss"))]
pub use vfsi_nfs::RpcsecGssProtection;
#[cfg(feature = "nfs")]
pub use vfsi_nfs::{
    NfsAuthentication, NfsEvent, NfsObserver, NfsReadPool, NfsReadPoolOptions, NfsRecoveryPolicy,
};
