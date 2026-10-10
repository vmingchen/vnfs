//! Shared native algorithms, grouped by operation family.
//! Defaults call backend hooks rather than bypassing specialized overrides.

use crate::backend::{HandleBackend, VectorBackend};
use crate::*;

use crate::backend::{bytes_to_path, metadata_mask, translate_open_flags};

use crate::traits::{
    take_single_result, validate_read_into_results, validate_read_results, validate_write_results,
};

use vfsi_core::internal::ManyResults;

use std::path::{Path, PathBuf};

mod handle;
pub use handle::*;
mod io;
pub use io::*;
mod metadata;
pub use metadata::*;
mod directory;
pub use directory::*;
mod namespace;
pub use namespace::*;
mod links;
pub use links::*;
mod copy;
pub use copy::*;
mod read;
pub use read::*;
mod removal;
pub use removal::*;
