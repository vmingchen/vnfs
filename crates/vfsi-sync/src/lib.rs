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
pub use traits::{VecFs, VecFsExt, rm_recursive};
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
        MetadataFileSystem, NamespaceFileSystem, NativeFileSystem, OpenOptions, VecFs, VecFsExt,
    };
    pub use vfsi_core::{Fd, VfAttrs, VfError, VfFile, VfOffset, VfResult, VfType};
}

/// Vectorized view of the synchronous interface.
pub mod vfsi {
    pub use crate::{VecFs, VecFsExt, VectorFileSystem, rm_recursive};
    pub use vfsi_core::*;
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support;
