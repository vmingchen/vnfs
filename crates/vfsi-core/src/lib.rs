//! Protocol-neutral operation types, errors, paths, and capability flags for
//! the VFSI workspace.

mod error;
pub mod path;
mod types;

pub use error::{RpcError, RpcResult, STATUS_TRANSPORT};
pub use types::*;
