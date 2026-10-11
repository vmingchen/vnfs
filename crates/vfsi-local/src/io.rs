//! Synchronous descriptor execution seam for local backend implementers.
//!
//! Namespace resolution, ownership and cursors remain in [`crate::LocalBackend`].
//! Engines must drain submitted operations before returning, preserve result
//! order and cardinality, and never retain borrowed files or buffers. Owned
//! destinations may be retained if draining fails; never return them while
//! outstanding operations might still access them. Stop
//! submitting new work after a known failure; unsent slots may return `Ok(0)`.
use std::os::unix::fs::FileExt;
use std::{fs::File, io, sync::Arc};

pub struct Read<'a> {
    pub file: &'a Arc<File>,
    pub offset: u64,
    pub buffer: &'a mut [u8],
}

/// Transfer ownership of final result storage to the executor. Unlike borrowed
/// reads, an executor can retain this storage if in-flight I/O cannot be drained.
pub struct OwnedRead {
    pub file: Arc<File>,
    pub offset: u64,
    pub buffer: Vec<u8>,
}

pub struct Write<'a> {
    pub file: &'a Arc<File>,
    pub offset: u64,
    pub data: &'a [u8],
}

/// Executor for independent descriptor I/O. Dependent/append/path
/// operations retain the local backend's ordered syscall implementation.
pub trait DescriptorIo: Send {
    /// Largest aggregate destination working set benefiting from owned reads.
    /// None preserves the existing borrowed dispatch/allocation path.
    fn owned_read_window(&self) -> Option<usize> {
        None
    }
    /// Whether writes execute strictly in input order. Ordered writers
    /// need no object-identity/overlap scan before positional writes; this
    /// does not constrain read or fsync concurrency.
    fn ordered(&self) -> bool {
        false
    }
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<io::Result<usize>>;
    fn read_owned(&mut self, operations: Vec<OwnedRead>) -> Vec<io::Result<Vec<u8>>> {
        read_owned_via_borrowed(self, operations)
    }
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<io::Result<usize>>;
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<io::Result<usize>>;
}

/// Adapter for executors whose synchronous borrowed read path already fills the
/// final destination (including cache-aware syscall selection).
pub fn read_owned_via_borrowed(
    engine: &mut (impl DescriptorIo + ?Sized),
    mut operations: Vec<OwnedRead>,
) -> Vec<io::Result<Vec<u8>>> {
    let mut borrowed: Vec<_> = operations
        .iter_mut()
        .map(|op| Read {
            file: &op.file,
            offset: op.offset,
            buffer: &mut op.buffer,
        })
        .collect();
    let results = engine.read(&mut borrowed);
    drop(borrowed);
    if results.len() != operations.len() {
        return operations
            .into_iter()
            .map(|_| {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid read result cardinality",
                ))
            })
            .collect();
    }
    operations
        .into_iter()
        .zip(results)
        .map(|(mut op, result)| {
            let count = result?;
            if count > op.buffer.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "read exceeds destination length",
                ));
            }
            op.buffer.truncate(count);
            Ok(op.buffer)
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Malformed {
        slots: usize,
        count: usize,
    }
    impl DescriptorIo for Malformed {
        fn read(&mut self, _: &mut [Read<'_>]) -> Vec<io::Result<usize>> {
            (0..self.slots).map(|_| Ok(self.count)).collect()
        }
        fn write(&mut self, _: &[Write<'_>]) -> Vec<io::Result<usize>> {
            unreachable!()
        }
        fn sync(&mut self, _: &[&Arc<File>], _: bool) -> Vec<io::Result<usize>> {
            unreachable!()
        }
    }

    #[test]
    fn owned_adapter_rejects_bad_counts_and_cardinality_without_panicking() {
        let file = Arc::new(File::open("/dev/null").unwrap());
        for (slots, count) in [(0, 0), (2, 1), (1, 5)] {
            let results = Malformed { slots, count }.read_owned(vec![OwnedRead {
                file: Arc::clone(&file),
                offset: 0,
                buffer: vec![99; 4],
            }]);
            assert_eq!(results.len(), 1);
            assert_eq!(
                results[0].as_ref().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let buffer = vec![99; 4];
        let pointer = buffer.as_ptr();
        let results = Malformed { slots: 1, count: 2 }.read_owned(vec![OwnedRead {
            file,
            offset: 0,
            buffer,
        }]);
        assert_eq!(results[0].as_ref().unwrap(), &[99; 2]);
        assert_eq!(results[0].as_ref().unwrap().as_ptr(), pointer);
    }
}
