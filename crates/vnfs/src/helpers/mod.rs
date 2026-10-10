//! High-level, protocol-independent workflows using [`crate::Vfsi`].
//!
//! Helpers plan work locally and preserve vector operations where possible.
//! They do not provide transactions, rollback, or a security sandbox.

#![cfg_attr(feature = "nfs", doc = include_str!("README.md"))]

mod tree;
pub use tree::{Tree, TreeBuilder};

mod transfer;
pub use transfer::{
    CopyLayout, CopyOptions, Existing, TransferProgress, TransferSummary, UnsupportedEntry,
    copy_items, copy_items_with_progress, copy_tree, copy_tree_with_progress, move_items,
    move_items_with_progress,
};
mod stats;
pub use stats::{TreeStats, tree_stats};
mod data;
pub use data::copy_to_writer;
