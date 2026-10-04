#![cfg_attr(feature = "nfs", doc = include_str!("overview.md"))]
#![cfg_attr(
    not(feature = "nfs"),
    doc = "Backend-independent filesystem traits and helpers. Enable the `nfs` feature (enabled by default) for the direct NFS client and canonical examples. See [`files`], [`directory`], [`error`], and [`helpers`]."
)]

/// Direct NFSv4 connections, authentication, tuning, and owned handles.
/// Start with [`Nfs::builder`] for a server or [`Nfs::from_mount`] for an
/// existing Linux NFS mount. Mount discovery still uses a direct connection,
/// not the kernel client's cache.
#[cfg(feature = "nfs")]
pub mod nfs {
    #[doc(inline)]
    #[cfg(target_os = "linux")]
    pub use crate::NfsMount;
    #[cfg(feature = "rpcsec-gss")]
    pub use crate::RpcsecGssProtection;
    #[doc(inline)]
    pub use crate::{
        Nfs, NfsAuthentication, NfsBuilder, NfsClient, NfsClientPool, NfsDir, NfsEvent, NfsFile,
        NfsObserver, NfsOpenOptions, NfsRead, NfsReadInto, NfsReadPool, NfsReadPoolOptions,
        NfsRecoveryPolicy, NfsSetMetadata, NfsVersion,
    };
}

/// Linux-mounted paths and opt-in automatic direct NFS routing.
/// [`Mounted`] always uses the kernel. [`Auto`] can use a separate direct NFS
/// client; it does **not** provide cache coherence with kernel access.
#[cfg(all(feature = "auto", target_os = "linux"))]
pub mod mounted {
    #[doc(inline)]
    pub use crate::{
        Auto, AutoClient, AutoDir, AutoFile, AutoOpenOptions, AutoRead, AutoReadInto, AutoRoute,
        AutoSetMetadata, Mounted, MountedDir, MountedFile, MountedOpenOptions, MountedRead,
        MountedReadInto, MountedSetMetadata,
    };
}

/// Backend-independent file I/O, allocation budgets, and vector result types.
pub mod files {
    #[doc(inline)]
    pub use crate::{
        Capabilities, FileHandle, Fs, FsExt, OpenFlags, OpenRequest, ReadIntoResult, ReadOp,
        ReadOptions, ReadResult, ReadStreamOptions, ResourceLimits, StreamCompletion, WriteOp,
        WriteOptions, WriteResult,
    };
}

/// Metadata, bounded directory listings, traversal, and removal policies.
pub mod directory {
    #[doc(inline)]
    pub use crate::{
        ControlFlow, DirEntry, DirectoryListing, FileType, Metadata, MetadataFields,
        MetadataOptions, Permissions, ReadDirOptions, RemoveMode, RemoveOptions,
        TraversalCompletion, VisitOptions, WalkControl, WalkEvent, WalkEventKind, WalkOptions,
    };
}

/// Error classification without losing protocol status or the failing input index.
pub mod error {
    #[doc(inline)]
    pub use crate::{Error, ErrorDomain, ErrorKind, Result, StatusCode, TransportKind};
}

/// Canonical, runnable application examples. Each listing is compiled as a
/// doctest and as a Cargo example; workflows are also tested on mounted files.
/// None requires importing backend crates. See the repository's
/// `crates/vnfs/examples/README.md` for commands and benchmark programs.
#[cfg(feature = "nfs")]
pub mod examples {
    /// Batch complete small files, with explicit fresh-directory ownership.
    #[doc = concat!("```no_run\n", include_str!("../examples/bulk_files.rs"), "\n```")]
    pub mod bulk_files {}
    /// Read ranges into caller-owned buffers and explicitly close handles.
    #[doc = concat!("```no_run\n", include_str!("../examples/open_handles.rs"), "\n```")]
    pub mod open_handles {}
    /// Process a large file with bounded memory rather than collecting it.
    #[doc = concat!("```no_run\n", include_str!("../examples/stream_file.rs"), "\n```")]
    pub mod stream_file {}
    /// Batch directory metadata or visit a tree incrementally.
    #[doc = concat!("```no_run\n", include_str!("../examples/directories.rs"), "\n```")]
    pub mod directories {}
}

/// Application-facing result type for the Rust-native API.
pub type Result<T> = vfsi_core::VfResult<T>;

/// Application-facing error type for the Rust-native API.
pub type Error = vfsi_core::VfError;

mod application;
pub use application::{FileHandle, Fs, FsExt};
mod metadata;
mod read;
mod visit;
mod write;
pub use metadata::MetadataOptions;
#[cfg(any(feature = "nfs", all(feature = "auto", target_os = "linux")))]
pub(crate) use read::ReadRequest;
pub use read::{ReadOp, ReadOptions, ReadResult};
pub(crate) use vfsi_sync::FsReadResult as OwnedReadResult;
pub use visit::VisitOptions;
pub use write::{WriteOp, WriteOptions};

// Keep negative API-contract doctests without presenting unsupported calls
// as introductory documentation on the Nfs constructor.
#[cfg(feature = "nfs")]
#[doc(hidden)]
#[doc = include_str!("api_boundary.md")]
mod api_contract {}

// Compile the published README examples without duplicating them in rustdoc.
#[cfg(feature = "nfs")]
#[doc(hidden)]
#[doc = include_str!("../README.md")]
mod readme_examples {}

#[cfg(any(feature = "nfs", all(feature = "auto", target_os = "linux")))]
mod facade;
/// High-level filesystem workflows built on the application API.
pub mod helpers;
#[cfg(all(feature = "auto", target_os = "linux"))]
pub use facade::{
    Mounted, MountedDir, MountedFile, MountedOpenOptions, MountedRead, MountedReadInto,
    MountedSetMetadata,
};
#[cfg(feature = "nfs")]
pub use facade::{
    NfsClient, NfsDir, NfsFile, NfsOpenOptions, NfsRead, NfsReadInto, NfsSetMetadata,
};

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
    AutoSetMetadata,
};

/// Common application imports.
pub mod prelude {
    pub use crate::MetadataFields;
    #[cfg(all(feature = "auto", target_os = "linux"))]
    pub use crate::{Auto, Mounted};
    pub use crate::{
        ControlFlow, FileHandle, Fs, FsExt, MetadataOptions, ReadOp, ReadOptions, ReadResult,
        RemoveMode, ResourceLimits, StreamCompletion, TraversalCompletion, VisitOptions, WriteOp,
        WriteOptions,
    };
    #[cfg(feature = "nfs")]
    pub use crate::{Nfs, NfsAuthentication, NfsBuilder, NfsClient, NfsFile, NfsVersion};
    pub use vfsi_core::{OpenFlags, OpenRequest, RemoveOptions};
    pub use vfsi_sync::{ReadDirOptions, ReadStreamOptions, WalkOptions};
}

pub use std::io::ErrorKind;
pub use std::ops::ControlFlow;
/// What each input to [`Fs::removev_with_options`] removes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RemoveMode {
    /// Remove a file, symlink, or empty directory; do not traverse directories.
    #[default]
    Entry,
    /// Remove the entry and, for directories, its descendants.
    Tree,
    /// Remove descendants while retaining the directory root. Final symlinks
    /// are not followed; execution uses the backend's anchored removal path.
    Contents,
}
/// Attribute selection for metadata queries, directory listings, and walks.
pub use vfsi_core::AttrMask as MetadataFields;
pub use vfsi_core::VfType as FileType;
pub use vfsi_core::{
    Capabilities, DirEntry, ErrorDomain, Metadata, OpenFlags, OpenRequest, Permissions,
    RemoveOptions, StatusCode, TransportKind,
};
pub(crate) use vfsi_sync::ReadAllOptions;
pub use vfsi_sync::{
    DirectoryListing, ReadDirOptions, ReadStreamOptions, ResourceLimits, StreamCompletion,
    TraversalCompletion, WalkControl, WalkEvent, WalkEventKind, WalkOptions,
};
pub use vfsi_sync::{FsReadIntoResult as ReadIntoResult, FsWriteResult as WriteResult};
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
