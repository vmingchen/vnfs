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
    AttrMask as Attributes, Attrs, Capabilities, CopyOption, DirEntry, ErrorDomain,
    FilesystemStats, OpenFlags, OpenOp, Permissions, RemoveOptions, StatusCode, TransportKind,
    VfType as FileType,
};
/// Per-pair rename semantics requested by [`Vfsi::vrename`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenameOptions {
    /// Replace a destination according to ordinary filesystem rename semantics.
    #[default]
    Replace,
    /// Fail atomically if the destination already exists.
    NoReplace,
    /// Atomically exchange the source and destination names.
    /// Both paths must exist. This is available only on backends that provide
    /// native exchange semantics; it is never emulated as multiple operations.
    Exchange,
}
pub use std::io::ErrorKind;
pub use std::ops::ControlFlow;
mod io;
pub use io::{FileIo, SyncMode};
mod builders;
pub use builders::OpenOptions;
mod traits;
pub use traits::{DirHandle, FileHandle, Vfsi, VfsiExt};
mod metadata;
pub use metadata::{AttrsOptions, SetAttrsOp};
mod mkdir;
pub use mkdir::MkDirOp;
mod read;
pub use read::{ReadOp, ReadOptions, ReadResult};
mod write;
pub use write::{WriteOp, WriteOptions};
mod visit;
pub use visit::ListDirOptions;
mod support;
pub use support::{
    DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
    DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
    DEFAULT_READV_MAX_TOTAL_BYTES, DEFAULT_WALK_MAX_DEPTH, DepthLimit, DirectoryListing,
    ResourceLimits, StreamCompletion, StreamOptions, TraversalCompletion,
};
pub use support::{ReadIntoResult, WriteResult};
mod listdir;
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
        AsTarget, Attributes, AttrsOptions, ControlFlow, CopyOption, DirHandle, FileHandle,
        ListDirOptions, MkDirOp, OpenFlags, OpenOp, ReadOp, ReadOptions, ReadResult, RemoveMode,
        RemoveOptions, RenameOptions, ResourceLimits, SetAttrsOp, StreamOptions, SyncMode, Target,
        Vfsi, VfsiExt, WriteOp, WriteOptions,
    };
}

/// Borrowed filesystem operand: a path to resolve or an already-open object.
/// Used by attribute updates, filesystem statistics, and scalar conveniences.
/// Handle targets retain object identity across rename/unlink; path symlink
/// policies do not change that identity. A target never owns or closes a handle.
#[derive(Debug)]
pub enum Target<'a, F> {
    Path(&'a std::path::Path),
    File(&'a F),
}
impl<F> Copy for Target<'_, F> {}
impl<F> Clone for Target<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F> Target<'_, F> {
    pub fn path(path: &impl AsRef<std::path::Path>) -> Target<'_, F> {
        Target::Path(path.as_ref())
    }
    pub fn file(file: &F) -> Target<'_, F> {
        Target::File(file)
    }
}
/// Borrow a target for metadata queries, attribute updates or filesystem statistics.
/// Paths can be passed directly; use [`Target`] for handles or mixed vectors.
/// Conversion performs no I/O. Clients convert each operand once at preflight
/// and retain that target for routing, execution, and error context.
pub trait AsTarget<F> {
    fn as_target(&self) -> Target<'_, F>;
}
impl<F, P: AsRef<std::path::Path>> AsTarget<F> for P {
    fn as_target(&self) -> Target<'_, F> {
        Target::Path(self.as_ref())
    }
}
impl<F> AsTarget<F> for Target<'_, F> {
    fn as_target(&self) -> Target<'_, F> {
        match self {
            Self::Path(path) => Target::Path(path),
            Self::File(file) => Target::File(file),
        }
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;
    #[test]
    fn borrowed_targets_and_handle_operations_do_not_require_cloning_handles() {
        struct NonClone;
        fn copy<T: Copy>(value: T) -> (T, T) {
            (value, value)
        }
        let file = NonClone;
        let (first, second) = copy(Target::file(&file));
        for target in [first, second.as_target()] {
            assert!(matches!(target, Target::File(actual) if std::ptr::eq(actual, &file)));
        }
        let (first, second) = copy(SetAttrsOp::file(&file).len(0).follow_symlinks(false));
        for op in [first, second] {
            assert_eq!(op.requested_len(), Some(0));
            assert!(!op.follows_symlinks());
            assert!(matches!(op.target(), Target::File(actual) if std::ptr::eq(*actual, &file)));
        }
        let path = std::path::Path::new("/file");
        let direct: Target<'_, NonClone> = path.as_target();
        let explicit: Target<'_, NonClone> = Target::path(&path);
        for target in [direct, explicit] {
            assert!(matches!(target, Target::Path(actual) if actual == path));
        }
    }
}
