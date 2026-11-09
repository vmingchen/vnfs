//! Synchronous descriptor execution seam for local backend implementers.
//!
//! Namespace resolution, ownership and cursors remain in [`crate::LocalBackend`].
//! Engines must drain submitted operations before returning, preserve result
//! order and cardinality, and never retain borrowed files or buffers. Owned
//! destinations may be retained if draining fails; never return them while
//! outstanding operations might still access them. Each read reports its own
//! completion state and confirmed prefix; unsent slots are never zero-byte successes.
//! Synchronization also retains independent outcomes and attempts later siblings.
//! Writes retain confirmed prefixes and continue independent siblings.
use std::os::unix::fs::FileExt;
use std::{fs::File, io, sync::Arc};
#[cfg(test)]
use vfsi_core::FailureState;
use vfsi_core::IoProgress;
use vfsi_core::{OpError, OpResult, VfError};

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

/// Executor for independent descriptor I/O. Dependent/append/range-path
/// operations retain the local backend's ordered syscalls; whole-file paths
/// can opt into bounded retained-handle cohorts.
pub trait DescriptorIo: Send {
    /// Opt into bounded, retained-handle cohorts for whole-file path reads.
    /// None preserves serial path syscalls. The bound includes temporary open
    /// descriptors, not just submitted I/O; output and wave byte limits still apply.
    fn path_read_batch_size(&self) -> Option<std::num::NonZeroUsize> {
        None
    }
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
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<OpResult<usize>>;
    fn read_owned(&mut self, operations: Vec<OwnedRead>) -> Vec<OpResult<Vec<u8>>> {
        read_owned_via_borrowed(self, operations)
    }
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<OpResult<usize>>;
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<OpResult<()>>;
}

/// Adapter for executors whose synchronous borrowed read path already fills the
/// final destination (including cache-aware syscall selection).
pub fn read_owned_via_borrowed(
    engine: &mut (impl DescriptorIo + ?Sized),
    mut operations: Vec<OwnedRead>,
) -> Vec<OpResult<Vec<u8>>> {
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
                Err(OpError::unknown(
                    VfError::invalid_reply(None, "invalid read result cardinality"),
                    None,
                ))
            })
            .collect();
    }
    operations
        .into_iter()
        .zip(results)
        .map(|(mut op, result)| {
            let count = match result {
                Ok(count) => count,
                Err(error) => {
                    return Err(vfsi_core::internal::own_read_error(
                        error, op.offset, op.buffer,
                    ));
                }
            };
            if count > op.buffer.len() {
                return Err(OpError::unknown(
                    VfError::invalid_reply(None, "read exceeds destination length"),
                    None,
                ));
            }
            op.buffer.truncate(count);
            Ok(op.buffer)
        })
        .collect()
}

/// Execute independent positional reads in input order, retaining every outcome.
pub fn read_ordered(operations: &mut [Read<'_>]) -> Vec<OpResult<usize>> {
    operations
        .iter_mut()
        .map(|op| {
            op.file
                .read_at(op.buffer, op.offset)
                .map_err(|error| OpError::failed(VfError::from_io(None, error), None))
        })
        .collect()
}

/// Complete one positional write without hiding confirmed bytes on failure.
pub(super) fn write_complete(
    offset: u64,
    data: &[u8],
    mut write: impl FnMut(&[u8], u64) -> io::Result<usize>,
) -> OpResult<usize> {
    if offset.checked_add(data.len() as u64).is_none() {
        return Err(OpError::not_attempted(VfError::errno(
            None,
            libc::EOVERFLOW as u32,
        )));
    }
    let mut completed = 0;
    while completed < data.len() {
        let progress = || {
            (completed != 0).then_some(IoProgress::Write {
                bytes_written: completed,
            })
        };
        match write(&data[completed..], offset + completed as u64) {
            Ok(0) => {
                return Err(OpError::failed(
                    VfError::from_io(None, io::Error::from(io::ErrorKind::WriteZero)),
                    progress(),
                ));
            }
            Ok(count) if count > data.len() - completed => {
                return Err(OpError::unknown(
                    VfError::invalid_reply(None, "write exceeds source length"),
                    progress(),
                ));
            }
            Ok(count) => completed += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(OpError::failed(VfError::from_io(None, error), progress())),
        }
    }
    Ok(completed)
}

/// Execute independent complete positional writes in syscall order.
pub fn write_ordered(operations: &[Write<'_>]) -> Vec<OpResult<usize>> {
    operations
        .iter()
        .map(|op| {
            write_complete(op.offset, op.data, |data, offset| {
                op.file.write_at(data, offset)
            })
        })
        .collect()
}

/// Synchronize each descriptor in syscall order, retaining independent failures.
pub fn sync_ordered(files: &[&Arc<File>], data_only: bool) -> Vec<OpResult<()>> {
    files
        .iter()
        .map(|file| {
            if data_only {
                file.sync_data()
            } else {
                file.sync_all()
            }
            .map_err(|error| OpError::failed(VfError::from_io(None, error), None))
        })
        .collect()
}

// Unit tests exercise shared machinery without introducing a dev-dependency
// cycle from vfsi-local back to a concrete executor.
#[cfg(test)]
impl DescriptorIo for () {
    fn ordered(&self) -> bool {
        true
    }
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<OpResult<usize>> {
        read_ordered(operations)
    }
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<OpResult<usize>> {
        write_ordered(operations)
    }
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<OpResult<()>> {
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
        fn read(&mut self, _: &mut [Read<'_>]) -> Vec<OpResult<usize>> {
            (0..self.slots).map(|_| Ok(self.count)).collect()
        }
        fn write(&mut self, _: &[Write<'_>]) -> Vec<OpResult<usize>> {
            unreachable!()
        }
        fn sync(&mut self, _: &[&Arc<File>], _: bool) -> Vec<OpResult<()>> {
            unreachable!()
        }
    }

    struct PrefixFailure;
    impl DescriptorIo for PrefixFailure {
        fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<OpResult<usize>> {
            operations
                .iter_mut()
                .map(|op| {
                    op.buffer[..3].copy_from_slice(b"abc");
                    Err(OpError::unknown(
                        VfError::errno(None, libc::EIO as u32),
                        Some(IoProgress::Read(vfsi_core::api::ReadResult::buffered(
                            op.offset, 3, false,
                        ))),
                    ))
                })
                .collect()
        }
        fn write(&mut self, _: &[Write<'_>]) -> Vec<OpResult<usize>> {
            unreachable!()
        }
        fn sync(&mut self, _: &[&Arc<File>], _: bool) -> Vec<OpResult<()>> {
            unreachable!()
        }
    }

    #[test]
    fn owned_adapter_moves_confirmed_prefix_without_copying_or_exposing_tail() {
        let buffer = vec![99; 8];
        let pointer = buffer.as_ptr();
        let result = PrefixFailure
            .read_owned(vec![OwnedRead {
                file: Arc::new(File::open("/dev/null").unwrap()),
                offset: 7,
                buffer,
            }])
            .pop()
            .unwrap()
            .unwrap_err();
        let (_, state, progress) = result.into_parts();
        assert_eq!(state, FailureState::Unknown);
        let Some(IoProgress::Read(read)) = progress else {
            panic!("missing prefix")
        };
        assert_eq!(read.offset(), 7);
        assert_eq!(read.data().unwrap(), b"abc");
        assert_eq!(read.data().unwrap().as_ptr(), pointer);
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
                results[0].as_ref().unwrap_err().cause().kind(),
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
    #[test]
    fn syscall_write_failure_retains_prefix_and_retries_only_the_unwritten_tail() {
        let mut script = [
            Ok(2),
            Err(io::Error::from(io::ErrorKind::Interrupted)),
            Ok(1),
            Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        ]
        .into_iter();
        let mut calls = Vec::new();
        let error = write_complete(11, b"abcdef", |data, offset| {
            calls.push((offset, data.to_vec()));
            script.next().unwrap()
        })
        .unwrap_err();
        assert_eq!(
            calls,
            [
                (11, b"abcdef".to_vec()),
                (13, b"cdef".to_vec()),
                (13, b"cdef".to_vec()),
                (14, b"def".to_vec())
            ]
        );
        assert_eq!(error.state(), FailureState::Failed);
        assert_eq!(error.cause().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 3 })
        );
    }

    #[test]
    fn syscall_write_zero_and_invalid_counts_do_not_replay_confirmed_bytes() {
        for invalid in [false, true] {
            let mut calls = 0;
            let error = write_complete(0, b"abc", |_, _| {
                calls += 1;
                Ok(if calls == 1 {
                    1
                } else if invalid {
                    9
                } else {
                    0
                })
            })
            .unwrap_err();
            assert_eq!(calls, 2);
            assert_eq!(
                error.progress(),
                Some(&IoProgress::Write { bytes_written: 1 })
            );
            assert_eq!(
                error.state(),
                if invalid {
                    FailureState::Unknown
                } else {
                    FailureState::Failed
                }
            );
            assert_eq!(
                error.cause().kind(),
                if invalid {
                    io::ErrorKind::InvalidData
                } else {
                    io::ErrorKind::WriteZero
                }
            );
        }
        let error = write_complete(u64::MAX - 1, b"ab", |_, _| {
            panic!("overflow must not dispatch")
        })
        .unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert!(error.progress().is_none());
        assert_eq!(
            write_complete(u64::MAX, b"", |_, _| panic!("empty write")),
            Ok(0)
        );
    }
}
