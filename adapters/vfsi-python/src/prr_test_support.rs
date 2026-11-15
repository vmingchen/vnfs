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
    pub(crate) read_error: Option<IndexedError>,
    pub(crate) read_errors: std::collections::VecDeque<IndexedError>,
    pub(crate) read_item_errors: Vec<(usize, std::io::Error)>,
    pub(crate) attrs_errors: std::collections::VecDeque<Option<IndexedError>>,
    pub(crate) attrs_item_errors: Vec<(usize, std::io::Error)>,
    pub(crate) attrs_omit_size: Option<usize>,
    pub(crate) attrs_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) extent_errors: std::collections::VecDeque<IndexedError>,
    pub(crate) extent_item_errors: Vec<(usize, vfsi_core::OpError)>,
    pub(crate) extent_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) write_item_errors: Vec<(usize, vfsi_core::OpError)>,
    pub(crate) read_error_on_call: usize,
    pub(crate) read_limit: Option<usize>,
    pub(crate) read_eof_at: Option<u64>,
    pub(crate) read_requests: ReadRequestTrace,
    pub(crate) grow_before_read: Option<(PathBuf, Vec<u8>)>,
    pub(crate) read_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) write_counts: Option<Vec<usize>>,
    pub(crate) write_overreports: bool,
    pub(crate) write_result_offset: Option<u64>,
    pub(crate) write_error: Option<IndexedError>,
    pub(crate) replace_after_write_error: Option<(PathBuf, Vec<u8>)>,
    pub(crate) read_all_errors_after_write: std::collections::VecDeque<IndexedError>,
    pub(crate) read_all_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) read_all_item_errors: Vec<(usize, vfsi_core::OpError)>,
    pub(crate) write_calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub(crate) close_item_errors: Vec<(usize, vfsi_core::OpError)>,
    pub(crate) consumed_close_error: Option<std::io::Error>,
    deferred_closes: Vec<FileOperand>,
}
impl FaultBackend {
    pub(crate) fn new(root: &Path, read_error: Option<IndexedError>) -> Self {
        Self {
            inner: vfsi_posix::backend(root).unwrap(),
            read_error,
            read_errors: Default::default(),
            read_item_errors: Default::default(),
            attrs_errors: Default::default(),
            attrs_calls: Default::default(),
            attrs_item_errors: Default::default(),
            attrs_omit_size: None,
            extent_errors: Default::default(),
            extent_item_errors: Default::default(),
            extent_calls: Default::default(),
            write_item_errors: Default::default(),
            read_error_on_call: 0,
            read_limit: None,
            read_eof_at: None,
            read_requests: Arc::default(),
            grow_before_read: None,
            read_calls: Arc::default(),
            write_counts: None,
            write_overreports: false,
            write_result_offset: None,
            write_error: None,
            replace_after_write_error: None,
            read_all_errors_after_write: std::collections::VecDeque::new(),
            read_all_calls: Arc::default(),
            read_all_item_errors: Vec::new(),
            write_calls: Arc::default(),
            close_item_errors: Vec::new(),
            consumed_close_error: None,
            deferred_closes: Vec::new(),
        }
    }
    fn attrs_results(&mut self, attrs: &mut [VfAttrs], follow: bool) -> vfsi_core::VecResult<()> {
        self.attrs_calls
            .lock()
            .unwrap()
            .push(attrs.iter().map(|attr| operand_trace(&attr.file)).collect());
        let mut injected = std::mem::take(&mut self.attrs_item_errors);
        if let Some(error) = self.attrs_errors.pop_front().flatten() {
            let (index, cause) = error.into_parts();
            let Some(index) = index else {
                return Err(cause);
            };
            if index >= attrs.len() {
                return Err(vfsi_core::internal::errors::invalid_reply(
                    "invalid injected metadata slot",
                ));
            }
            injected.push((index, cause));
        }
        let mut results = if follow {
            self.inner.vgetattrs_impl(attrs)?
        } else {
            self.inner.vgetattrs_nofollow_impl(attrs)?
        };
        if let Some(index) = self.attrs_omit_size.take() {
            assert!(results[index].is_ok());
            attrs[index].returned.remove(AttrMask::SIZE);
            attrs[index].size = 0;
        }
        for (index, error) in injected {
            // Read-only fault injection: actual sibling results come from the
            // single native batch, never inferred from a legacy prefix index.
            results[index] = Err(if vfsi_core::transport_kind(&error).is_some() {
                vfsi_core::OpError::unknown(error, None)
            } else {
                vfsi_core::OpError::failed(error, None)
            });
        }
        Ok(results)
    }
}
impl HandleBackend for FaultBackend {
    fn vfsync_impl(
        &mut self,
        files: &[FileOperand],
        mode: vfsi_core::api::SyncMode,
    ) -> vfsi_core::VecResult<()> {
        self.inner.vfsync_impl(files, mode)
    }
    fn sync_data(&mut self, file: &FileOperand) -> vfsi_core::VfResult<()> {
        self.inner.sync_data(file)
    }
    fn open_impl(&mut self, op: &vfsi_core::OpenOp) -> vfsi_core::VfResult<FileOperand> {
        self.inner.open_impl(op)
    }
    fn vclose_impl(&mut self, files: &[vfsi_core::FileOperand]) -> vfsi_core::VecResult<()> {
        let mut errors = std::mem::take(&mut self.close_item_errors);
        Ok(files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                if let Some(position) = errors.iter().position(|(slot, _)| *slot == index) {
                    Err(errors.remove(position).1)
                } else {
                    self.inner
                        .close_impl(file)
                        .map_err(vfsi_core::OpError::not_attempted)
                }
            })
            .collect())
    }

    fn close_impl(&mut self, file: &FileOperand) -> vfsi_core::VfResult<()> {
        self.inner.close_impl(file)?;
        if let Some(error) = self.consumed_close_error.take() {
            self.deferred_closes.push(file.clone());
            return Err(error);
        }
        Ok(())
    }

    fn close_deferred(&mut self, file: &FileOperand) -> vfsi_core::VfResult<()> {
        if let Some(index) = self
            .deferred_closes
            .iter()
            .position(|pending| pending.fd() == file.fd())
        {
            self.deferred_closes.remove(index);
            Ok(())
        } else {
            self.inner.close_impl(file)
        }
    }
    fn read_impl(&mut self, op: &ReadOp) -> vfsi_core::VfResult<ReadOutput> {
        self.inner.read_impl(op)
    }
    fn write_impl(&mut self, op: WriteOp<&FileOperand, &[u8]>) -> vfsi_core::VfResult<WriteOutput> {
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
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.inner.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, ops: &[ReadOp]) -> vfsi_sync::VecResult<ReadOutput> {
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
        let mut injected = std::mem::take(&mut self.read_item_errors);
        if let Some(error) = self.read_errors.pop_front() {
            let (index, cause) = error.into_parts();
            let Some(index) = index else {
                return Err(cause);
            };
            if index >= ops.len() {
                return Err(vfsi_core::internal::errors::invalid_reply(
                    "invalid injected read slot",
                ));
            }
            injected.push((index, cause));
        }
        if call == self.read_error_on_call
            && let Some(error) = self.read_error.take()
        {
            let (index, cause) = error.into_parts();
            let Some(index) = index else {
                return Err(cause);
            };
            if index >= ops.len() {
                return Err(vfsi_core::internal::errors::invalid_reply(
                    "invalid injected read slot",
                ));
            }
            injected.push((index, cause));
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
        let mut results = self.inner.vread_impl(&limited)?;
        if let Some(end) = self.read_eof_at {
            for result in results.iter_mut().filter_map(|result| result.as_mut().ok()) {
                result.eof |= result.offset.saturating_add(result.data.len() as u64) >= end;
            }
        }
        for (index, cause) in injected {
            results[index] = Err(if vfsi_core::transport_kind(&cause).is_some() {
                vfsi_core::OpError::unknown(cause, None)
            } else {
                vfsi_core::OpError::failed(cause, None)
            });
        }
        Ok(results)
    }
    fn vwrite_impl(
        &mut self,
        ops: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_core::VecResult<WriteOutput> {
        self.write_calls
            .lock()
            .unwrap()
            .push(ops.iter().map(|op| operand_trace(op.file())).collect());
        let mut rejected = std::mem::take(&mut self.write_item_errors);
        if let Some(error) = self.write_error.take() {
            let (index, cause) = error.into_parts();
            let Some(index) = index else {
                return Err(cause);
            };
            if index >= ops.len() {
                return Err(vfsi_core::internal::errors::invalid_reply(
                    "invalid injected write slot",
                ));
            }
            rejected.push((index, vfsi_core::OpError::unknown(cause, None)));
        }
        let mut result = if !rejected.is_empty() {
            let indices: Vec<_> = (0..ops.len())
                .filter(|index| !rejected.iter().any(|(failed, _)| failed == index))
                .collect();
            let pending: Vec<_> = indices.iter().map(|&index| ops[index]).collect();
            let mut slots: Vec<_> = (0..ops.len()).map(|_| None).collect();
            for (index, outcome) in indices.into_iter().zip(self.inner.vwrite_impl(&pending)?) {
                slots[index] = Some(outcome);
            }
            for (index, error) in rejected {
                slots[index] = Some(Err(error));
            }
            Ok(slots.into_iter().map(Option::unwrap).collect())
        } else if let Some(counts) = self.write_counts.take() {
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
        if self.write_overreports {
            self.write_overreports = false;
            if let Ok(items) = &mut result {
                items[0].as_mut().unwrap().bytes_written += 1;
            }
        }
        if let Some(offset) = self.write_result_offset.take()
            && let Ok(items) = &mut result
        {
            items[0].as_mut().unwrap().offset = offset;
        }
        if (result.is_err()
            || result
                .as_ref()
                .is_ok_and(|items| items.iter().any(Result::is_err)))
            && let Some((path, data)) = self.replace_after_write_error.take()
        {
            std::fs::write(path, data).unwrap();
        }
        result
    }
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        self.attrs_results(attrs, true)
    }
    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        self.attrs_results(attrs, false)
    }
    fn vcopy_impl(
        &mut self,
        pairs: &[ExtentPair],
        options: vfsi_core::CopyOptions,
    ) -> vfsi_core::VecResult<()> {
        self.extent_calls
            .lock()
            .unwrap()
            .push(pairs.iter().map(|pair| pair.src_path.clone()).collect());
        let mut rejected = std::mem::take(&mut self.extent_item_errors);
        if let Some(error) = self.extent_errors.pop_front() {
            let (index, cause) = error.into_parts();
            let Some(index) = index else {
                return Err(cause);
            };
            if index >= pairs.len() {
                return Err(vfsi_core::internal::errors::invalid_reply(
                    "invalid injected copy slot",
                ));
            }
            rejected.push((index, vfsi_core::OpError::unknown(cause, None)));
        }
        let indices: Vec<_> = (0..pairs.len())
            .filter(|index| !rejected.iter().any(|(failed, _)| failed == index))
            .collect();
        let pending: Vec<_> = indices.iter().map(|&index| pairs[index].clone()).collect();
        let mut results: Vec<_> = (0..pairs.len()).map(|_| None).collect();
        for (index, result) in indices
            .into_iter()
            .zip(self.inner.vcopy_impl(&pending, options)?)
        {
            results[index] = Some(result);
        }
        for (index, error) in rejected {
            results[index] = Some(Err(error));
        }
        Ok(results.into_iter().map(Option::unwrap).collect())
    }
    fn vread_all_with_options_impl(
        &mut self,
        files: &[FileOperand],
        options: ReadAllOptions,
    ) -> vfsi_core::VecResult<Vec<u8>> {
        self.read_all_calls
            .lock()
            .unwrap()
            .push(files.iter().map(operand_trace).collect());
        let mut injected = if self.write_calls.lock().unwrap().is_empty() {
            Default::default()
        } else {
            std::mem::take(&mut self.read_all_errors_after_write)
        };
        if let Some(position) = injected
            .iter()
            .position(|error| error.index().is_none_or(|index| index >= files.len()))
        {
            let error = injected.remove(position).unwrap();
            return Err(if error.index().is_none() {
                error.into_cause()
            } else {
                vfsi_core::internal::errors::invalid_reply("invalid injected whole-file slot")
            });
        }
        let mut outcomes = self.inner.vread_all_with_options_impl(files, options)?;
        for (index, error) in std::mem::take(&mut self.read_all_item_errors) {
            outcomes[index] = Err(error);
        }
        for error in injected {
            let (index, error) = error.into_parts();
            let index = index.expect("validated injected position");
            outcomes[index] = Err(if vfsi_core::transport_kind(&error).is_some() {
                vfsi_core::OpError::unknown(error, None)
            } else {
                vfsi_core::OpError::failed(error, None)
            });
        }
        Ok(outcomes)
    }
}
impl BindingBackend for FaultBackend {}
pub(crate) fn client(backend: impl BindingBackend + Send + 'static) -> NfsClient {
    NfsClient {
        fs: Mutex::new(Some(Box::new(backend))),
        append_fds: Mutex::new(HashSet::new()),
        read_all_max_total_bytes: 1024,
        directory_max_entries: 100,
        directory_max_path_bytes: 1024,
        walk_max_depth: 10,
    }
}
