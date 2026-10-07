//! Protocol-neutral vectorized filesystem contracts for the VFSI workspace.
//!
//! [`Vfsi`] provides native vector execution; [`VfsiExt`] adds blanket scalar
//! conveniences and composed workflows without replacing batching with loops.
//! Portable requests, results, options, and imports live in [`api`].
//! No filesystem backend, RPC library, or async runtime is required.
//! The `vnfs` crate supplies NFS clients and re-exports these same contracts.
//!
//! ```no_run
//! use vfsi_core::{Vfsi, VfsiExt};
//! fn load_parts(fs: &impl Vfsi) -> vfsi_core::api::Result<Vec<Vec<u8>>> {
//!     fs.read_files(&["/file-1", "/file-2"])
//! }
//! ```
//!
//! The other root types support backend implementation; application read/write
//! operations should be imported from [`api`] rather than their backend counterparts.

mod error;
#[doc(hidden)]
pub mod internal;
pub mod path;
mod types;

pub use error::{RpcError, RpcResult, STATUS_TRANSPORT, TransportKind};
pub use types::*;

/// Portable application contracts, operations, options, and extension workflows.
pub mod api;
pub use api::{AsTarget, FileHandle, MkDirOp, SetAttrsOp, Target, Vfsi, VfsiExt};
