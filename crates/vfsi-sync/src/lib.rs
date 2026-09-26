//! Synchronous filesystem interface facets.
//!
//! The compatibility [`VecFs`] trait remains the optimized vector contract.
//! Its single-operation helpers form today's scalar facet without introducing
//! a second, conflicting set of method names.

use std::path::{Path, PathBuf};

pub use vfsi_core::*;

#[doc(hidden)]
pub mod path {
    pub use vfsi_core::path::*;
}

mod traits;
pub use traits::{
    DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
    DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
    DEFAULT_WALK_MAX_DEPTH, ReadAllOptions, ReadDirOptions, ReadStreamOptions, VecFs, VecFsExt,
    WalkOptions, rm_recursive,
};
mod io;
pub use io::{VfFileHandle, VfOpenOptions};
mod client;
pub use client::{FsClient, FsFile, FsRead, FsReadInto, FsWrite, OpenOptions, SetMetadata};
mod native;
pub use native::{
    CopyFileSystem, DirectoryFileSystem, FileSystem, LinkFileSystem, MetadataFileSystem,
    NamespaceFileSystem, NativeFileSystem, VectorFileSystem,
};

/// Scalar/singular view of the synchronous interface.
pub mod sfsi {
    pub use crate::{
        CopyFileSystem, DirectoryFileSystem, FileSystem, FsClient, FsFile, LinkFileSystem,
        MetadataFileSystem, NamespaceFileSystem, NativeFileSystem, OpenOptions, ReadDirOptions,
        ReadStreamOptions, VecFs, VecFsExt, WalkOptions,
    };
    pub use vfsi_core::{Fd, VfAttrs, VfError, VfFile, VfOffset, VfResult, VfType};
}

/// Vectorized view of the synchronous interface.
pub mod vfsi {
    pub use crate::{
        DEFAULT_DIRECTORY_MAX_ENTRIES, DEFAULT_DIRECTORY_MAX_PATH_BYTES,
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, DEFAULT_READ_STREAM_CHUNK_BYTES,
        DEFAULT_WALK_MAX_DEPTH, ReadAllOptions, ReadDirOptions, ReadStreamOptions, VecFs, VecFsExt,
        VectorFileSystem, WalkOptions, rm_recursive,
    };
    pub use vfsi_core::*;
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support;
