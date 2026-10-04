//! Portable synchronous vectorized filesystem API.
//!
//! Implement [`Vfsi`] for native vector execution; [`VfsiExt`] is blanket
//! implemented and composes scalar conveniences and workflows from those vectors.
//! This module has no backend, native RPC, or runtime dependencies.
//!
//! Application read/write operations live here rather than sharing the legacy
//! backend operation types at the crate root.

pub type Result<T> = crate::VfResult<T>;
pub type Error = crate::VfError;
pub use crate::{
    AttrMask as MetadataFields, Capabilities, DirEntry, ErrorDomain, Metadata, MetadataUpdate,
    OpenFlags, OpenRequest, Permissions, RemoveOptions, StatusCode, TransportKind,
    VfType as FileType,
};
pub use std::io::ErrorKind;
pub use std::ops::ControlFlow;
mod traits;
pub use traits::{FileHandle, Vfsi, VfsiExt};
mod metadata;
pub use metadata::MetadataOptions;
mod read;
pub use read::{ReadOp, ReadOptions, ReadResult};
mod write;
pub use write::{WriteOp, WriteOptions};
mod visit;
pub use visit::VisitOptions;
mod support;
pub use support::{
    DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
    DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
    DEFAULT_READV_MAX_TOTAL_BYTES, DEFAULT_WALK_MAX_DEPTH, DepthLimit, DirectoryListing,
    ReadDirOptions, ReadStreamOptions, ResourceLimits, StreamCompletion, TraversalCompletion,
    WalkOptions,
};
pub use support::{ReadIntoResult, WriteResult};
mod traversal;
#[doc(hidden)]
pub use traversal::walk_events;
pub use traversal::{WalkControl, WalkEvent, WalkEventKind};
/// What each input to [`Vfsi::vremove`] removes.
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

/// Unstable dispatch projections for workspace backend implementations.
#[doc(hidden)]
pub mod internal {
    pub use super::read::{ReadRequest, consume_ops, read_batch};
    pub use super::support::FsReadResult as OwnedReadResult;
}

/// Common imports for backend-independent application code.
pub mod prelude {
    pub use super::{
        ControlFlow, FileHandle, MetadataFields, MetadataOperand, MetadataOptions, MetadataTarget,
        MetadataUpdate, OpenFlags, OpenRequest, ReadOp, ReadOptions, ReadResult, ReadStreamOptions,
        RemoveMode, RemoveOptions, ResourceLimits, Vfsi, VfsiExt, VisitOptions, WriteOp,
        WriteOptions,
    };
}

/// A path or an opened object for an attribute update. Handle targets retain
/// object identity across rename/unlink; `follow_symlinks` applies only to paths.
pub enum MetadataTarget<'a, F> {
    Path(&'a std::path::Path),
    File(&'a F),
}
impl<F> MetadataTarget<'_, F> {
    pub fn path(path: &impl AsRef<std::path::Path>) -> MetadataTarget<'_, F> {
        MetadataTarget::Path(path.as_ref())
    }
    pub fn file(file: &F) -> MetadataTarget<'_, F> {
        MetadataTarget::File(file)
    }
}
/// Inputs accepted by [`Vfsi::vsetattrs`]. Paths can be passed directly;
/// use [`MetadataTarget`] to submit handles or mixed path/handle vectors.
pub trait MetadataOperand<F> {
    fn metadata_target(&self) -> MetadataTarget<'_, F>;
}
impl<F, P: AsRef<std::path::Path>> MetadataOperand<F> for P {
    fn metadata_target(&self) -> MetadataTarget<'_, F> {
        MetadataTarget::Path(self.as_ref())
    }
}
impl<F> MetadataOperand<F> for MetadataTarget<'_, F> {
    fn metadata_target(&self) -> MetadataTarget<'_, F> {
        match self {
            Self::Path(path) => MetadataTarget::Path(path),
            Self::File(file) => MetadataTarget::File(file),
        }
    }
}
