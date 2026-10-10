//! Rooted filesystem access through ordered POSIX syscalls.
//!
//! [`connect`] implements [`vfsi_core::Vfsi`] and [`vfsi_core::VfsiExt`].
//! Namespace resolution, owned handles, cursors, traversal and dependent I/O
//! are shared with `vfsi-uring` through `vfsi-local`; this crate selects syscall
//! execution, without an io_uring or network protocol dependency.
//! Linux root containment requires Linux 5.6+ and mounted `/proc`.

use std::{fs::File, io, path::Path, sync::Arc};
use vfsi_core::VfResult;
use vfsi_local::{
    LocalBackend,
    io::{DescriptorIo, Read, Write},
};

/// Ordered syscall executor for backend implementers.
#[derive(Default)]
pub struct PosixIo;

impl DescriptorIo for PosixIo {
    fn ordered(&self) -> bool {
        true
    }
    fn read(&mut self, operations: &mut [Read<'_>]) -> Vec<io::Result<usize>> {
        vfsi_local::io::read_ordered(operations)
    }
    fn write(&mut self, operations: &[Write<'_>]) -> Vec<io::Result<usize>> {
        vfsi_local::io::write_ordered(operations)
    }
    fn sync(&mut self, files: &[&Arc<File>], data_only: bool) -> Vec<io::Result<usize>> {
        vfsi_local::io::sync_ordered(files, data_only)
    }
}

/// Construct the rooted native backend, creating a missing root.
/// Applications normally use [`connect`] or `vnfs::Posix` instead.
pub fn backend(root: impl AsRef<Path>) -> VfResult<LocalBackend> {
    LocalBackend::new(root.as_ref().to_path_buf(), PosixIo)
}

/// Connect to a root (created if missing), using ordered syscalls.
pub fn connect(root: impl AsRef<Path>) -> VfResult<vfsi_sync::FsClient<LocalBackend>> {
    backend(root).map(vfsi_sync::FsClient::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfsi_core::api::{ReadOp, SyncMode, WriteOp};
    use vfsi_core::{OpenFlags, OpenOp, Vfsi, VfsiExt};

    #[test]
    fn standalone_backend_roundtrips_vectors_buffers_and_sync() {
        let root = tempfile::tempdir().unwrap();
        let fs = connect(root.path()).unwrap();
        let files = fs
            .vopen(&[
                OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
                OpenOp::new("/b", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
            ])
            .unwrap();
        let written = fs
            .vwrite(
                &[
                    WriteOp::at(&files[0], 0, b"alpha"),
                    WriteOp::at(&files[1], 0, b"beta"),
                ],
                Default::default(),
            )
            .unwrap();
        assert_eq!(
            written.iter().map(|r| r.written).collect::<Vec<_>>(),
            [5, 4]
        );
        let mut a = [0; 5];
        let results = fs
            .vread(
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
        assert!(results[1].eof());
        fs.vfsync(&files.iter().collect::<Vec<_>>(), SyncMode::Data)
            .unwrap();
        fs.vfsync(&files.iter().collect::<Vec<_>>(), SyncMode::All)
            .unwrap();
        fs.close_files(files).unwrap();
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"alpha");
        std::fs::hard_link(root.path().join("a"), root.path().join("alias")).unwrap();
        let files = fs
            .vopen(&[
                OpenOp::new("/a", OpenFlags::WRITE),
                OpenOp::new("/alias", OpenFlags::WRITE),
            ])
            .unwrap();
        let results = fs
            .vwrite(
                &[
                    WriteOp::at(&files[0], 0, b"1234"),
                    WriteOp::at(&files[1], 2, b"XY"),
                ],
                Default::default(),
            )
            .unwrap();
        assert_eq!(
            results.iter().map(|r| r.written).collect::<Vec<_>>(),
            [4, 2]
        );
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"12XYa");
        fs.close_files(files).unwrap();
    }

    #[test]
    fn executor_stops_after_error_and_preserves_result_slots() {
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
        assert_eq!(*results[2].as_ref().unwrap(), 0);
        assert_eq!(std::fs::read(path).unwrap(), b"Ariginal");

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
        assert!(results[0].is_err());
        assert_eq!(*results[1].as_ref().unwrap(), 0);
        assert_eq!(second, [7]);
    }
}
