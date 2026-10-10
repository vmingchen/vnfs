#![cfg_attr(feature = "nfs", doc = include_str!("overview.md"))]
#![cfg_attr(
    not(feature = "nfs"),
    doc = "Backend-independent filesystem traits and helpers. Enable the `nfs` feature (enabled by default) for the direct NFS client and canonical examples. See [`files`], [`directory`], [`error`], and [`helpers`]."
)]

/// Direct NFSv4 connections, authentication, tuning, and owned handles.
/// Start with Nfs::builder for a server. Linux mount discovery and path
/// mapping live under the mount submodule.
#[cfg(feature = "nfs")]
#[doc = include_str!("guides/standard_io.md")]
#[doc = include_str!("guides/failure_recovery.md")]
#[doc = include_str!("guides/authentication.md")]
#[doc = include_str!("guides/operations.md")]
pub mod nfs {
    pub use crate::facade::{NfsClient, NfsDir, NfsFile};
    pub use crate::native_nfs::{Nfs, NfsBuilder, NfsClientPool, NfsVersion};
    #[cfg(feature = "rpcsec-gss")]
    pub use vfsi_nfs::RpcsecGssProtection;
    pub use vfsi_nfs::{
        NfsAuthentication, NfsEvent, NfsObserver, NfsReadPool, NfsReadPoolOptions,
        NfsRecoveryPolicy,
    };

    /// Linux mount discovery and local-to-remote path mapping for direct NFS access.
    #[cfg(target_os = "linux")]
    pub mod mount;
}

/// Linux-mounted paths and opt-in automatic direct NFS routing.
/// crate::posix::Posix always uses the kernel. Auto can use a separate direct NFS
/// client; it does not provide cache coherence with kernel access.
#[cfg(all(feature = "auto", target_os = "linux"))]
pub mod mounted {
    pub use crate::auto::{Auto, AutoDir, AutoFile, AutoRoute};
}

/// Rooted filesystem access through ordinary POSIX syscalls (posix feature).
#[cfg(all(feature = "posix", unix))]
pub mod posix {
    pub use crate::facade::{Posix, PosixDir, PosixFile};
}

/// Backend-independent file I/O, allocation budgets, and vector result types.
#[cfg_attr(
    feature = "nfs",
    doc = "\n\n## Runnable examples\n\n### Bulk files\n\nBatch complete small files while preserving input order.\n"
)]
#[cfg_attr(feature = "nfs", doc = concat!("\n```no_run\n", include_str!("../examples/bulk_files.rs"), "\n```\n"))]
#[cfg_attr(
    feature = "nfs",
    doc = "\n### Open handles\n\nRead ranges into caller-owned buffers and close handles explicitly.\n"
)]
#[cfg_attr(feature = "nfs", doc = concat!("\n```no_run\n", include_str!("../examples/open_handles.rs"), "\n```\n"))]
#[cfg_attr(
    feature = "nfs",
    doc = "\n### Bounded streaming\n\nProcess a large file without collecting it in memory.\n"
)]
#[cfg_attr(feature = "nfs", doc = concat!("\n```no_run\n", include_str!("../examples/stream_file.rs"), "\n```\n"))]
pub mod files {
    pub use vfsi_core::api::{
        OpenOptions, ReadOp, ReadOptions, ReadResult, ResourceLimits, StreamCompletion,
        StreamOptions, SyncMode, WriteOp, WriteOptions, WriteResult,
    };
    pub use vfsi_core::{
        AsTarget, Capabilities, CopyOption, FileHandle, FilesystemStats, OpenFlags, OpenOp, Target,
        Vfsi, VfsiExt,
    };
}

/// Attrs, bounded directory listings, traversal, and removal policies.
#[cfg_attr(
    feature = "nfs",
    doc = "\n\n## Runnable example\n\nBatch directory metadata or visit a tree incrementally.\n"
)]
#[cfg_attr(feature = "nfs", doc = concat!("\n```no_run\n", include_str!("../examples/directories.rs"), "\n```\n"))]
pub mod directory {
    pub use std::ops::ControlFlow;
    pub use vfsi_core::api::{
        AttrsOptions, DepthLimit, DirHandle, DirectoryListing, ListDirOptions, MkDirOp, RemoveMode,
        RemoveOptions, RenameOptions, SetAttrsOp, TraversalCompletion, WalkControl, WalkEvent,
        WalkEventKind,
    };
    pub use vfsi_core::{AttrMask as Attributes, Attrs, DirEntry, Permissions, VfType as FileType};
}

/// Error classification without losing protocol status or the failing input index.
pub mod error {
    pub use crate::{Error, Result};
    pub use vfsi_core::api::ErrorKind;
    pub use vfsi_core::{ErrorDomain, StatusCode, TransportKind};
}

/// Application-facing result type for the Rust-native API.
pub type Result<T> = vfsi_core::VfResult<T>;

/// Application-facing error type for the Rust-native API.
pub type Error = vfsi_core::VfError;

#[cfg(any(
    feature = "nfs",
    all(feature = "posix", unix),
    all(any(feature = "auto", feature = "uring"), target_os = "linux")
))]
mod application;
// The root keeps only the core application traits, errors, and result alias.
// Other public types have one canonical path under files, directory, error,
// or a backend-specific namespace. Crate-private aliases keep implementation
// modules concise without expanding the external API.
pub(crate) use vfsi_core::{DirHandle, FileHandle, Target};
pub use vfsi_core::{Vfsi, VfsiExt};
#[cfg(any(
    feature = "nfs",
    all(feature = "posix", unix),
    all(any(feature = "auto", feature = "uring"), target_os = "linux")
))]
mod read;
#[cfg(all(feature = "auto", target_os = "linux"))]
pub(crate) use read::ReadRequest;
#[cfg(all(feature = "auto", target_os = "linux"))]
pub(crate) use vfsi_core::api::internal::OwnedReadResult;
pub(crate) use vfsi_core::api::{
    AttrsOptions, ListDirOptions, ReadOp, ReadOptions, ReadResult, RenameOptions, SyncMode,
    WriteOp, WriteOptions,
};

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

#[cfg(any(
    feature = "nfs",
    all(feature = "posix", unix),
    all(any(feature = "auto", feature = "uring"), target_os = "linux")
))]
mod facade;
/// High-level filesystem workflows built on the application API.
pub mod helpers;
#[cfg(feature = "nfs")]
pub(crate) use facade::{NfsClient, NfsDir, NfsFile};
#[cfg(all(feature = "posix", unix))]
pub(crate) use facade::{Posix, PosixDir, PosixFile};

/// Batched local descriptor I/O through Linux io_uring (opt-in `uring` feature).
#[cfg(all(feature = "uring", target_os = "linux"))]
#[doc = include_str!("guides/uring.md")]
pub mod uring {
    pub use crate::facade::{Uring, UringDir, UringFile};
    pub use vfsi_uring::{Options, Stats, Telemetry};
}
#[cfg(all(feature = "uring", target_os = "linux"))]
pub(crate) use uring::Uring;

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
pub(crate) use auto::{Auto, AutoDir, AutoFile};

/// Minimal backend-independent imports for generic application code.
/// Import concrete operations from files and directory, and choose a backend
/// explicitly from nfs, posix, mounted, or uring.
pub mod prelude {
    pub use crate::{Error, Result, Vfsi, VfsiExt};
}

// Internal compatibility imports for implementation modules only. These are
// deliberately crate-private; external callers use the grouped namespaces.
#[cfg(test)]
pub(crate) use std::io::ErrorKind;
pub(crate) use std::ops::ControlFlow;
pub(crate) use vfsi_core::AttrMask as Attributes;
#[cfg(all(feature = "auto", target_os = "linux"))]
pub(crate) use vfsi_core::DirEntry;
pub(crate) use vfsi_core::VfType as FileType;
pub(crate) use vfsi_core::api::{
    DepthLimit, DirectoryListing, MkDirOp, RemoveMode, RemoveOptions, ResourceLimits, SetAttrsOp,
    StreamCompletion, StreamOptions, TraversalCompletion, WriteResult,
};
pub(crate) use vfsi_core::{
    Attrs, Capabilities, CopyOption, FilesystemStats, OpenFlags, OpenOp, TransportKind,
};
#[cfg(all(feature = "auto", target_os = "linux"))]
pub(crate) use vfsi_sync::ReadAllOptions;
#[cfg(feature = "nfs")]
mod native_nfs;
#[cfg(all(feature = "auto", target_os = "linux"))]
pub(crate) use native_nfs::{Nfs, NfsVersion};

#[cfg(all(feature = "auto", target_os = "linux"))]
pub(crate) use vfsi_core::api::ReadIntoResult;
