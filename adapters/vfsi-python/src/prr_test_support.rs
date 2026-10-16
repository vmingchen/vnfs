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

fn operand_trace(file: &FileOperand) -> PathBuf {
    if let Some(path) = file.path() {
        path.to_path_buf()
    } else if let Some(fd) = file.fd() {
        PathBuf::from(format!("fd:{fd}"))
    } else {
        PathBuf::from(format!("{file:?}"))
    }
}

type ReadRequestTrace = Arc<Mutex<Vec<Vec<(u64, usize)>>>>;

pub(crate) struct FaultBackend {
    pub(crate) inner: LocalBackend,
    pub(crate) read_error: Option<VfError>,
    pub(crate) read_errors: std::collections::VecDeque<VfError>,
    pub(crate) attrs_errors: std::collections::VecDeque<Option<VfError>>,
    pub(crate) attrs_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) extent_errors: std::collections::VecDeque<VfError>,
    pub(crate) extent_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) write_errors: std::collections::VecDeque<VfError>,
    pub(crate) read_error_on_call: usize,
    pub(crate) read_limit: Option<usize>,
    pub(crate) read_requests: ReadRequestTrace,
    pub(crate) grow_before_read: Option<(PathBuf, Vec<u8>)>,
    pub(crate) read_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) write_counts: Option<Vec<usize>>,
    pub(crate) write_error: Option<VfError>,
    pub(crate) replace_after_write_error: Option<(PathBuf, Vec<u8>)>,
    pub(crate) read_all_errors_after_write: std::collections::VecDeque<VfError>,
    pub(crate) write_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
}
impl FaultBackend {
    pub(crate) fn new(root: &Path, read_error: Option<VfError>) -> Self {
        Self {
            inner: vfsi_posix::backend(root).unwrap(),
            read_error,
            read_errors: Default::default(),
            attrs_errors: Default::default(),
            attrs_calls: Default::default(),
            extent_errors: Default::default(),
            extent_calls: Default::default(),
            write_errors: Default::default(),
            read_error_on_call: 0,
            read_limit: None,
            read_requests: Arc::default(),
            grow_before_read: None,
            read_calls: Arc::default(),
            write_counts: None,
            write_error: None,
            replace_after_write_error: None,
            read_all_errors_after_write: std::collections::VecDeque::new(),
            write_calls: Arc::default(),
        }
    }
    fn record_attrs_call(&mut self, attrs: &[VfAttrs]) -> vfsi_core::VfRes {
        self.attrs_calls
            .lock()
            .unwrap()
            .push(attrs.iter().map(|attr| operand_trace(&attr.file)).collect());
        match self.attrs_errors.pop_front().flatten() {
            Some(error) => Err(error),
            None => Ok(()),
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
        self.read_calls
            .lock()
            .unwrap()
            .push(ops.iter().map(|op| operand_trace(&op.file)).collect());
        let call = self.read_calls.lock().unwrap().len() - 1;
        self.read_requests.lock().unwrap().push(
            ops.iter()
                .map(|op| match op.offset {
                    VfOffset::At(offset) => (offset, op.length),
                    _ => panic!("range probe must use absolute offsets"),
                })
                .collect(),
        );
        if let Some(error) = self.read_errors.pop_front() {
            return Err(error);
        }
        if call == self.read_error_on_call
            && let Some(error) = self.read_error.take()
        {
            return Err(error);
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
        self.write_calls
            .lock()
            .unwrap()
            .push(ops.iter().map(|op| operand_trace(op.file())).collect());
        if let Some(error) = self.write_errors.pop_front() {
            return Err(error);
        }
        if let Some(error) = self.write_error.take() {
            return Err(error);
        }
        let result = if let Some(counts) = self.write_counts.take() {
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
        };
        if result.is_err()
            && let Some((path, data)) = self.replace_after_write_error.take()
        {
            std::fs::write(path, data).unwrap();
        }
        result
    }
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VfRes {
        self.record_attrs_call(attrs)?;
        self.inner.vgetattrs_impl(attrs)
    }
    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VfRes {
        self.record_attrs_call(attrs)?;
        self.inner.vgetattrs_nofollow_impl(attrs)
    }
    fn vcopy_impl(
        &mut self,
        pairs: &[ExtentPair],
        options: vfsi_core::CopyOption,
    ) -> vfsi_core::VfRes {
        self.extent_calls
            .lock()
            .unwrap()
            .push(pairs.iter().map(|pair| pair.src_path.clone()).collect());
        if let Some(error) = self.extent_errors.pop_front() {
            return Err(error);
        }
        vfsi_sync::backend::helpers::vcopy_impl_default(self, pairs, options)
    }
    fn vread_all_with_options_impl(
        &mut self,
        files: &[FileOperand],
        options: ReadAllOptions,
    ) -> vfsi_core::VfResult<Vec<Vec<u8>>> {
        if !self.write_calls.lock().unwrap().is_empty()
            && let Some(error) = self.read_all_errors_after_write.pop_front()
        {
            return Err(error);
        }
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
