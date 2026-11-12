//! Rooted filesystem access through ordered POSIX syscalls.
//!
//! [`connect`] implements [`vfsi_core::Vfsi`] and [`vfsi_core::VfsiExt`].
//! Namespace resolution, owned handles, cursors, traversal and dependent I/O
//! are shared with `vfsi-uring` through `vfsi-local`; this crate selects syscall
//! execution, without an io_uring or network protocol dependency.
//! Linux root containment requires Linux 5.6+ and mounted `/proc`.

use std::{fs::File, path::Path, sync::Arc};
use vfsi_core::VfResult;
use vfsi_local::{
    LocalBackend,
    io::{DescriptorIo, OwnedRead, Read, Write},
};

/// Ordered syscall executor for backend implementers.
#[derive(Default)]
pub struct PosixIo;

impl DescriptorIo for PosixIo {
    fn ordered(&self) -> bool {
        true
    }
    fn owned_read_window(&self) -> Option<usize> {
        // Synchronous pread fills the final output directly and needs no scratch.
        Some(usize::MAX)
    }
    fn read_owned(&mut self, operations: Vec<OwnedRead>) -> Vec<vfsi_core::OpResult<Vec<u8>>> {
        use std::os::unix::fs::FileExt;
        operations
            .into_iter()
            .map(|mut op| {
                let count = op
                    .file
                    .read_at(&mut op.buffer, op.offset)
                    .map_err(|error| {
                        vfsi_core::OpError::failed(vfsi_core::VfError::from_io(None, error), None)
                    })?;
                op.buffer.truncate(count);
                Ok(op.buffer)
            })
            .collect()
    }
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<vfsi_core::OpResult<usize>> {
        vfsi_local::io::read_ordered(operations)
    }
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<vfsi_core::OpResult<usize>> {
        vfsi_local::io::write_ordered(operations)
    }
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<vfsi_core::OpResult<()>> {
        vfsi_local::io::sync_ordered(files, data_only)
    }
}

/// Construct the native backend rooted at an existing directory.
/// Applications normally use [`connect`] or `vnfs::Posix` instead.
pub fn backend(root: impl AsRef<Path>) -> VfResult<LocalBackend> {
    LocalBackend::new(root.as_ref().to_path_buf(), PosixIo)
}

/// Connect to an existing directory, using ordered syscalls.
pub fn connect(root: impl AsRef<Path>) -> VfResult<vfsi_sync::FsClient<LocalBackend>> {
    backend(root).map(vfsi_sync::FsClient::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfsi_core::api::{ReadOp, SyncMode, WriteOp};
    use vfsi_core::{OpenFlags, OpenOp, VfsiExt};

    #[test]
    fn standalone_backend_roundtrips_vectors_buffers_and_sync() {
        let root = tempfile::tempdir().unwrap();
        let fs = connect(root.path()).unwrap();
        let files = fs
            .open_batch(&[
                OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
                OpenOp::new("/b", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
            ])
            .unwrap();
        let written = fs
            .write_batch(
                &[
                    WriteOp::at(&files[0], 0, b"alpha"),
                    WriteOp::at(&files[1], 0, b"beta"),
                ],
                Default::default(),
            )
            .unwrap();
        assert_eq!(
            written.iter().map(|r| r.bytes_written).collect::<Vec<_>>(),
            [5, 4]
        );
        assert!(written.iter().all(|r| !r.stable));
        let mut a = [0; 5];
        let results = fs
            .read_batch(
                [
                    ReadOp::into(&files[0], 0, &mut a),
                    ReadOp::range(&files[1], 0, 10),
                ],
                Default::default(),
            )
            .unwrap();
        assert_eq!(&a, b"alpha");
        assert!(results[0].data().is_none());
        assert_eq!(results[1].data(), Some(b"beta".as_slice()));
        assert!(
            !results[1].eof(),
            "positive short progress is not proof of EOF"
        );
        let eof = fs
            .read_batch([ReadOp::range(&files[1], 4, 1)], Default::default())
            .unwrap();
        assert_eq!(eof[0].bytes_read(), 0);
        assert!(eof[0].eof());
        fs.sync_batch(&files.iter().collect::<Vec<_>>(), SyncMode::Data)
            .unwrap();
        fs.sync_batch(&files.iter().collect::<Vec<_>>(), SyncMode::All)
            .unwrap();
        fs.close_files(files).unwrap();
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"alpha");
        std::fs::hard_link(root.path().join("a"), root.path().join("alias")).unwrap();
        let files = fs
            .open_batch(&[
                OpenOp::new("/a", OpenFlags::WRITE),
                OpenOp::new("/alias", OpenFlags::WRITE),
            ])
            .unwrap();
        let results = fs
            .write_batch(
                &[
                    WriteOp::at(&files[0], 0, b"1234"),
                    WriteOp::at(&files[1], 2, b"XY"),
                ],
                Default::default(),
            )
            .unwrap();
        assert_eq!(
            results.iter().map(|r| r.bytes_written).collect::<Vec<_>>(),
            [4, 2]
        );
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"12XYa");
        fs.close_files(files).unwrap();
    }

    #[test]
    fn executor_keeps_write_order_and_independent_read_outcomes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let write = Arc::new(std::fs::OpenOptions::new().write(true).open(&path).unwrap());
        let read = Arc::new(File::open(&path).unwrap());
        let results = PosixIo.write(&[
            Write {
                file: &write,
                offset: 0,
                data: b"A",
            },
            Write {
                file: &read,
                offset: 1,
                data: b"B",
            },
            Write {
                file: &write,
                offset: 2,
                data: b"C",
            },
        ]);
        assert_eq!(results.len(), 3);
        assert_eq!(*results[0].as_ref().unwrap(), 1);
        assert!(results[1].is_err());
        assert_eq!(*results[2].as_ref().unwrap(), 1);
        assert_eq!(
            results[1].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::Failed
        );
        assert!(results[1].as_ref().unwrap_err().progress().is_none());
        assert_eq!(std::fs::read(path).unwrap(), b"ArCginal");

        let mut first = [7; 1];
        let mut second = [7; 1];
        let results = PosixIo.read(&mut [
            Read {
                file: &write,
                offset: 0,
                buffer: &mut first,
            },
            Read {
                file: &read,
                offset: 0,
                buffer: &mut second,
            },
        ]);
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::Failed
        );
        assert_eq!(*results[1].as_ref().unwrap(), 1);
        assert_eq!(first, [7]);
        assert_eq!(second, *b"A");
    }

    #[test]
    fn owned_reads_reuse_buffers_and_keep_success_after_a_failed_input() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"abcdef").unwrap();
        let readable = Arc::new(File::open(&path).unwrap());
        let write_only = Arc::new(File::options().write(true).open(&path).unwrap());
        let first = vec![0xcc; 3];
        let first_pointer = first.as_ptr();
        let tail = vec![0xcc; 8];
        let tail_pointer = tail.as_ptr();
        let outcomes = PosixIo.read_owned(vec![
            OwnedRead {
                file: Arc::clone(&readable),
                offset: 1,
                buffer: first,
            },
            OwnedRead {
                file: write_only,
                offset: 0,
                buffer: vec![0xcc; 4],
            },
            OwnedRead {
                file: Arc::clone(&readable),
                offset: 4,
                buffer: tail,
            },
            OwnedRead {
                file: readable,
                offset: 6,
                buffer: vec![0xcc; 4],
            },
        ]);
        assert_eq!(outcomes.len(), 4);
        assert_eq!(outcomes[0].as_ref().unwrap(), b"bcd");
        assert_eq!(outcomes[0].as_ref().unwrap().as_ptr(), first_pointer);
        let error = outcomes[1].as_ref().unwrap_err();
        assert_eq!(error.state(), vfsi_core::FailureState::Failed);
        assert!(error.progress().is_none());
        assert_eq!(outcomes[2].as_ref().unwrap(), b"ef");
        assert_eq!(outcomes[2].as_ref().unwrap().as_ptr(), tail_pointer);
        assert!(outcomes[3].as_ref().unwrap().is_empty());
    }
}
