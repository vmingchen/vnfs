//! Synchronous descriptor execution seam for local backend implementers.
//!
//! Namespace resolution, ownership and cursors remain in [`crate::LocalBackend`].
//! Engines must drain submitted operations before returning, preserve result
//! order and cardinality, and never retain borrowed files or buffers. Stop
//! submitting new work after a known failure; unsent slots may return `Ok(0)`.
use std::os::unix::fs::FileExt;
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

/// Executor for independent descriptor I/O. Dependent/append/path
/// operations retain the local backend's ordered syscall implementation.
pub trait DescriptorIo: Send {
    /// Whether requests execute strictly in input order. Ordered executors
    /// need no object-identity/overlap scan before positional writes.
    fn ordered(&self) -> bool {
        false
    }
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<io::Result<usize>>;
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<io::Result<usize>>;
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<io::Result<usize>>;
}

// Keep syscall fallbacks here, not in either concrete executor crate. They
// preserve input order and do not execute a suffix after a known failure.
fn ordered<T>(
    items: impl IntoIterator<Item = T>,
    mut execute: impl FnMut(T) -> io::Result<usize>,
) -> Vec<io::Result<usize>> {
    let mut failed = false;
    items
        .into_iter()
        .map(|item| {
            if failed {
                return Ok(0);
            }
            let result = execute(item);
            failed = result.is_err();
            result
        })
        .collect()
}

/// Execute positional reads in order, stopping after the first error.
pub fn read_ordered(operations: &mut [Read<'_>]) -> Vec<io::Result<usize>> {
    ordered(operations, |op| op.file.read_at(op.buffer, op.offset))
}

/// Execute complete positional writes in order, stopping after the first error.
pub fn write_ordered(operations: &[Write<'_>]) -> Vec<io::Result<usize>> {
    ordered(operations, |op| {
        op.file.write_all_at(op.data, op.offset)?;
        Ok(op.data.len())
    })
}

/// Synchronize descriptors in order, stopping after the first error.
pub fn sync_ordered(files: &[&Arc<File>], data_only: bool) -> Vec<io::Result<usize>> {
    ordered(files, |file| {
        if data_only {
            file.sync_data()?;
        } else {
            file.sync_all()?;
        }
        Ok(0)
    })
}

// Unit tests exercise shared machinery without introducing a dev-dependency
// cycle from vfsi-local back to a concrete executor.
#[cfg(test)]
impl DescriptorIo for () {
    fn ordered(&self) -> bool {
        true
    }
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<io::Result<usize>> {
        read_ordered(operations)
    }
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<io::Result<usize>> {
        write_ordered(operations)
    }
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<io::Result<usize>> {
        sync_ordered(files, data_only)
    }
}
