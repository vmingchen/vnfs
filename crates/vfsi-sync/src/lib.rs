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

/// Scalar/singular view of the synchronous interface.
pub mod sfsi {
    pub use crate::{VecFs, VecFsExt};
    pub use vfsi_core::{Fd, VfAttrs, VfError, VfFile, VfOffset, VfResult, VfType};
}

/// Vectorized view of the synchronous interface.
pub mod vfsi {
    pub use crate::{VecFs, VecFsExt, rm_recursive};
    pub use vfsi_core::*;
}
