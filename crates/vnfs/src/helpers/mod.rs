//! High-level, protocol-independent workflows using [`crate::Client`].
//!
//! Helpers plan work locally and preserve vector operations where possible.
//! They do not provide transactions, rollback, or a security sandbox.

mod tree;
pub use tree::{Tree, TreeBuilder};
