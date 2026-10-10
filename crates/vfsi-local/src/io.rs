//! Synchronous descriptor execution seam for local backend implementers.
//!
//! Namespace resolution, ownership and cursors remain in `DummyVecFs`.
//! Engines must finish every operation before returning, preserve result order
//! and cardinality, and never retain borrowed files or buffers.
use std::{fs::File, io, sync::Arc};

pub struct Read<'a> {
    pub file: &'a Arc<File>,
    pub offset: u64,
    pub buffer: &'a mut [u8],
}

pub struct Write<'a> {
    pub file: &'a Arc<File>,
    pub offset: u64,
    pub data: &'a [u8],
}

/// Optional executor for independent descriptor I/O. Dependent/append/path
/// operations retain the local backend's ordered syscall implementation.
pub trait DescriptorIo: Send {
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<io::Result<usize>>;
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<io::Result<usize>>;
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<io::Result<usize>>;
}
