//! Synchronous filesystem contracts.
//!
//! `FileSystem` supports owned handles. `Backend` adds vector engines and
//! overridable workflows; shared helpers compose those engines.

pub use vfsi_core::*;

#[doc(hidden)]
pub mod path {
    pub use vfsi_core::path::*;
}

mod traits;
pub use traits::{BackendDirectoryPage, DirPageCursor, DirectoryPage, ReadAllOptions};
mod io;
pub use io::{VfFileHandle, VfOpenOptions};
mod client;
mod traversal;
pub use client::{FsClient, FsDir, FsFile, FsRead, FsReadInto, FsWrite, OpenOptions, SetMetadata};
pub use traversal::walk_events;
#[doc(hidden)]
pub mod backend_helpers;
mod native;
pub use native::{Backend, FileSystem};

/// Scalar/singular view of the synchronous interface.
pub mod sfsi {
    pub use crate::{
        DepthLimit, DirPageCursor, FileSystem, FsClient, FsFile, OpenOptions, ReadDirOptions,
        ReadStreamOptions, WalkOptions,
    };
    pub use vfsi_core::{Fd, VfAttrs, VfError, VfFile, VfOffset, VfResult, VfType};
}

/// Vectorized view of the synchronous interface.
pub mod vfsi {
    pub use crate::{
        Backend, DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
        DEFAULT_WALK_MAX_DEPTH, DepthLimit, DirPageCursor, ReadAllOptions, ReadDirOptions,
        ReadStreamOptions, WalkOptions,
    };
    pub use vfsi_core::*;
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support;

// Preserve backend-facing paths while the canonical portable types live in core.
pub use vfsi_core::api::{
    DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
    DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
    DEFAULT_READV_MAX_TOTAL_BYTES, DEFAULT_WALK_MAX_DEPTH, DepthLimit, DirectoryListing,
    ReadDirOptions, ReadIntoResult as FsReadIntoResult, ReadStreamOptions, ResourceLimits,
    StreamCompletion, TraversalCompletion, WalkControl, WalkEvent, WalkEventKind, WalkOptions,
    WriteResult as FsWriteResult,
};

#[doc(hidden)]
pub mod application;

pub use vfsi_core::api::internal::OwnedReadResult as FsReadResult;
