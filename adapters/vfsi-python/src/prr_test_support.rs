//! Shared native fault support; ordinary operations retain the local backend contract.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vfsi_sync::backend::{HandleBackend, VectorBackend};

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);
pub(crate) struct TestRoot(pub(crate) PathBuf);
impl TestRoot {
    pub(crate) fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "vfsi-python-prr-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("test root setup failed: {error}"),
            }
        }
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) struct FaultBackend {
    pub(crate) inner: LocalBackend,
    pub(crate) read_error: Option<VfError>,
    pub(crate) read_error_on_call: usize,
    pub(crate) read_limit: Option<usize>,
    pub(crate) read_requests: Arc<Mutex<Vec<Vec<(u64, usize)>>>>,
    pub(crate) grow_before_read: Option<(PathBuf, Vec<u8>)>,
    pub(crate) read_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) write_counts: Option<Vec<usize>>,
    pub(crate) write_error: Option<VfError>,
    pub(crate) write_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
}
impl FaultBackend {
    pub(crate) fn new(root: &Path, read_error: Option<VfError>) -> Self {
        Self {
            inner: vfsi_posix::backend(root).unwrap(),
            read_error,
            read_error_on_call: 0,
            read_limit: None,
            read_requests: Arc::default(),
            grow_before_read: None,
            read_calls: Arc::default(),
            write_counts: None,
            write_error: None,
            write_calls: Arc::default(),
        }
    }
}
impl HandleBackend for FaultBackend {
    fn sync_data(&mut self, file: &FileOperand) -> vfsi_core::VfResult<()> {
        self.inner.sync_data(file)
    }
    fn open_impl(&mut self, op: &vfsi_core::OpenOp) -> vfsi_core::VfResult<FileOperand> {
        self.inner.open_impl(op)
    }
    fn close_impl(&mut self, file: &FileOperand) -> vfsi_core::VfResult<()> {
        self.inner.close_impl(file)
    }
    fn read_impl(&mut self, op: &ReadOp) -> vfsi_core::VfResult<ReadResult> {
        self.inner.read_impl(op)
    }
    fn write_impl(&mut self, op: WriteOp<&FileOperand, &[u8]>) -> vfsi_core::VfResult<WriteResult> {
        self.inner.write_impl(op)
    }
    fn seek_impl(
        &mut self,
        file: &FileOperand,
        offset: std::io::SeekFrom,
    ) -> vfsi_core::VfResult<u64> {
        self.inner.seek_impl(file, offset)
    }
    fn metadata_impl(
        &mut self,
        target: vfsi_core::api::Target<'_, FileOperand>,
        options: vfsi_core::api::AttrsOptions,
    ) -> vfsi_core::VfResult<VfAttrs> {
        self.inner.metadata_impl(target, options)
    }
    fn set_attributes_impl(
        &mut self,
        op: &vfsi_core::api::SetAttrsOp<vfsi_core::api::Target<'_, FileOperand>>,
    ) -> vfsi_core::VfResult<()> {
        self.inner.set_attributes_impl(op)
    }
}
impl VectorBackend for FaultBackend {
    fn vread_impl(&mut self, ops: &[ReadOp]) -> vfsi_core::VfResult<Vec<ReadResult>> {
        self.read_calls.lock().unwrap().push(
            ops.iter()
                .map(|op| op.file.path().unwrap().to_path_buf())
                .collect(),
        );
        let call = self.read_calls.lock().unwrap().len() - 1;
        self.read_requests.lock().unwrap().push(
            ops.iter()
                .map(|op| match op.offset {
                    VfOffset::At(offset) => (offset, op.length),
                    _ => panic!("range probe must use absolute offsets"),
                })
                .collect(),
        );
        if call == self.read_error_on_call {
            if let Some(error) = self.read_error.take() {
                return Err(error);
            }
        }
        if let Some((path, data)) = self.grow_before_read.take() {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(&data)
                .unwrap();
        }
        let limited: Vec<ReadOp> = ops
            .iter()
            .map(|op| {
                ReadOp::new(
                    op.file.clone(),
                    op.offset,
                    self.read_limit
                        .map_or(op.length, |limit| limit.min(op.length)),
                )
            })
            .collect();
        self.inner.vread_impl(&limited)
    }
    fn vwrite_impl(
        &mut self,
        ops: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_core::VfResult<Vec<WriteResult>> {
        self.write_calls.lock().unwrap().push(
            ops.iter()
                .map(|op| op.file().path().unwrap().to_path_buf())
                .collect(),
        );
        if let Some(error) = self.write_error.take() {
            return Err(error);
        }
        if let Some(counts) = self.write_counts.take() {
            assert_eq!(counts.len(), ops.len());
            let partial: Vec<WriteOp> = ops
                .iter()
                .zip(counts)
                .map(|(op, count)| {
                    assert!(count <= op.data().len());
                    let mut partial =
                        WriteOp::new(op.file().clone(), op.offset(), op.data()[..count].to_vec());
                    if op.creates() {
                        partial = partial.with_creation();
                    }
                    if op.truncates() {
                        partial = partial.with_truncate();
                    }
                    partial
                })
                .collect();
            self.inner
                .vwrite_impl(&partial.iter().map(WriteOp::borrowed).collect::<Vec<_>>())
        } else {
            self.inner.vwrite_impl(ops)
        }
    }
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VfRes {
        self.inner.vgetattrs_impl(attrs)
    }
    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VfRes {
        self.inner.vgetattrs_nofollow_impl(attrs)
    }
    fn vread_all_with_options_impl(
        &mut self,
        files: &[FileOperand],
        options: ReadAllOptions,
    ) -> vfsi_core::VfResult<Vec<Vec<u8>>> {
        self.inner.vread_all_with_options_impl(files, options)
    }
}
impl BindingBackend for FaultBackend {}
pub(crate) fn client(backend: FaultBackend) -> NfsClient {
    NfsClient {
        fs: Mutex::new(Some(Box::new(backend))),
        read_all_max_total_bytes: 1024,
        directory_max_entries: 100,
        directory_max_path_bytes: 1024,
        walk_max_depth: 10,
    }
}
