//! Synchronous filesystem contracts.
//!
//! Application code uses [`Vfsi`] and [`VfsiExt`]. Backend implementers use
//! [`backend::HandleBackend`] and [`backend::VectorBackend`]; shared defaults
//! live in [`backend::helpers`].

#[doc(hidden)]
pub use vfsi_core::path;
pub use vfsi_core::*;

mod contracts;
pub use contracts::{BackendDirectoryPage, DirPageCursor, DirectoryPage, ReadAllOptions};
mod application_macros;
mod client;
pub use client::{FsClient, FsDir, FsRead, FsReadInto, FsWrite, OpenFile};
/// Native execution contracts for backend implementers and language bindings.
/// Application callers use [`Vfsi`] and [`VfsiExt`].
pub mod backend;

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support;

// Preserve backend-facing paths while the canonical portable types live in core.
pub use vfsi_core::api::{
    DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
    DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
    DEFAULT_READV_MAX_TOTAL_BYTES, DEFAULT_WALK_MAX_DEPTH, DepthLimit, DirectoryListing,
    ListDirOptions, OpenOptions, ReadIntoResult as FsReadIntoResult, ReadOptions, ResourceLimits,
    StreamCompletion, StreamOptions, TraversalCompletion, WalkControl, WalkEvent, WalkEventKind,
    WriteResult as FsWriteResult, walk_events,
};

#[doc(hidden)]
pub mod application;

pub use vfsi_core::api::internal::OwnedReadResult as FsReadResult;
