//! This target depends only on vfsi-core: extensions must not require a backend.
use std::{cell::Cell, io, path::Path};
use vfsi_core::api::*;
use vfsi_core::{Vfsi, VfsiExt};

#[derive(Debug)]
struct TestFile;
impl io::Read for TestFile {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("unexpected scalar I/O")
    }
}
impl io::Write for TestFile {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("unexpected scalar I/O")
    }
}
impl io::Seek for TestFile {
    fn seek(&mut self, _: io::SeekFrom) -> io::Result<u64> {
        panic!("unexpected scalar I/O")
    }
}
impl FileHandle for TestFile {
    type ReadRequest<'a> = (u64, usize);
    type ReadIntoRequest<'a> = (u64, &'a mut [u8]);
    fn path(&self) -> &Path {
        Path::new("/file")
    }
    fn metadata(&self) -> Result<Metadata> {
        panic!("unexpected scalar metadata")
    }
    fn read_request_at(&self, offset: u64, length: usize) -> Self::ReadRequest<'_> {
        (offset, length)
    }
    fn read_request_at_into<'a>(
        &'a self,
        offset: u64,
        buffer: &'a mut [u8],
    ) -> Self::ReadIntoRequest<'a> {
        (offset, buffer)
    }
    fn read_at(&self, _: &mut [u8], _: u64) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn write_at(&self, _: &[u8], _: u64) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn read_native(&mut self, _: &mut [u8]) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn read_to_end_with_limit(&mut self, _: usize) -> Result<Vec<u8>> {
        panic!("unexpected scalar I/O")
    }
    fn write_native(&mut self, _: &[u8]) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn seek_native(&mut self, _: io::SeekFrom) -> Result<u64> {
        panic!("unexpected scalar I/O")
    }
    fn sync_data(&self) -> Result<()> {
        panic!("unexpected scalar I/O")
    }
    fn sync_all(&self) -> Result<()> {
        panic!("unexpected scalar I/O")
    }
    fn try_close(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_closed(&self) -> bool {
        false
    }
    fn close(self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct PortableFs {
    reads: Cell<usize>,
    listings: Cell<usize>,
    shape: Cell<u8>,
}
impl Vfsi for PortableFs {
    fn vsetattrs<P: AsRef<std::path::Path>>(
        &self,
        _: &[(P, vfsi_core::MetadataUpdate)],
        _: bool,
    ) -> vfsi_core::api::Result<()> {
        Ok(())
    }

    type File = TestFile;
    fn limits(&self) -> ResourceLimits {
        ResourceLimits::default()
    }
    fn vgetattrs<P: AsRef<Path>>(&self, _: &[P], _: MetadataOptions) -> Result<Vec<Metadata>> {
        panic!("unexpected metadata")
    }
    fn vopen(&self, requests: &[OpenRequest]) -> Result<Vec<TestFile>> {
        Ok(requests.iter().map(|_| TestFile).collect())
    }
    fn vread<'a>(
        &self,
        ops: impl IntoIterator<Item = ReadOp<'a, TestFile>>,
        _: ReadOptions,
    ) -> Result<Vec<ReadResult>> {
        self.reads.set(self.reads.get() + 1);
        let mut results: Vec<_> = ops
            .into_iter()
            .enumerate()
            .map(|(index, mut op)| {
                if let Some((offset, buffer)) = op.buffer_request_mut() {
                    buffer.fill(index as u8);
                    ReadResult::buffered(*offset, buffer.len(), false)
                } else {
                    assert!(op.whole_file_path().is_some());
                    ReadResult::owned(0, vec![index as u8], true)
                }
            })
            .collect();
        match self.shape.get() {
            1 => {
                results.pop();
            }
            2 => results.push(ReadResult::owned(0, vec![], true)),
            3 => results[0] = ReadResult::buffered(0, 1, true),
            4 => results[0] = ReadResult::owned(1, vec![0], true),
            5 => results[0] = ReadResult::owned(0, vec![0], false),
            6 => return Err(Error::transport(None, "lost reply")),
            _ => {}
        }
        Ok(results)
    }
    fn vwrite<'a>(&self, _: &[WriteOp<'a, TestFile>], _: WriteOptions) -> Result<Vec<WriteResult>> {
        panic!("unexpected write")
    }
    fn vclose(&self, _: &mut [TestFile]) -> Result<()> {
        Ok(())
    }
    fn vmkdir<P: AsRef<Path>>(&self, _: &[P]) -> Result<()> {
        panic!("unexpected mkdir")
    }
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(&self, _: &[(P, Q)]) -> Result<()> {
        panic!("unexpected copy")
    }
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(&self, _: &[(P, Q)]) -> Result<()> {
        panic!("unexpected rename")
    }
    fn vremove<P: AsRef<Path>>(&self, _: &[P], _: RemoveMode, _: RemoveOptions) -> Result<()> {
        panic!("unexpected remove")
    }
    fn vstream<P: AsRef<Path>>(
        &self,
        _: &[P],
        _: ReadStreamOptions,
        _: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
    ) -> Result<Vec<StreamCompletion>> {
        panic!("unexpected stream")
    }
    fn vlistdirs<P: AsRef<Path>>(
        &self,
        paths: &[P],
        _: VisitOptions,
        mut callback: impl FnMut(usize, DirectoryListing) -> Result<ControlFlow<()>>,
    ) -> Result<Vec<TraversalCompletion>> {
        self.listings.set(self.listings.get() + 1);
        let mut results = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            let stopped = callback(
                index,
                DirectoryListing {
                    path: path.as_ref().to_owned(),
                    entries: vec![],
                },
            )?
            .is_break();
            results.push(if stopped {
                TraversalCompletion::Stopped
            } else {
                TraversalCompletion::Complete
            });
            if stopped {
                break;
            }
        }
        Ok(results)
    }
}

#[test]
fn blanket_extensions_preserve_vector_dispatch_and_root_grouping() {
    let fs = PortableFs::default();
    assert_eq!(fs.read_files(&["/a", "/b"]).unwrap(), [vec![0], vec![1]]);
    assert_eq!(fs.reads.get(), 1);
    let trees = fs
        .read_dirs_with_options(&["/a", "/b"], VisitOptions::new())
        .unwrap();
    assert_eq!(fs.listings.get(), 1);
    assert_eq!(trees[0][0].path, Path::new("/a"));
    assert_eq!(trees[1][0].path, Path::new("/b"));
}

#[test]
fn extensions_reject_malformed_results_and_do_not_replay_transport_failures() {
    let fs = PortableFs::default();
    for shape in 1..=6 {
        fs.shape.set(shape);
        let before = fs.reads.get();
        let error = fs.read_files(&["/a", "/b"]).unwrap_err();
        assert!(error.is_transport(), "{error}");
        assert_eq!(fs.reads.get(), before + 1);
        if shape == 6 {
            assert_eq!(error.index(), None);
        }
    }
}

#[test]
fn consuming_reads_release_caller_buffers_and_owned_counts_are_derived() {
    let fs = PortableFs::default();
    let file = TestFile;
    let mut buffer = [99; 3];
    let result = fs
        .vread([ReadOp::into(&file, 7, &mut buffer)], ReadOptions::new())
        .unwrap();
    assert_eq!(buffer, [0; 3]);
    assert_eq!((result[0].offset(), result[0].read()), (7, 3));
    assert!(result[0].is_buffered());
    buffer.fill(42); // Results retain no borrow of caller storage.
    let owned = ReadResult::owned(2, vec![1, 2, 3], true);
    assert_eq!(owned.read(), owned.data().unwrap().len());
}
