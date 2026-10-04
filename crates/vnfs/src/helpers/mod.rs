//! High-level, protocol-independent workflows using [`crate::Vfsi`].
//!
//! Helpers plan work locally and preserve vector operations where possible.
//! They do not provide transactions, rollback, or a security sandbox.

#![cfg_attr(feature = "nfs", doc = include_str!("README.md"))]

mod tree;
pub use tree::{Tree, TreeBuilder};
