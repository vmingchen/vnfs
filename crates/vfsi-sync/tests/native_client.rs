use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use vfsi_sync::api::ReadOptions;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{
    Capabilities, DirEntry, DirPageCursor, FileOperand, FsClient, ListDirOptions, OpenFlags,
    OpenOp, ReadIntoResult, ReadOp, ReadResult, SetAttrsOp, Target, VfAttrs, VfError, VfOffset,
    VfResult, Vfsi, VfsiExt, WriteOp, WriteResult,
};

#[test]
fn client_policy_applies_to_scalar_vector_and_into_reads_before_dispatch() {
    let backend = ScalarOnly {
        data: b"abcdef".to_vec(),
        ..ScalarOnly::default()
    };
    let read_calls = Arc::clone(&backend.read_calls);
    let client = FsClient::new(backend).with_limits(
        vfsi_sync::ResourceLimits::new()
            .with_max_read_bytes(3)
            .with_stream_chunk_size(std::num::NonZeroUsize::new(2).unwrap()),
    );
    assert_eq!(client.clone().limits(), client.limits());
    assert_eq!(client.capabilities().unwrap(), Capabilities::empty());
    assert_eq!(
        client.read("/file").unwrap_err().kind(),
        std::io::ErrorKind::FileTooLarge
    );
    assert_eq!(
        client
            .read_with_options(
                "/file",
                ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(6).unwrap())
            )
            .unwrap(),
        b"abcdef"
    );
    let file = client.open("/file").unwrap();
    let before = read_calls.load(Ordering::SeqCst);
    assert!(
        client
            .vread_native(&[file.read_request_at(0, 4)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .is_err()
    );
    let mut buffer = [0; 4];
    assert!(
        client
            .vread_into_native(&mut [file.read_request_at_into(0, &mut buffer)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .is_err()
    );
    assert_eq!(read_calls.load(Ordering::SeqCst), before);
    let result = client
        .vread_into_with_limit_native(&mut [file.read_request_at_into(0, &mut buffer)], 4)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(result[0].bytes_read, 4);
    assert_eq!(&buffer, b"abcd");
    buffer.fill(0xff);
    let before = read_calls.load(Ordering::SeqCst);
    assert!(
        client
            .vread_into_with_limit_native(&mut [file.read_request_at_into(0, &mut buffer)], 3)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .is_err()
    );
    assert_eq!(read_calls.load(Ordering::SeqCst), before);
    assert_eq!(buffer, [0xff; 4]);
    let mut offsets = Vec::new();
    let result = client
        .read_stream("/file", |offset, bytes| {
            offsets.push((offset, bytes.len()));
            Ok(if offset == 0 {
                std::ops::ControlFlow::Continue(())
            } else {
                std::ops::ControlFlow::Break(())
            })
        })
        .unwrap();
    assert_eq!(offsets, [(0, 2), (2, 2)]);
    assert_eq!(
        result,
        vfsi_sync::StreamCompletion::Stopped { next_offset: 4 }
    );
    assert_eq!(
        client
            .read_stream("/file", |_, _| Ok(std::ops::ControlFlow::Continue(())))
            .unwrap(),
        vfsi_sync::StreamCompletion::Complete
    );
}

#[test]
fn scalar_read_uses_bounded_whole_file_api_without_opening_a_handle() {
    for read_failure in [false, true] {
        let client = FsClient::new(ScalarOnly {
            data: b"data".to_vec(),
            close_failures_remaining: 1,
            read_failure,
            ..ScalarOnly::default()
        });
        let result = client.read("/file");
        if read_failure {
            let error = result.unwrap_err();
            assert!(!error.is_transport());
            assert_eq!(error.operation(), Some("read"));
        } else {
            assert_eq!(result.unwrap(), b"data");
        }
        let backend = client.into_inner().unwrap();
        assert_eq!(backend.close_calls, 0);
        assert!(!backend.open);
    }
}

#[test]
fn into_inner_drains_cleanup_queued_while_an_earlier_close_is_running() {
    let close_entered = Arc::new(std::sync::Barrier::new(2));
    let close_release = Arc::new(std::sync::Barrier::new(2));
    let client = FsClient::new(ScalarOnly {
        blocked_close: Some((close_entered.clone(), close_release.clone())),
        ..ScalarOnly::default()
    });
    let first = client.open("/first").unwrap();
    let concurrent = client.open("/concurrent").unwrap();
    drop(first);

    let dropper = std::thread::spawn(move || {
        close_entered.wait();
        drop(concurrent);
        close_release.wait();
    });
    let backend = client
        .into_inner()
        .expect("exclusive extraction must close every queued handle");
    dropper.join().unwrap();

    assert_eq!(backend.close_calls, 2);
    assert!(!backend.open);
}

type Notifications = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;
type MetadataCalls = Arc<Mutex<Vec<(bool, Vec<FileOperand>)>>>;
type SyncCalls = Arc<Mutex<Vec<(vfsi_core::api::SyncMode, Vec<FileOperand>)>>>;

type AttributeCalls = Arc<Mutex<Vec<(bool, Vec<Option<u64>>)>>>;

#[derive(Default)]
struct ScalarOnly {
    identity_preflight_failure: bool,
    identity_ref_failure: bool,
    descriptor_objects: std::collections::HashMap<i32, u64>,
    named_objects: std::collections::HashMap<std::path::PathBuf, u64>,
    unknown_objects: bool,
    unknown_object_fd: Option<i32>,
    alias_objects: bool,
    sync_calls: SyncCalls,
    sync_failure: Option<VfError>,
    sync_result_count: Option<usize>,
    attrs_calls: AttributeCalls,
    attrs_failure: Option<(usize, VfError)>,
    stats_calls: Arc<AtomicUsize>,
    stats_result_count: Option<usize>,
    stats_error_index: Option<usize>,
    notifications: Notifications,
    blocked_read: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    blocked_close: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    close_observed: Arc<AtomicUsize>,
    data: Vec<u8>,
    cursor: u64,
    open: bool,
    oversized_read: bool,
    oversized_write_count: bool,
    read_failure: bool,
    read_failure_call: Option<usize>,
    transport_failure: bool,
    vector_result_limit: Arc<Mutex<Option<usize>>>,
    wrong_result_file: bool,
    wrong_result_offset: bool,
    close_failures_remaining: usize,
    close_calls: usize,
    direct_into_only: bool,
    into_calls: Arc<AtomicUsize>,
    read_calls: Arc<AtomicUsize>,
    write_calls: Arc<AtomicUsize>,
    max_write_once: Option<usize>,
    interrupt_write_call: Option<usize>,
    open_requests: Arc<Mutex<Vec<OpenOp<'static>>>>,
    directory_entries: usize,
    directory_page_sizes: Arc<Mutex<Vec<usize>>>,
    directory_fields: Arc<Mutex<Vec<vfsi_sync::AttrMask>>>,
}

#[test]
fn std_io_collection_obeys_client_limits_and_retains_cursor() {
    for limit in [0, 3, 6, 7] {
        let client = FsClient::new(ScalarOnly {
            data: b"abcdef".to_vec(),
            ..Default::default()
        })
        .with_limits(vfsi_sync::ResourceLimits::new().with_max_read_bytes(limit));
        let mut file = client.open("/file").unwrap();
        let mut io = client.std_io(&file);
        let mut output = Vec::new();
        let result = io.read_to_end(&mut output);
        if limit < 6 {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::FileTooLarge);
            assert_eq!(io.stream_position().unwrap(), (limit + 1) as u64);
            assert_eq!(output, &b"abcdef"[..limit]);
        } else {
            assert_eq!(result.unwrap(), 6);
            assert_eq!(output, b"abcdef");
            assert_eq!(io.stream_position().unwrap(), 6);
        }
        // EOF probing also works with a zero remaining collection budget.
        io.seek(SeekFrom::Start(6)).unwrap();
        assert_eq!(io.read_to_end(&mut Vec::new()).unwrap(), 0);
        drop(io);
        file.try_close().unwrap();
        assert!(client.std_io(&file).read(&mut [0]).is_err());
    }
}

impl VectorBackend for ScalarOnly {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(vfsi_sync::VfError::from_kind(
                None,
                std::io::ErrorKind::InvalidInput,
            ));
        }
        Ok(paths
            .iter()
            .zip(flags)
            .zip(modes)
            .map(|((path, _flags), mode)| {
                self.open_impl(&OpenOp::new(path, OpenFlags::READ).mode(*mode))
                    .map_err(|cause| {
                        if cause.is_transport() {
                            vfsi_sync::OpError::unknown(cause, None)
                        } else {
                            vfsi_sync::OpError::failed(cause, None)
                        }
                    })
            })
            .collect())
    }

    fn vopen_impl(&mut self, requests: &[OpenOp]) -> vfsi_core::VecResult<FileOperand> {
        if self.transport_failure {
            return Err(VfError::transport(None, "reply lost"));
        }
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        Ok(requests
            .iter()
            .take(limit)
            .map(|request| {
                self.open_impl(request)
                    .map_err(|error| vfsi_core::OpError::failed(error, None))
            })
            .collect())
    }

    fn listdir_page_impl(
        &mut self,
        dir: &std::path::Path,
        masks: vfsi_sync::AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
        _follow_symlinks: bool,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        self.directory_fields.lock().unwrap().push(masks);
        self.directory_page_sizes.lock().unwrap().push(page_size);
        let start = cursor
            .map(|cursor| cursor.into_state::<usize>())
            .transpose()?
            .unwrap_or(0);
        let count = if max_entries == 0 {
            self.directory_entries
        } else {
            self.directory_entries
                .min(start.saturating_add(max_entries))
        };
        let end = start.saturating_add(page_size).min(count);
        let entries = (start..end)
            .map(|index| VfAttrs {
                file: FileOperand::from_os_path(&dir.join(format!("item-{index:04}"))),
                masks,
                mode: 0o100644,
                size: index as u64,
                ..VfAttrs::default()
            })
            .collect();
        Ok((entries, (end < count).then(|| DirPageCursor::new(end))))
    }

    fn vread_impl(&mut self, requests: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        let result: VfResult<Vec<ReadResult>> = {
            let limit = self
                .vector_result_limit
                .lock()
                .expect("vector limit poisoned")
                .unwrap_or(usize::MAX);
            let mut results: Vec<_> = requests
                .iter()
                .take(limit)
                .map(|request| self.read_impl(request))
                .collect::<VfResult<_>>()?;
            if let Some(result) = results.first_mut() {
                if self.wrong_result_file {
                    result.file = FileOperand::from_fd(999);
                }
                if self.wrong_result_offset {
                    result.offset = result.offset.saturating_add(1);
                }
            }
            Ok(results)
        };
        result.map(|results| results.into_iter().map(Ok).collect())
    }

    fn vread_into_impl(
        &mut self,
        requests: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> vfsi_sync::VecResult<ReadIntoResult> {
        if requests.len() != buffers.len() {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        let mut results: Vec<vfsi_sync::OpResult<ReadIntoResult>> = requests
            .iter()
            .zip(buffers.iter_mut())
            .take(limit)
            .enumerate()
            .map(|(index, (request, buffer))| {
                self.read_into_impl(request, buffer)
                    .map_err(|error| vfsi_sync::OpError::failed(error.with_index(index), None))
            })
            .collect();
        if let Some(Ok(result)) = results.first_mut() {
            if self.wrong_result_file {
                result.file = FileOperand::from_fd(999);
            }
            if self.wrong_result_offset {
                result.offset = result.offset.saturating_add(1);
            }
        }
        Ok(results)
    }

    fn vwrite_impl(
        &mut self,
        requests: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_sync::VecResult<WriteResult> {
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        let mut results: Vec<_> = requests
            .iter()
            .take(limit)
            .map(|request| {
                self.write_impl(*request)
                    .map_err(|cause| vfsi_sync::OpError::failed(cause, None))
            })
            .collect();
        if let Some(Ok(result)) = results.first_mut() {
            if self.wrong_result_file {
                result.file = FileOperand::from_fd(999);
            }
            if self.wrong_result_offset {
                result.offset = result.offset.saturating_add(1);
            }
        }
        Ok(results)
    }

    fn read_dir_page_with_fields_impl(
        &mut self,
        path: &std::path::Path,
        fields: vfsi_sync::AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
        self.directory_fields.lock().unwrap().push(fields);
        self.read_dir_page_impl(path, cursor, page_size, max_entries)
    }
    fn create_dir_impl(&mut self, _: &std::path::Path, _: u32) -> VfResult<()> {
        Ok(())
    }

    fn read_dir_impl(&mut self, _: &std::path::Path, _: ListDirOptions) -> VfResult<Vec<DirEntry>> {
        unreachable!("the visitor must use paged enumeration")
    }

    fn read_dir_page_impl(
        &mut self,
        _: &std::path::Path,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
        self.directory_page_sizes.lock().unwrap().push(page_size);
        let start = cursor
            .map(|cursor| cursor.into_state::<usize>())
            .transpose()?
            .unwrap_or(0);
        let ceiling = if max_entries == 0 {
            self.directory_entries
        } else {
            self.directory_entries.min(max_entries)
        };
        let end = start.saturating_add(page_size).min(ceiling);
        let entries = (start..end)
            .map(|index| {
                DirEntry::new(
                    format!("/tree/item-{index:04}").into(),
                    vfsi_core::metadata_from_attrs(VfAttrs::default()),
                )
            })
            .collect();
        let next = (end < ceiling).then(|| DirPageCursor::new(end));
        Ok((entries, next))
    }
}

/// These wrappers exercise the actual minimal contracts, independently of the
/// full probe's vector/directory overrides.
#[derive(Default)]
struct DefaultBackend {
    scalar: ScalarOnly,
    vector_reads: usize,
}

#[derive(Default)]
struct PagedBackend {
    scalar: ScalarOnly,
    page_calls: Arc<Mutex<Vec<(vfsi_sync::AttrMask, usize, usize)>>>,
}

macro_rules! handle_contract {
    (@close $close:item) => { $close };
    (@close) => {
        fn vclose_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
            self.scalar.vclose_impl(files)
        }
    };
    ($backend:ty $(, $close:item)? $(; $extra:item)*) => {
        impl HandleBackend for $backend {
            handle_contract!(@close $($close)?);
            $($extra)*
            fn open_object_ref(
                &self,
                file: &FileOperand,
            ) -> VfResult<Option<vfsi_sync::backend::OpenObjectRef<'_>>> {
                self.scalar.open_object_ref(file)
            }
            fn open_impl(&mut self, request: &OpenOp) -> VfResult<FileOperand> {
                self.scalar.open_impl(request)
            }
            fn open_path_impl(
                &mut self,
                base: vfsi_sync::VfPathBase,
                path: &std::path::Path,
                flags: i32,
                mode: u32,
            ) -> VfResult<FileOperand> {
                let _ = (base, flags, mode);
                self.open_impl(&OpenOp::new(path, OpenFlags::READ))
            }
            fn close_impl(&mut self, file: &FileOperand) -> VfResult<()> {
                self.scalar.close_impl(file)
            }
            fn vfsync_impl(
                &mut self,
                files: &[FileOperand],
                mode: vfsi_core::api::SyncMode,
            ) -> vfsi_sync::VecResult<()> {
                self.scalar.vfsync_impl(files, mode)
            }
            fn sync_all(&mut self, file: &FileOperand) -> VfResult<()> {
                self.scalar.sync_all(file)
            }
            fn sync_data(&mut self, file: &FileOperand) -> VfResult<()> {
                self.scalar.sync_data(file)
            }
            fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
                self.scalar.read_impl(request)
            }
            fn write_impl(
                &mut self,
                request: WriteOp<&FileOperand, &[u8]>,
            ) -> VfResult<WriteResult> {
                self.scalar.write_impl(request)
            }
            fn seek_impl(&mut self, file: &FileOperand, position: SeekFrom) -> VfResult<u64> {
                self.scalar.seek_impl(file, position)
            }
            fn metadata_impl(
                &mut self,
                target: Target<'_, FileOperand>,
                options: vfsi_core::api::AttrsOptions,
            ) -> VfResult<VfAttrs> {
                self.scalar.metadata_impl(target, options)
            }
            fn set_attributes_impl(
                &mut self,
                update: &SetAttrsOp<Target<'_, FileOperand>>,
            ) -> VfResult<()> {
                self.scalar.set_attributes_impl(update)
            }
        }
    };
}

handle_contract!(DefaultBackend);
handle_contract!(PagedBackend);

#[derive(Default)]
struct LegacyDirectoryBackend {
    scalar: ScalarOnly,
    calls: usize,
    last_fields: Option<vfsi_sync::AttrMask>,
}
handle_contract!(LegacyDirectoryBackend);
impl VectorBackend for LegacyDirectoryBackend {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, _: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        panic!("unexpected read")
    }
    fn listdir_impl(
        &mut self,
        _: &std::path::Path,
        fields: vfsi_sync::AttrMask,
        _: usize,
        _: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        self.calls += 1;
        self.last_fields = Some(fields);
        Ok(Vec::new())
    }
}

#[test]
fn scalar_directory_default_rejects_unsupported_no_follow_before_listing() {
    let mut backend = LegacyDirectoryBackend::default();
    let path = std::path::Path::new("/tree");
    let error = backend
        .read_dir_impl(
            path,
            ListDirOptions::new().with_allow_symlink_components(false),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(error.operation(), Some("read_dir"));
    assert_eq!(error.path(), Some(path));
    assert_eq!(backend.calls, 0);
}

#[test]
fn scalar_directory_default_preserves_selected_fields() {
    let mut backend = LegacyDirectoryBackend::default();
    let fields = vfsi_sync::AttrMask::SIZE | vfsi_sync::AttrMask::MODE;
    backend
        .read_dir_impl(
            std::path::Path::new("/tree"),
            ListDirOptions::new().with_fields(vfsi_sync::AttrMask::SIZE),
        )
        .unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.last_fields, Some(fields));
}

#[test]
fn legacy_directory_fallback_fails_closed_without_no_follow_support() {
    let mut backend = LegacyDirectoryBackend::default();
    let dir = std::path::Path::new("/tree");
    let page = backend
        .vlistdir_pages_impl(&[dir], vfsi_sync::AttrMask::MODE, vec![None], 1, 4, true)
        .unwrap();
    assert!(page[0].as_ref().unwrap().0.is_empty());
    assert_eq!(backend.calls, 1);
    let error = backend
        .vlistdir_pages_impl(&[dir], vfsi_sync::AttrMask::MODE, vec![None], 1, 4, false)
        .unwrap()
        .remove(0)
        .err()
        .expect("legacy listing cannot provide no-follow guarantees");
    assert_eq!(error.cause().kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(
        backend.calls, 1,
        "do not check and then call a following backend"
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WriteObservation {
    pointer: usize,
    length: usize,
    offset: VfOffset,
    create: bool,
    truncate: bool,
}

#[derive(Default)]
struct WriteBoundaryProbe {
    scalar: ScalarOnly,
    append_end: Option<u64>,
    append_gap: u64,
    calls: Arc<Mutex<Vec<Vec<WriteObservation>>>>,
    write_waves: std::collections::VecDeque<Vec<vfsi_sync::OpResult<usize>>>,
    write_outcomes: Option<Vec<vfsi_sync::OpResult<usize>>>,
    failure: Option<VfError>,
    failure_after: usize,
}
handle_contract!(WriteBoundaryProbe);
impl VectorBackend for WriteBoundaryProbe {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, _: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        let result: VfResult<Vec<ReadResult>> = { Err(VfError::unsupported(0)) };
        result.map(|results| results.into_iter().map(Ok).collect())
    }
    fn vwrite_impl(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_sync::VecResult<WriteResult> {
        self.calls.lock().unwrap().push(
            writes
                .iter()
                .map(|op| WriteObservation {
                    pointer: op.data().as_ptr() as usize,
                    length: op.data().len(),
                    offset: op.offset(),
                    create: op.creates(),
                    truncate: op.truncates(),
                })
                .collect(),
        );
        if let Some(outcomes) = self
            .write_outcomes
            .take()
            .or_else(|| self.write_waves.pop_front())
        {
            return Ok(outcomes
                .into_iter()
                .enumerate()
                .map(|(index, result)| {
                    let write = writes.get(index).unwrap_or(&writes[0]);
                    result.map(|bytes_written| WriteResult {
                        file: (*write.file()).clone(),
                        offset: match write.offset() {
                            VfOffset::At(offset) => offset,
                            _ => 0,
                        },
                        bytes_written,
                        stable: true,
                    })
                })
                .collect());
        }
        if let Some(error) = &self.failure
            && self.calls.lock().unwrap().len() > self.failure_after
        {
            return Err(error.clone());
        }
        Ok(writes
            .iter()
            .map(|op| WriteResult {
                file: op.file().clone(),
                offset: match op.offset() {
                    VfOffset::End => {
                        let end = self.append_end.as_mut().expect("append target configured");
                        let offset = *end;
                        *end += op.data().len().min(2) as u64 + self.append_gap;
                        offset
                    }
                    VfOffset::At(offset) => offset,
                    _ => 0,
                },
                bytes_written: op.data().len().min(2),
                stable: true,
            })
            .map(Ok)
            .collect())
    }
}

#[test]
fn shared_write_operations_keep_storage_borrowed_through_dynamic_vector_dispatch() {
    let mut concrete = WriteBoundaryProbe::default();
    let calls = Arc::clone(&concrete.calls);
    let backend: &mut dyn VectorBackend = &mut concrete;
    let storage = [
        WriteOp::from_path("/created", VfOffset::At(0), b"abc".to_vec())
            .with_creation()
            .with_truncate(),
        WriteOp::from_fd(7, VfOffset::Cur, b"def".to_vec()),
    ];
    let writes: Vec<_> = storage.iter().map(vfsi_core::WriteOp::borrowed).collect();
    let results = backend
        .vwrite_impl(&writes)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].file, *storage[0].file());
    assert_eq!(results[1].file, *storage[1].file());
    assert_eq!(
        *calls.lock().unwrap(),
        [vec![
            WriteObservation {
                pointer: storage[0].data().as_ptr() as usize,
                length: 3,
                offset: VfOffset::At(0),
                create: true,
                truncate: true
            },
            WriteObservation {
                pointer: storage[1].data().as_ptr() as usize,
                length: 3,
                offset: VfOffset::Cur,
                create: false,
                truncate: false
            },
        ]]
    );
}

#[test]
fn consolidated_write_dispatch_retries_only_successful_short_writes_without_copying() {
    for failure in [
        None,
        Some(VfError::errno(1, libc::ENOSPC as u32)),
        Some(VfError::transport(None, "lost write reply")),
    ] {
        let backend = WriteBoundaryProbe {
            failure: failure.clone(),
            ..Default::default()
        };
        let calls = Arc::clone(&backend.calls);
        let client = FsClient::new(backend);
        let files = [
            client.open("/first").unwrap(),
            client.open("/second").unwrap(),
        ];
        let data = b"abcdef";
        let ops = [
            files[0].write_request_at(0, data),
            files[1].write_request_at(10, data),
        ];
        let result = client.vwrite_all_native(&ops).and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        });
        let calls = calls.lock().unwrap();
        if let Some(error) = failure {
            let actual = result.unwrap_err();
            assert_eq!(
                actual.index(),
                Some(0),
                "strict selection identifies the first rejected input"
            );
            assert_eq!(actual.is_transport(), error.is_transport());
            if error.index().is_some() {
                assert_eq!(actual.path(), Some(std::path::Path::new("/first")));
            }
            assert_eq!(
                calls.len(),
                1,
                "indexed and ambiguous failures must never replay"
            );
        } else {
            assert!(
                result
                    .unwrap()
                    .iter()
                    .all(|r| r.bytes_written == data.len())
            );
            assert_eq!(calls.len(), 3);
            for (wave, requests) in calls.iter().enumerate() {
                assert_eq!(requests.len(), 2, "independent requests stay vectorized");
                for (index, op) in requests.iter().enumerate() {
                    assert_eq!(op.pointer, data[2 * wave..].as_ptr() as usize);
                    assert_eq!(op.length, data.len() - 2 * wave);
                    assert_eq!(op.offset, VfOffset::At(index as u64 * 10 + 2 * wave as u64));
                    assert!(!op.create && !op.truncate);
                }
            }
        }
    }
}

#[derive(Default)]
struct AttrsBackend {
    removals: usize,
    metadata_calls: MetadataCalls,
    metadata_failure: Option<usize>,
    scalar: ScalarOnly,
    calls: Vec<(bool, Vec<VfAttrs>)>,
    failure: Option<(usize, VfError)>,
    queries: Vec<(bool, FileOperand, vfsi_core::AttrMask)>,
}
handle_contract!(AttrsBackend);
impl VectorBackend for AttrsBackend {
    fn vremove_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        self.removals += files.len();
        Ok(files.iter().map(|_| Ok(())).collect())
    }

    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, _: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        let result: VfResult<Vec<ReadResult>> = { Err(VfError::unsupported(0)) };
        result.map(|results| results.into_iter().map(Ok).collect())
    }
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        let requested = attrs.len();
        self.metadata_calls
            .lock()
            .unwrap()
            .push((true, attrs.iter().map(|a| a.file.clone()).collect()));
        self.queries
            .push((true, attrs[0].file.clone(), attrs[0].masks));
        Ok((0..requested)
            .map(|index| {
                if self.metadata_failure == Some(index) {
                    Err(vfsi_core::OpError::failed(
                        VfError::errno(None, libc::EACCES as u32),
                        None,
                    ))
                } else {
                    Ok(())
                }
            })
            .collect())
    }
    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        let requested = attrs.len();
        self.metadata_calls
            .lock()
            .unwrap()
            .push((false, attrs.iter().map(|a| a.file.clone()).collect()));
        self.queries
            .push((false, attrs[0].file.clone(), attrs[0].masks));
        Ok((0..requested)
            .map(|index| {
                if self.metadata_failure == Some(index) {
                    Err(vfsi_core::OpError::failed(
                        VfError::errno(None, libc::EACCES as u32),
                        None,
                    ))
                } else {
                    Ok(())
                }
            })
            .collect())
    }
    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        self.record_attrs(true, attrs)
    }
    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        self.record_attrs(false, attrs)
    }
}
impl AttrsBackend {
    fn record_attrs(&mut self, follow: bool, attrs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        self.calls.push((follow, attrs.to_vec()));
        if let Some((call, error)) = &self.failure
            && self.calls.len() == *call
        {
            if let Some(index) = error.index().filter(|&index| index < attrs.len()) {
                return Ok((0..attrs.len())
                    .map(|position| {
                        if position == index {
                            Err(vfsi_core::OpError::failed(error.clone(), None))
                        } else {
                            Ok(())
                        }
                    })
                    .collect());
            }
            return Err(error.clone());
        }
        Ok(attrs.iter().map(|_| Ok(())).collect())
    }
}

#[test]
fn shared_attribute_ops_preserve_all_fields_at_the_raw_backend_boundary() {
    use vfsi_sync::backend::helpers::{native_metadata_impl_default, vsetattrs_typed_default};
    use vfsi_sync::{AttrMask, Permissions};
    let mut concrete = AttrsBackend::default();
    let backend: &mut dyn VectorBackend = &mut concrete;
    let file = FileOperand::from_fd(7);
    let op = SetAttrsOp::file(&file)
        .len(0)
        .permissions(Permissions::from_mode(0o640))
        .uid(42)
        .gid(43)
        .accessed(std::time::UNIX_EPOCH + std::time::Duration::new(123, 456))
        .modified(std::time::UNIX_EPOCH - std::time::Duration::new(1, 25))
        .with_follow_final_symlink(false);
    vsetattrs_typed_default(backend, std::slice::from_ref(&op)).unwrap();
    // Borrowing the operation leaves it reusable; changing options performs no I/O.
    vsetattrs_typed_default(backend, std::slice::from_ref(&op)).unwrap();
    let fields = AttrMask::MODE
        | AttrMask::SIZE
        | AttrMask::UID
        | AttrMask::GID
        | AttrMask::ATIME
        | AttrMask::MTIME;
    let attrs = native_metadata_impl_default(
        backend,
        Target::File(&file),
        vfsi_core::api::AttrsOptions::new()
            .with_fields(fields)
            .with_follow_final_symlink(false),
    )
    .unwrap();
    assert_eq!(attrs.file, file);
    assert_eq!(attrs.masks, fields);
    native_metadata_impl_default(
        backend,
        Target::Path(std::path::Path::new("/link")),
        vfsi_core::api::AttrsOptions::new().with_fields(AttrMask::SIZE),
    )
    .unwrap();
    assert_eq!(concrete.calls.len(), 2);
    for (follow, attrs) in &concrete.calls {
        assert!(*follow, "opened-object updates ignore path symlink policy");
        assert_eq!(attrs.len(), 1);
        let attrs = &attrs[0];
        assert_eq!(attrs.file, file);
        assert_eq!(attrs.masks, fields);
        assert_eq!(
            (attrs.mode, attrs.size, attrs.uid, attrs.gid),
            (0o640, 0, 42, 43)
        );
        assert_eq!((attrs.atime_sec, attrs.atime_nsec), (123, 456));
        assert_eq!((attrs.mtime_sec, attrs.mtime_nsec), (-2, 999_999_975));
    }
    assert_eq!(
        concrete.queries,
        [
            (false, file, fields),
            (true, FileOperand::from_path("/link"), AttrMask::SIZE),
        ]
    );
    let client = FsClient::new(concrete);
    client
        .vgetattrs_native::<&str>(&[], AttrMask::MODE, false)
        .unwrap();
    assert_eq!(
        client
            .vgetattrs_native(&["/link", "/file"], AttrMask::MODE, false)
            .unwrap()
            .len(),
        2
    );
    let backend = client.into_inner().unwrap();
    assert_eq!(
        backend.queries.len(),
        3,
        "one no-follow vector, no empty dispatch"
    );
    assert_eq!(
        backend.queries[2],
        (false, FileOperand::from_path("/link"), AttrMask::MODE)
    );
}

#[test]
fn handle_attribute_updates_ignore_path_symlink_policy_without_splitting_batches() {
    use vfsi_sync::backend::helpers::vsetattrs_typed_default;
    let file = FileOperand::from_fd(7);
    let path = std::path::Path::new("/link");
    let ops = [
        SetAttrsOp::new(Target::Path(path))
            .len(1)
            .with_follow_final_symlink(false),
        SetAttrsOp::file(&file)
            .len(2)
            .with_follow_final_symlink(false),
        SetAttrsOp::new(Target::Path(path)).len(3),
        SetAttrsOp::file(&file).len(4),
    ];
    let mut backend = AttrsBackend {
        failure: Some((1, VfError::unsupported(None))),
        ..Default::default()
    };
    let outcomes = vsetattrs_typed_default(&mut backend, &ops).unwrap();
    assert_eq!(outcomes.len(), ops.len());
    assert_eq!(
        outcomes[0].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::NotAttempted
    );
    assert!(outcomes[1..].iter().all(Result::is_ok));
    assert_eq!(
        backend
            .calls
            .iter()
            .map(|(follow, attrs)| (*follow, attrs.iter().map(|a| a.size).collect::<Vec<_>>()))
            .collect::<Vec<_>>(),
        [(false, vec![1]), (true, vec![2, 3, 4])]
    );
}

#[test]
fn shared_native_attribute_batches_isolate_validation_and_preserve_policy_runs() {
    use vfsi_sync::backend::helpers::vsetattrs_typed_default;
    let ops = [
        SetAttrsOp::new(Target::Path(std::path::Path::new("/a"))).len(1),
        SetAttrsOp::new(Target::Path(std::path::Path::new("/b")))
            .len(2)
            .with_follow_final_symlink(false),
        SetAttrsOp::new(Target::Path(std::path::Path::new("/c")))
            .len(3)
            .with_follow_final_symlink(false),
        SetAttrsOp::new(Target::Path(std::path::Path::new("/a"))).len(4),
    ];
    let mut backend = AttrsBackend::default();
    vsetattrs_typed_default(&mut backend, &[]).unwrap();
    assert!(backend.calls.is_empty());
    vsetattrs_typed_default(&mut backend, &ops).unwrap();
    assert_eq!(
        backend
            .calls
            .iter()
            .map(|(follow, attrs)| (*follow, attrs.iter().map(|a| a.size).collect::<Vec<_>>()))
            .collect::<Vec<_>>(),
        [(true, vec![1]), (false, vec![2, 3]), (true, vec![4])]
    );
    backend.calls.clear();
    for invalid in [ops[3].uid(u32::MAX), ops[3].gid(u32::MAX)] {
        backend.calls.clear();
        let results = vsetattrs_typed_default(&mut backend, &[ops[0], invalid]).unwrap();
        assert!(results[0].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(std::path::Path::new("/a")));
        assert_eq!(backend.calls.len(), 1);
        assert_eq!(backend.calls[0].1.len(), 1);
    }
    for failure in [
        VfError::errno(1, libc::ENOENT as u32),
        VfError::errno(2, libc::ENOENT as u32),
        VfError::transport(None, "reply lost"),
    ] {
        backend.failure = Some((2, failure.clone()));
        backend.calls.clear();
        let results = vsetattrs_typed_default(&mut backend, &ops).unwrap();
        assert!(results[0].is_ok() && results[3].is_ok());
        let error = vfsi_core::internal::collect_vec_result(Ok(results), ops.len()).unwrap_err();
        assert_eq!(backend.calls.len(), 3);
        if failure.index() == Some(1) {
            assert_eq!(error.index(), Some(2));
            assert_eq!(error.raw_code(), libc::ENOENT as u32);
        } else {
            assert!(error.is_transport());
            assert_eq!(error.index(), Some(1));
        }
    }
}

impl VectorBackend for PagedBackend {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, _: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        panic!("directory paging must not issue reads")
    }

    fn listdir_page_impl(
        &mut self,
        dir: &std::path::Path,
        masks: vfsi_sync::AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
        _follow_symlinks: bool,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        self.page_calls
            .lock()
            .unwrap()
            .push((masks, page_size, max_entries));
        let start = cursor
            .map(|cursor| cursor.into_state::<usize>())
            .transpose()?
            .unwrap_or(0);
        let count = if max_entries == 0 {
            3
        } else {
            3.min(max_entries)
        };
        let end = start.saturating_add(page_size).min(count);
        let entries = (start..end)
            .map(|index| VfAttrs {
                file: FileOperand::from_os_path(&dir.join(format!("item-{index}"))),
                masks,
                mode: 0o100644,
                size: index as u64,
                ..VfAttrs::default()
            })
            .collect();
        Ok((entries, (end < count).then(|| DirPageCursor::new(end))))
    }
}

#[test]
fn minimal_backend_directory_defaults_return_unsupported() {
    let mut concrete = DefaultBackend::default();
    let backend: &mut dyn VectorBackend = &mut concrete;
    let dir = std::path::Path::new("/tree");
    assert_eq!(
        backend
            .read_dir_page_impl(dir, None, 1, 4)
            .err()
            .expect("paging is unsupported")
            .kind(),
        std::io::ErrorKind::Unsupported,
    );
    assert_eq!(
        backend
            .read_dir_page_with_fields_impl(dir, vfsi_sync::AttrMask::SIZE, None, 1, 4)
            .err()
            .expect("paging is unsupported")
            .kind(),
        std::io::ErrorKind::Unsupported,
    );
    let client = FsClient::new(concrete);
    assert_eq!(
        client
            .listdir(dir, vfsi_core::api::ListDirOptions::new(), |_| panic!(
                "unsupported listing must not invoke the visitor"
            ))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported,
    );
}

#[test]
fn default_directory_pages_preserve_cursor_fields_and_bounds() {
    let mut concrete = PagedBackend::default();
    let calls = concrete.page_calls.clone();
    let backend: &mut dyn VectorBackend = &mut concrete;
    let dir = std::path::Path::new("/tree");
    let (first, next) = backend.read_dir_page_impl(dir, None, 1, 3).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].path(), dir.join("item-0"));
    let (rest, next) = backend
        .read_dir_page_with_fields_impl(dir, vfsi_sync::AttrMask::SIZE, next, 2, 3)
        .unwrap();
    assert_eq!(
        rest.iter().map(|entry| entry.path()).collect::<Vec<_>>(),
        [dir.join("item-1"), dir.join("item-2")]
    );
    assert!(next.is_none());
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0]
            .0
            .contains(vfsi_sync::AttrMask::MODE | vfsi_sync::AttrMask::SIZE)
    );
    assert_eq!((calls[0].1, calls[0].2), (1, 3));
    assert_eq!(
        calls[1],
        (vfsi_sync::AttrMask::MODE | vfsi_sync::AttrMask::SIZE, 2, 3)
    );
}

#[test]
fn default_directory_visitor_uses_native_pages_without_materializing_a_listing() {
    // Only the native page engine is implemented; listdir_impl stays unsupported.
    let concrete = PagedBackend::default();
    let calls = concrete.page_calls.clone();
    let client = FsClient::new(concrete);
    let mut paths = Vec::new();
    let completion = client
        .listdir(
            "/tree",
            vfsi_sync::ListDirOptions::new()
                .with_fields(vfsi_sync::AttrMask::SIZE)
                .with_max_entries(3),
            |entry| {
                paths.push(entry.entry.path().to_path_buf());
                Ok(vfsi_core::api::WalkControl::Continue)
            },
        )
        .unwrap();
    assert_eq!(completion, vfsi_sync::TraversalCompletion::Complete);
    assert_eq!(
        paths,
        (0..3)
            .map(|i| format!("/tree/item-{i}").into())
            .collect::<Vec<std::path::PathBuf>>()
    );
    assert_eq!(
        *calls.lock().unwrap(),
        [
            (vfsi_sync::AttrMask::MODE | vfsi_sync::AttrMask::SIZE, 1, 4),
            (vfsi_sync::AttrMask::MODE | vfsi_sync::AttrMask::SIZE, 3, 3),
        ]
    );
}

#[derive(Default)]
struct WorkflowOverride {
    scalar: ScalarOnly,
    calls: Vec<&'static str>,
}
handle_contract!(WorkflowOverride);

impl VectorBackend for WorkflowOverride {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, _: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        panic!("specialized workflows must not fall back to generic vector reads")
    }

    fn vread_all_with_options_impl(
        &mut self,
        files: &[FileOperand],
        _: vfsi_sync::ReadAllOptions,
    ) -> vfsi_sync::VecResult<Vec<u8>> {
        self.calls.push("read_all");
        Ok(files.iter().map(|_| Ok(b"override".to_vec())).collect())
    }

    fn read_dir_page_with_fields_impl(
        &mut self,
        _: &std::path::Path,
        fields: vfsi_sync::AttrMask,
        _: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
        assert!(fields.contains(vfsi_sync::AttrMask::MODE | vfsi_sync::AttrMask::SIZE));
        assert_eq!((page_size, max_entries), (1, 4));
        self.calls.push("directory");
        Ok((Vec::new(), None))
    }

    fn vstream_impl(
        &mut self,
        _: &[FileOperand],
        _: usize,
        _: usize,
        callback: &mut vfsi_sync::ReadStreamCallback<'_>,
    ) -> vfsi_sync::VfRes {
        self.calls.push("stream");
        callback(0, 0, b"override", true);
        Ok(())
    }

    fn walk_with_options_impl(
        &mut self,
        _: &std::path::Path,
        _: vfsi_sync::AttrMask,
        _: vfsi_sync::ListDirOptions,
        _: &mut dyn FnMut(&std::path::Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<vfsi_sync::WalkEntry>> {
        self.calls.push("walk");
        Ok(Vec::new())
    }

    fn remove_paths_with_options_impl(
        &mut self,
        paths: &[&std::path::Path],
        _: vfsi_core::api::RemoveMode,
        _: vfsi_sync::RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        self.calls.push("remove");
        Ok(paths.iter().map(|_| Ok(())).collect())
    }

    fn vcopy_impl(
        &mut self,
        pairs: &[vfsi_sync::ExtentPair],
        _: vfsi_sync::CopyOptions,
    ) -> vfsi_core::VecResult<()> {
        self.calls.push("copy");
        Ok(pairs.iter().map(|_| Ok(())).collect())
    }
}

#[test]
fn shared_workflows_keep_dynamic_backend_overrides_reachable() {
    let mut concrete = WorkflowOverride::default();
    let backend: &mut dyn VectorBackend = &mut concrete;
    let files = [FileOperand::from_fd(1)];
    assert_eq!(
        backend
            .vread_all_impl(&files)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap(),
        [b"override".to_vec()]
    );
    let mut observed = Vec::new();
    backend
        .vstream_impl(&files, 16, 16, &mut |_, _, data, _| {
            observed.extend_from_slice(data);
            true
        })
        .unwrap();
    assert_eq!(observed, b"override");
    assert!(
        backend
            .walk_impl(
                std::path::Path::new("/tree"),
                vfsi_sync::AttrMask::default(),
                &mut |_, _| {},
            )
            .unwrap()
            .is_empty()
    );
    backend
        .remove_impl(std::path::Path::new("/tree"), true)
        .unwrap();
    {
        let copy_pairs = &[vfsi_sync::ExtentPair::from_os_paths(
            std::path::Path::new("/a"),
            0,
            std::path::Path::new("/b"),
            0,
            None,
        )];
        vfsi_core::internal::collect_vec_result(
            backend.vcopy_impl(copy_pairs, vfsi_sync::CopyOptions::default()),
            copy_pairs.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    let (entries, next) = backend
        .read_dir_page_impl(std::path::Path::new("/tree"), None, 1, 4)
        .unwrap();
    assert!(entries.is_empty());
    assert!(next.is_none());
    assert_eq!(
        concrete.calls,
        ["read_all", "stream", "walk", "remove", "copy", "directory"]
    );
}

impl VectorBackend for DefaultBackend {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, requests: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        let result: VfResult<Vec<ReadResult>> = {
            self.vector_reads += 1;
            requests
                .iter()
                .map(|request| self.read_impl(request))
                .collect()
        };
        result.map(|results| results.into_iter().map(Ok).collect())
    }
}

#[test]
fn minimal_backend_defaults_are_object_safe_bounded_and_terminate() {
    let mut concrete = DefaultBackend {
        scalar: ScalarOnly {
            data: b"abcdef".to_vec(),
            ..Default::default()
        },
        ..Default::default()
    };
    let backend: &mut dyn VectorBackend = &mut concrete;
    assert!(backend.vstatfs_impl(&[]).unwrap().is_empty());
    assert!(backend.take_notifications().is_empty());
    assert_eq!(backend.getcwd(), std::path::Path::new("/"));
    let path = std::path::Path::new("relative/file");
    assert_eq!(backend.abs_path(path), path);
    assert_eq!(
        backend.chdir(path).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        backend
            .seek_raw_impl(&FileOperand::from_fd(0), 0, vfsi_sync::SeekFrom::Set)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    let files = backend
        .vopen_impl(&[OpenOp::new("/file", OpenFlags::READ)])
        .unwrap()
        .into_iter()
        .map(|result| result.unwrap())
        .collect::<Vec<_>>();
    let data = backend
        .vread_all_with_options_impl(
            &files,
            vfsi_sync::ReadAllOptions::new().with_max_total_bytes(6),
        )
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(data, [b"abcdef".to_vec()]);
    assert_eq!(
        backend
            .vread_all_with_options_impl(
                &files,
                vfsi_sync::ReadAllOptions::new().with_max_total_bytes(3),
            )
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::FileTooLarge
    );
    let mut received = Vec::new();
    backend
        .vstream_impl(&files, 2, 4, &mut |index, offset, bytes, eof| {
            assert_eq!(index, 0);
            assert_eq!(offset as usize, received.len());
            assert!(bytes.len() <= 2);
            received.extend_from_slice(bytes);
            assert_eq!(eof, received.len() == 6);
            true
        })
        .unwrap();
    assert_eq!(received, b"abcdef");
    let mut buffer = [0; 3];
    let result = backend
        .vread_into_impl(&[ReadOp::at(files[0].clone(), 2, 3)], &mut [&mut buffer])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(buffer, *b"cde");
    assert_eq!(result[0].bytes_read, 3);
    let paths = [(
        FileOperand::from_path("/source"),
        FileOperand::from_path("/target"),
    )];
    assert_eq!(
        backend
            .rename_impl(
                std::path::Path::new("/source"),
                std::path::Path::new("/target")
            )
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        {
            let rename_pairs = &paths;
            vfsi_core::internal::collect_vec_result(
                backend
                    .vrename_with_options_impl(rename_pairs, vfsi_core::api::RenameMode::NoReplace),
                rename_pairs.len(),
            )
            .map(|_| ())
        }
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        backend
            .read_link_impl(std::path::Path::new("/link"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        backend
            .vwrite_impl(&[WriteOp::new(&files[0], VfOffset::At(0), b"x")])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    backend.sync_all(&files[0]).unwrap();
    backend.close_deferred(&files[0]).unwrap();
    let files = backend
        .vopen_impl(&[OpenOp::new("/file", OpenFlags::READ)])
        .unwrap()
        .into_iter()
        .map(|result| result.unwrap())
        .collect::<Vec<_>>();
    assert!(
        backend
            .vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert!(!concrete.scalar.open);
    assert_eq!(concrete.scalar.close_calls, 2);
    assert!(concrete.vector_reads >= 5);
}

impl HandleBackend for ScalarOnly {
    fn prepare_object_refs(&mut self, _: &[&FileOperand]) -> VfResult<()> {
        if self.identity_preflight_failure {
            Err(VfError::errno(None, libc::EACCES as u32))
        } else {
            Ok(())
        }
    }

    fn vclose_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        Ok(files
            .iter()
            .map(|file| {
                self.close_impl(file)
                    .map_err(|error| vfsi_core::OpError::unknown(error, None))
            })
            .collect())
    }

    fn open_object_ref(
        &self,
        file: &FileOperand,
    ) -> VfResult<Option<vfsi_sync::backend::OpenObjectRef<'_>>> {
        if self.identity_ref_failure {
            return Err(VfError::errno(None, libc::EACCES as u32));
        }
        Ok(
            if self.unknown_objects || self.unknown_object_fd == file.fd() {
                None
            } else {
                self.descriptor_objects.get(&file.fd().unwrap()).map(|id| {
                    vfsi_sync::backend::OpenObjectRef::Id(vfsi_sync::FsObjId::new(0, *id))
                })
            },
        )
    }

    fn vfsync_impl(
        &mut self,
        files: &[FileOperand],
        mode: vfsi_core::api::SyncMode,
    ) -> vfsi_sync::VecResult<()> {
        self.sync_calls.lock().unwrap().push((mode, files.to_vec()));
        let Some(error) = self.sync_failure.clone() else {
            return Ok((0..self.sync_result_count.unwrap_or(files.len()))
                .map(|_| Ok(()))
                .collect());
        };
        let Some(index) = error.index().filter(|index| *index < files.len()) else {
            return Err(error);
        };
        Ok(files
            .iter()
            .enumerate()
            .map(|(position, _)| {
                if position != index {
                    return Ok(());
                }
                Err(if error.is_transport() {
                    vfsi_sync::OpError::unknown(error.clone(), None)
                } else {
                    vfsi_sync::OpError::failed(error.clone(), None)
                })
            })
            .collect())
    }
    fn vsetattrs_impl(
        &mut self,
        updates: &[SetAttrsOp<Target<'_, FileOperand>>],
    ) -> vfsi_core::VecResult<()> {
        let mut calls = self.attrs_calls.lock().unwrap();
        calls.push((
            updates[0].follow_final_symlink(),
            updates.iter().map(|op| op.requested_len()).collect(),
        ));
        if let Some((call, error)) = &self.attrs_failure
            && calls.len() == *call
        {
            if let Some(index) = error.index().filter(|&index| index < updates.len()) {
                return Ok((0..updates.len())
                    .map(|position| {
                        if position == index {
                            Err(vfsi_core::OpError::failed(error.clone(), None))
                        } else {
                            Ok(())
                        }
                    })
                    .collect());
            }
            return Err(error.clone());
        }
        Ok(updates.iter().map(|_| Ok(())).collect())
    }
    fn vstatfs_impl(
        &mut self,
        files: &[FileOperand],
    ) -> vfsi_core::VecResult<vfsi_sync::FilesystemStats> {
        self.stats_calls.fetch_add(1, Ordering::SeqCst);
        let mut results = vec![
            Ok(vfsi_sync::FilesystemStats::default());
            self.stats_result_count.unwrap_or(files.len())
        ];
        if let Some(index) = self.stats_error_index {
            if let Some(slot) = results.get_mut(index) {
                *slot = Err(vfsi_core::OpError::failed(
                    VfError::errno(index, libc::ENOENT as u32),
                    None,
                ));
            } else {
                results.push(Err(vfsi_core::OpError::failed(
                    VfError::invalid_reply(None, "invalid injected statfs index"),
                    None,
                )));
            }
        }
        Ok(results)
    }

    fn take_notifications(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        std::mem::take(&mut *self.notifications.lock().unwrap())
    }
    fn open_impl(&mut self, request: &OpenOp) -> VfResult<FileOperand> {
        self.open_requests
            .lock()
            .unwrap()
            .push(request.clone().into_owned());
        self.open = true;
        let fd = self.descriptor_objects.len() as i32 + 1;
        let next = self.named_objects.len() as u64;
        let object = if self.alias_objects {
            0
        } else {
            *self
                .named_objects
                .entry(request.path().to_path_buf())
                .or_insert(next)
        };
        self.descriptor_objects.insert(fd, object);
        Ok(FileOperand::from_fd(fd))
    }

    fn close_impl(&mut self, _: &FileOperand) -> VfResult<()> {
        self.close_calls += 1;
        self.close_observed.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = self.blocked_close.take() {
            entered.wait();
            release.wait();
        }
        if self.close_failures_remaining != 0 {
            self.close_failures_remaining -= 1;
            return Err(VfError::transport(None, "injected close failure"));
        }
        self.open = false;
        Ok(())
    }
    fn sync_data(&mut self, _: &FileOperand) -> VfResult<()> {
        Ok(())
    }
    fn sync_all(&mut self, _: &FileOperand) -> VfResult<()> {
        Ok(())
    }

    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        if let Some((entered, release)) = self.blocked_read.take() {
            entered.wait();
            release.wait();
        }
        let call = self.read_calls.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(!self.direct_into_only, "owned read path must not run");
        if self.read_failure_call == Some(call) {
            return Err(VfError::errno(None, libc::EIO as u32));
        }
        if self.read_failure {
            return Err(VfError::errno(0, libc::EACCES as u32));
        }
        if self.oversized_read {
            return Ok(ReadResult {
                file: request.file.clone(),
                offset: 0,
                eof: false,
                data: vec![0; request.length + 1],
            });
        }
        let offset = match request.offset {
            VfOffset::At(value) => value,
            VfOffset::Cur => self.cursor,
            VfOffset::End => self.data.len() as u64,
            _ => 0,
        };
        let start = offset as usize;
        let end = start.saturating_add(request.length).min(self.data.len());
        let data = self.data.get(start..end).unwrap_or_default().to_vec();
        self.cursor = offset + data.len() as u64;
        Ok(ReadResult {
            file: request.file.clone(),
            offset,
            eof: end == self.data.len(),
            data,
        })
    }

    fn read_into_impl(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        if !self.direct_into_only {
            let result = self.read_impl(request)?;
            if result.data.len() > buffer.len() {
                return Err(VfError::errno(0, vfsi_sync::ERR_IO));
            }
            buffer[..result.data.len()].copy_from_slice(&result.data);
            return Ok(ReadIntoResult {
                file: result.file,
                offset: result.offset,
                bytes_read: result.data.len(),
                eof: result.eof,
            });
        }
        if request.length != buffer.len() {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        self.into_calls.fetch_add(1, Ordering::SeqCst);
        let offset = match request.offset {
            VfOffset::At(value) => value,
            VfOffset::Cur => self.cursor,
            VfOffset::End => self.data.len() as u64,
            _ => 0,
        };
        let start = offset as usize;
        let end = start.saturating_add(buffer.len()).min(self.data.len());
        let data = self.data.get(start..end).unwrap_or_default();
        buffer[..data.len()].copy_from_slice(data);
        self.cursor = offset + data.len() as u64;
        Ok(ReadIntoResult {
            file: request.file.clone(),
            offset,
            bytes_read: data.len(),
            eof: end == self.data.len(),
        })
    }

    fn write_impl(&mut self, request: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteResult> {
        let call = self.write_calls.fetch_add(1, Ordering::SeqCst);
        if self.interrupt_write_call == Some(call) {
            return Err(VfError::errno(0, libc::EINTR as u32));
        }
        if self.oversized_write_count {
            return Ok(WriteResult {
                file: request.file().clone(),
                offset: 0,
                bytes_written: request.data().len() + 1,
                stable: true,
            });
        }
        let offset = match request.offset() {
            VfOffset::At(value) => value,
            VfOffset::Cur => self.cursor,
            VfOffset::End => self.data.len() as u64,
            _ => 0,
        };
        let start = offset as usize;
        let length = request
            .data()
            .len()
            .min(self.max_write_once.unwrap_or(usize::MAX));
        self.data.resize(self.data.len().max(start + length), 0);
        self.data[start..start + length].copy_from_slice(&request.data()[..length]);
        self.cursor = offset + length as u64;
        Ok(WriteResult {
            file: request.file().clone(),
            offset,
            bytes_written: length,
            stable: true,
        })
    }

    fn seek_impl(&mut self, _: &FileOperand, position: SeekFrom) -> VfResult<u64> {
        let next = match position {
            SeekFrom::Start(value) => value as i128,
            SeekFrom::Current(value) => self.cursor as i128 + value as i128,
            SeekFrom::End(value) => self.data.len() as i128 + value as i128,
        };
        if next < 0 || next > u64::MAX as i128 {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        self.cursor = next as u64;
        Ok(self.cursor)
    }

    fn metadata_impl(
        &mut self,
        _: Target<'_, FileOperand>,
        _: vfsi_core::api::AttrsOptions,
    ) -> VfResult<vfsi_sync::VfAttrs> {
        Err(VfError::unsupported(0))
    }

    fn set_attributes_impl(&mut self, _: &SetAttrsOp<Target<'_, FileOperand>>) -> VfResult<()> {
        Err(VfError::unsupported(0))
    }
}

#[test]
fn setattrs_ops_batch_policy_runs_and_isolate_invalid_siblings() {
    use vfsi_core::SetAttrsOp;
    let backend = ScalarOnly::default();
    let calls = Arc::clone(&backend.attrs_calls);
    let fs = FsClient::new(backend);
    fs.vsetattrs::<&str>(&[]).unwrap();
    assert!(calls.lock().unwrap().is_empty());
    fs.setattrs_all(&[
        SetAttrsOp::new("/a").len(1),
        SetAttrsOp::new("/b").len(2),
        SetAttrsOp::new("/c")
            .len(3)
            .with_follow_final_symlink(false),
        SetAttrsOp::new("/d")
            .len(4)
            .with_follow_final_symlink(false),
        SetAttrsOp::new("/a").len(5),
    ])
    .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        [
            (true, vec![Some(1), Some(2)]),
            (false, vec![Some(3), Some(4)]),
            (true, vec![Some(5)]),
        ]
    );
    calls.lock().unwrap().clear();
    let error = fs
        .setattrs_all(&[
            SetAttrsOp::new("/a").len(6),
            SetAttrsOp::new("/b")
                .uid(u32::MAX)
                .with_follow_final_symlink(false),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.raw_code(), libc::EINVAL as u32);
    assert_eq!(*calls.lock().unwrap(), [(true, vec![Some(6)])]);
}

#[test]
fn setattrs_ops_remap_later_run_failures_without_replaying_successes() {
    use vfsi_core::SetAttrsOp;
    for failure in [
        VfError::errno(1, libc::ENOENT as u32),
        VfError::errno(2, libc::ENOENT as u32),
        VfError::transport(None, "lost attribute reply"),
    ] {
        let backend = ScalarOnly {
            attrs_failure: Some((2, failure.clone())),
            ..Default::default()
        };
        let calls = Arc::clone(&backend.attrs_calls);
        let fs = FsClient::new(backend);
        let error = fs
            .setattrs_all(&[
                SetAttrsOp::new("/a").len(1),
                SetAttrsOp::new("/b")
                    .len(2)
                    .with_follow_final_symlink(false),
                SetAttrsOp::new("/c")
                    .len(3)
                    .with_follow_final_symlink(false),
                SetAttrsOp::new("/d").len(4),
            ])
            .unwrap_err();
        assert_eq!(calls.lock().unwrap().len(), 3);
        if failure.index() == Some(1) {
            assert_eq!(error.index(), Some(2));
            assert_eq!(error.raw_code(), libc::ENOENT as u32);
            assert_eq!(error.path(), Some(std::path::Path::new("/c")));
        } else {
            assert!(error.is_transport());
            assert_eq!(error.index(), Some(1));
        }
    }
}

#[test]
fn owned_handle_keeps_backend_alive_after_standard_io_adapter_is_dropped() {
    let backend = ScalarOnly::default();
    let closes = Arc::clone(&backend.close_observed);
    let client = FsClient::new(backend);
    let file = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .open("/file")
        .unwrap();
    {
        let mut io = client.std_io(&file);
        io.write_all(b"vector").unwrap();
        io.seek(SeekFrom::Start(0)).unwrap();
        let mut output = String::new();
        io.read_to_string(&mut output).unwrap();
        assert_eq!(output, "vector");
    }
    assert!(!file.is_closed());
    drop(client);
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    file.close().unwrap();
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}

#[test]
fn std_io_rejects_backend_results_that_violate_io_contracts() {
    let client = FsClient::new(ScalarOnly {
        oversized_read: true,
        ..Default::default()
    });
    let file = client.open("/file").unwrap();
    let error = client.std_io(&file).read(&mut [0; 4]).unwrap_err();
    let error = error.get_ref().unwrap().downcast_ref::<VfError>().unwrap();
    assert_eq!(error.raw_code(), vfsi_sync::ERR_IO);

    let client = FsClient::new(ScalarOnly {
        oversized_write_count: true,
        ..Default::default()
    });
    let file = client
        .open_with(OpenOp::new("/file", OpenFlags::WRITE))
        .unwrap();
    let error = client.std_io(&file).write(b"data").unwrap_err();
    let outcome = error
        .get_ref()
        .unwrap()
        .downcast_ref::<vfsi_sync::OpError>()
        .unwrap();
    assert_eq!(outcome.state(), vfsi_sync::FailureState::Unknown);
    let error = outcome.cause();
    assert!(error.is_transport());
    assert_eq!(error.operation(), Some("vwrite_native"));
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
}

#[test]
fn std_io_errors_retain_vector_operation_and_path_context() {
    let client = FsClient::new(ScalarOnly {
        read_failure: true,
        ..Default::default()
    });
    let file = client.open("/important").unwrap();
    let error = client.std_io(&file).read(&mut [0; 1]).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let error = error.get_ref().unwrap().downcast_ref::<VfError>().unwrap();
    assert_eq!(error.operation(), Some("vread_into_native"));
    assert_eq!(error.path(), Some(std::path::Path::new("/important")));
}

#[test]
fn convenience_string_errors_retain_operation_and_path_context() {
    let client = FsClient::new(ScalarOnly {
        data: vec![0xff],
        ..ScalarOnly::default()
    });
    let error = client.read_to_string("/not-utf8").unwrap_err();
    assert_eq!(error.raw_code(), vfsi_sync::ERR_INVAL);
    assert_eq!(error.operation(), Some("read_to_string"));
    assert_eq!(error.path(), Some(std::path::Path::new("/not-utf8")));
}

#[test]
fn allocating_whole_file_reads_enforce_a_configurable_limit() {
    let client = FsClient::new(ScalarOnly {
        data: b"four".to_vec(),
        ..ScalarOnly::default()
    });
    let error = client
        .read_with_options(
            "/file",
            ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(3).unwrap()),
        )
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    assert_eq!(error.operation(), Some("read"));
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));

    let client = FsClient::new(ScalarOnly {
        data: b"four".to_vec(),
        ..ScalarOnly::default()
    });
    assert_eq!(
        client
            .read_with_options(
                "/file",
                ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(4).unwrap())
            )
            .unwrap(),
        b"four"
    );

    let client = FsClient::new(ScalarOnly {
        data: b"utf8".to_vec(),
        ..ScalarOnly::default()
    });
    assert_eq!(
        client
            .read_to_string_with_options(
                "/file",
                ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(4).unwrap())
            )
            .unwrap(),
        "utf8"
    );
}

#[test]
fn vector_transport_failure_does_not_invent_request_zero_context() {
    let client = FsClient::new(ScalarOnly {
        transport_failure: true,
        ..ScalarOnly::default()
    });
    let error = client
        .open_batch(&[
            OpenOp::new("/first", OpenFlags::READ),
            OpenOp::new("/second", OpenFlags::READ),
        ])
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
    assert_eq!(error.path(), None);
}

#[test]
fn openv_rejects_wrong_result_count_and_cleans_returned_handles() {
    let client = FsClient::new(ScalarOnly {
        vector_result_limit: Arc::new(Mutex::new(Some(1))),
        ..ScalarOnly::default()
    });
    let error = client
        .open_batch(&[
            OpenOp::new("/first", OpenFlags::READ),
            OpenOp::new("/second", OpenFlags::READ),
        ])
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(1));
    assert!(error.to_string().contains("missing open outcome"));
    assert!(!client.into_inner().unwrap().open);
}

#[test]
fn readv_and_writev_reject_wrong_result_counts() {
    let backend = ScalarOnly::default();
    let vector_result_limit = Arc::clone(&backend.vector_result_limit);
    let client = FsClient::new(backend);
    let files = client
        .open_batch(&[
            OpenOp::new("/first", OpenFlags::READ | OpenFlags::WRITE),
            OpenOp::new("/second", OpenFlags::READ | OpenFlags::WRITE),
        ])
        .unwrap();
    *vector_result_limit.lock().unwrap() = Some(1);

    let read_error = client
        .vread_native(&[
            files[0].read_request_at(0, 1),
            files[1].read_request_at(0, 1),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert!(read_error.is_transport());
    assert_eq!(read_error.index(), Some(1));

    let write_error = client
        .vwrite_native(&[
            files[0].write_request_at(0, b"a"),
            files[1].write_request_at(0, b"b"),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert!(write_error.is_transport());
    assert_eq!(write_error.index(), Some(1));

    client.vclose_owned(files).unwrap();
}

#[test]
fn owned_vector_reads_reject_oversized_batches_before_backend_io() {
    let read_calls = Arc::new(AtomicUsize::new(0));
    let client = FsClient::new(ScalarOnly {
        data: b"abcdef".to_vec(),
        read_calls: Arc::clone(&read_calls),
        ..ScalarOnly::default()
    });
    let file = client.open("/file").unwrap();
    let error = client
        .vread_with_limit_native(&[file.read_request_at(0, 3), file.read_request_at(3, 3)], 5)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
    assert_eq!(read_calls.load(Ordering::SeqCst), 0);

    let error = client
        .vread_native(&[file.read_request_at(0, vfsi_sync::DEFAULT_READV_MAX_TOTAL_BYTES + 1)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    assert_eq!(read_calls.load(Ordering::SeqCst), 0);

    let results = client
        .vread_with_limit_native(&[file.read_request_at(0, 3)], 3)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(results[0].data, b"abc");
}

#[test]
fn write_allv_dependency_schedule_visits_requests_only_when_ready() {
    let backend = WriteBoundaryProbe::default();
    let calls = Arc::clone(&backend.calls);
    let client = FsClient::new(backend);
    let file = client.open("/same").unwrap();
    let payloads: Vec<_> = (0..128).map(|index| [index as u8]).collect();
    let requests: Vec<_> = payloads
        .iter()
        .map(|payload| file.write_request_at(0, payload))
        .collect();
    let projections = std::cell::Cell::new(0usize);
    let results = client
        .vwrite_all_mapped_native(&requests, |request| {
            projections.set(projections.get() + 1);
            *request
        })
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert!(results.iter().all(|result| result.bytes_written == 1));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), requests.len());
    for (index, wave) in calls.iter().enumerate() {
        assert_eq!(
            wave.len(),
            1,
            "conflicting requests complete in input order"
        );
        assert_eq!(wave[0].pointer, payloads[index].as_ptr() as usize);
        assert_eq!(wave[0].offset, VfOffset::At(0));
    }
    println!(
        "128 conflicting writes: {} request projections",
        projections.get()
    );
    assert!(
        projections.get() <= 8 * requests.len(),
        "blocked requests must not be projected again in every readiness scan: {}",
        projections.get()
    );
}

#[test]
fn write_allv_dependency_schedule_orders_newly_ready_and_partial_requests() {
    let backend = WriteBoundaryProbe::default();
    let calls = Arc::clone(&backend.calls);
    let client = FsClient::new(backend);
    let a = client.open("/a").unwrap();
    let b = client.open("/b").unwrap();
    let payloads: [&[u8]; 4] = [b"AA", b"BBBB", b"CCCCCC", b"dddddd"];
    let requests = [
        a.write_request_at(0, payloads[0]),
        a.write_request_at(4, payloads[1]),
        b.write_request_at(10, payloads[2]),
        a.write_request_at(1, payloads[3]),
    ];
    let results = client
        .vwrite_all_native(&requests)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.bytes_written)
            .collect::<Vec<_>>(),
        [2, 4, 6, 6]
    );
    assert!(results.iter().all(|result| result.stable));
    // Request 3 waits for both overlapping predecessors, including every
    // short-write retry of request 1. It then joins request 2's final retry.
    let expected: &[&[(usize, usize, u64)]] = &[
        &[(0, 0, 0), (1, 0, 4), (2, 0, 10)],
        &[(1, 2, 6), (2, 2, 12)],
        &[(2, 4, 14), (3, 0, 1)],
        &[(3, 2, 3)],
        &[(3, 4, 5)],
    ];
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), expected.len());
    for (wave, expected) in calls.iter().zip(expected) {
        assert_eq!(wave.len(), expected.len());
        for (op, &(index, start, offset)) in wave.iter().zip(*expected) {
            assert_eq!(op.pointer, payloads[index][start..].as_ptr() as usize);
            assert_eq!(op.length, payloads[index].len() - start);
            assert_eq!(op.offset, VfOffset::At(offset));
        }
    }
}

#[test]
fn write_allv_dependency_schedule_treats_validated_empty_requests_as_complete() {
    let backend = WriteBoundaryProbe::default();
    let calls = Arc::clone(&backend.calls);
    let client = FsClient::new(backend);
    let file = client.open("/same").unwrap();
    let append = client.open_options().append(true).open("/same").unwrap();
    let data = b"AAAA";
    let later = b"BB";
    let independent = b"CC";
    let results = client
        .vwrite_all_native(&[
            append.write_request_at(u64::MAX, b""),
            file.write_request_at(0, data),
            file.write_request_at(2, b""),
            file.write_request_at(0, later),
            file.write_request_at(10, b""),
            file.write_request_at(20, independent),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| (result.offset, result.bytes_written))
            .collect::<Vec<_>>(),
        [(u64::MAX, 0), (0, 4), (2, 0), (0, 2), (10, 0), (20, 2)]
    );
    assert!(results.iter().all(|result| result.stable));
    let observations = calls.lock().unwrap();
    assert_eq!(
        observations.iter().map(Vec::len).collect::<Vec<_>>(),
        [2, 1, 1]
    );
    assert_eq!(observations[0][0].pointer, data.as_ptr() as usize);
    assert_eq!(observations[0][1].pointer, independent.as_ptr() as usize);
    assert_eq!(observations[1][0].pointer, data[2..].as_ptr() as usize);
    assert_eq!(observations[2][0].pointer, later.as_ptr() as usize);
    drop(observations);
    let results = client
        .vwrite_all_native(&[
            file.write_request_at(3, b""),
            append.write_request_at(7, b""),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.bytes_written)
            .collect::<Vec<_>>(),
        [0, 0]
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "empty batches perform no writes"
    );
}

#[test]
fn write_allv_dependency_schedule_matches_reference_waves() {
    let mut state = 0x4d59_5df4_d0f3_3173u64;
    for case in 0..64 {
        let count = [8, 17, 64, 320][case % 4];
        let specifications: Vec<_> = (0..count)
            .map(|index| {
                if case == 63 {
                    // One sparse overlapping object crosses the 256 cutoff
                    // while every request still needs two short-write waves.
                    return (0, false, index as u64 * 3, 4);
                }
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (
                    (state & 1) as usize,
                    state & 4 != 0,
                    (state >> 8) % 12,
                    ((state >> 16) % 7) as usize,
                )
            })
            .collect();
        let payloads: Vec<_> = specifications
            .iter()
            .map(|&(_, _, _, length)| vec![case as u8; length])
            .collect();
        // Reference the previous readiness rule, including completed empty
        // requests. Batches across the indexing threshold cover paths, intervals,
        // append handles, empty payloads, and multiple short-write waves.
        let mut totals = vec![0usize; count];
        let mut expected = Vec::new();
        while specifications
            .iter()
            .enumerate()
            .any(|(index, &(_, _, _, length))| totals[index] < length)
        {
            let wave: Vec<_> = specifications
                .iter()
                .enumerate()
                .filter_map(|(index, &(path, append, offset, length))| {
                    if totals[index] == length {
                        return None;
                    }
                    let ready = specifications[..index].iter().enumerate().all(
                        |(earlier, &(prior_path, prior_append, start, prior_length))| {
                            prior_path != path
                                || !(append
                                    || prior_append
                                    || (offset < start + prior_length as u64
                                        && start < offset + length as u64))
                                || totals[earlier] == prior_length
                        },
                    );
                    ready.then_some(index)
                })
                .collect();
            assert!(!wave.is_empty());
            expected.push(
                wave.iter()
                    .map(|&index| {
                        (
                            payloads[index][totals[index]..].as_ptr() as usize,
                            payloads[index].len() - totals[index],
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            for index in wave {
                totals[index] += (payloads[index].len() - totals[index]).min(2);
            }
        }
        let backend = WriteBoundaryProbe {
            append_end: Some(0),
            ..Default::default()
        };
        let calls = Arc::clone(&backend.calls);
        let client = FsClient::new(backend);
        let files = [client.open("/a").unwrap(), client.open("/b").unwrap()];
        let appends = [
            client.open_options().append(true).open("/a").unwrap(),
            client.open_options().append(true).open("/b").unwrap(),
        ];
        let requests: Vec<_> = specifications
            .iter()
            .enumerate()
            .map(|(index, &(path, append, offset, _))| {
                let file = if append { &appends[path] } else { &files[path] };
                file.write_request_at(offset, &payloads[index])
            })
            .collect();
        let results = client
            .vwrite_all_native(&requests)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        for (result, &(_, append, offset, length)) in results.iter().zip(&specifications) {
            assert_eq!(result.bytes_written, length);
            if !append || length == 0 {
                assert_eq!(result.offset, offset);
            }
        }
        let actual: Vec<Vec<_>> = calls
            .lock()
            .unwrap()
            .iter()
            .map(|wave| wave.iter().map(|op| (op.pointer, op.length)).collect())
            .collect();
        assert_eq!(actual, expected, "case {case}: {specifications:?}");
    }
}

#[test]
fn write_allv_whole_wave_rejection_preserves_prior_progress_without_replay() {
    for error in [
        VfError::errno(0, libc::ENOSPC as u32),
        VfError::errno(1, libc::ENOSPC as u32),
        VfError::transport(None, "lost write reply"),
    ] {
        let backend = WriteBoundaryProbe {
            failure: Some(error.clone()),
            failure_after: 1,
            ..Default::default()
        };
        let calls = Arc::clone(&backend.calls);
        let client = FsClient::new(backend);
        let a = client.open("/a").unwrap();
        let b = client.open("/b").unwrap();
        let payloads: [&[u8]; 4] = [b"AA", b"BB", b"CCCC", b"DD"];
        let results = client
            .vwrite_all_native(&[
                a.write_request_at(0, payloads[0]),
                a.write_request_at(0, payloads[1]),
                b.write_request_at(0, payloads[2]),
                a.write_request_at(0, payloads[3]),
            ])
            .unwrap();
        assert_eq!(results[0].as_ref().unwrap().bytes_written, 2);
        for (index, state) in [
            (1, vfsi_sync::FailureState::NotAttempted),
            (2, vfsi_sync::FailureState::Failed),
            (3, vfsi_sync::FailureState::NotAttempted),
        ] {
            let actual = results[index].as_ref().unwrap_err();
            assert_eq!(actual.state(), state);
            assert_eq!(actual.cause().is_transport(), error.is_transport());
            assert_eq!(actual.cause().raw_code(), error.raw_code());
            assert_eq!(actual.cause().index(), None);
        }
        assert_eq!(
            results[2].as_ref().unwrap_err().progress(),
            Some(&vfsi_sync::IoProgress::Write { bytes_written: 2 })
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "a failed wave must not be replayed");
        assert_eq!(calls.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2]);
        assert_eq!(calls[0][0].pointer, payloads[0].as_ptr() as usize);
        assert_eq!(calls[0][1].pointer, payloads[2].as_ptr() as usize);
        assert_eq!(calls[1][0].pointer, payloads[1].as_ptr() as usize);
        assert_eq!(calls[1][1].pointer, payloads[2][2..].as_ptr() as usize);
    }
}

#[test]
fn write_allv_retries_short_writes_in_vector_waves() {
    let write_calls = Arc::new(AtomicUsize::new(0));
    let client = FsClient::new(ScalarOnly {
        max_write_once: Some(2),
        write_calls: Arc::clone(&write_calls),
        ..ScalarOnly::default()
    });
    let file = client
        .open_with(OpenOp::new("/file", OpenFlags::WRITE))
        .unwrap();
    let results = client
        .vwrite_all_native(&[
            file.write_request_at(0, b"abcde"),
            file.write_request_at(10, b"VWXYZ"),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.bytes_written)
            .collect::<Vec<_>>(),
        [5, 5]
    );
    assert!(results.iter().all(|result| result.stable));
    assert_eq!(write_calls.load(Ordering::SeqCst), 6);
    drop(file);
    let backend = client.into_inner().unwrap();
    assert_eq!(&backend.data[..5], b"abcde");
    assert_eq!(&backend.data[10..15], b"VWXYZ");
}

#[test]
fn write_allv_preserves_order_for_overlapping_short_writes() {
    let client = FsClient::new(ScalarOnly {
        max_write_once: Some(2),
        ..ScalarOnly::default()
    });
    let file = client
        .open_with(OpenOp::new("/file", OpenFlags::WRITE))
        .unwrap();
    let results = client
        .vwrite_all_native(&[
            file.write_request_at(0, b"AAAA"),
            file.write_request_at(2, b"BBBB"),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.bytes_written)
            .collect::<Vec<_>>(),
        [4, 4]
    );
    drop(file);
    assert_eq!(client.into_inner().unwrap().data, b"AABBBB");
}

#[test]
fn write_allv_waits_for_every_earlier_overlapping_request() {
    let client = FsClient::new(ScalarOnly {
        max_write_once: Some(2),
        ..ScalarOnly::default()
    });
    let file = client
        .open_with(OpenOp::new("/file", OpenFlags::WRITE))
        .unwrap();
    client
        .vwrite_all_native(&[
            file.write_request_at(0, b"AAAAAA"),
            file.write_request_at(6, b"BB"),
            file.write_request_at(4, b"CCCC"),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    drop(file);
    assert_eq!(client.into_inner().unwrap().data, b"AAAACCCC");
}

#[test]
fn write_allv_rejects_zero_progress() {
    let client = FsClient::new(ScalarOnly {
        max_write_once: Some(0),
        ..ScalarOnly::default()
    });
    let file = client
        .open_with(OpenOp::new("/file", OpenFlags::WRITE))
        .unwrap();
    let error = client
        .vwrite_all_native(&[file.write_request_at(0, b"data")])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(error.raw_code(), vfsi_sync::ERR_IO);
    assert_eq!(error.index(), Some(0));
}

#[test]
fn strict_write_all_collection_does_not_suppress_a_healthy_sibling() {
    let write_calls = Arc::new(AtomicUsize::new(0));
    let client = FsClient::new(ScalarOnly {
        write_calls: Arc::clone(&write_calls),
        ..ScalarOnly::default()
    });
    let other = FsClient::new(ScalarOnly::default());
    let local_file = client
        .open_with(OpenOp::new("/local", OpenFlags::WRITE))
        .unwrap();
    let foreign_file = other
        .open_with(OpenOp::new("/foreign", OpenFlags::WRITE))
        .unwrap();
    let error = client
        .vwrite_all_native(&[
            local_file.write_request_at(0, b"would-write"),
            foreign_file.write_request_at(0, b"invalid"),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(write_calls.load(Ordering::SeqCst), 1);
    drop(local_file);
    assert_eq!(client.into_inner().unwrap().data, b"would-write");
}

#[test]
fn vector_results_must_match_their_requests_and_io_limits() {
    for backend in [
        ScalarOnly {
            wrong_result_file: true,
            ..ScalarOnly::default()
        },
        ScalarOnly {
            wrong_result_offset: true,
            ..ScalarOnly::default()
        },
        ScalarOnly {
            oversized_read: true,
            ..ScalarOnly::default()
        },
    ] {
        let client = FsClient::new(backend);
        let file = client.open("/file").unwrap();
        let error = client
            .vread_native(&[file.read_request_at(0, 1)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.index(), Some(0));
        assert_eq!(error.operation(), Some("vread_native"));
        drop(file);
    }

    let client = FsClient::new(ScalarOnly {
        oversized_write_count: true,
        ..ScalarOnly::default()
    });
    let file = client
        .open_with(OpenOp::new("/file", OpenFlags::WRITE))
        .unwrap();
    let error = client
        .vwrite_native(&[file.write_request_at(0, b"x")])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.operation(), Some("vwrite_native"));
}

#[test]
fn explicit_close_failure_keeps_handle_armed_for_drop_cleanup() {
    let client = FsClient::new(ScalarOnly {
        close_failures_remaining: 1,
        ..ScalarOnly::default()
    });
    let file = client.open("/file").unwrap();
    assert!(file.close().is_err());

    let backend = client.into_inner().expect("drop released the client");
    assert_eq!(backend.close_calls, 2);
    assert!(!backend.open);
}

#[test]
fn failed_try_close_retains_handle_for_explicit_retry() {
    let client = FsClient::new(ScalarOnly {
        close_failures_remaining: 1,
        ..ScalarOnly::default()
    });
    let mut file = client.open("/file").unwrap();
    assert!(file.try_close().is_err());
    assert!(file.try_close().is_ok());
    assert!(file.try_close().is_ok());
    drop(file);

    let backend = client.into_inner().expect("drop released the client");
    assert_eq!(backend.close_calls, 2);
    assert!(!backend.open);
}

#[test]
fn failed_try_closev_retains_handles_for_explicit_retry() {
    let client = FsClient::new(ScalarOnly {
        close_failures_remaining: 1,
        ..ScalarOnly::default()
    });
    let mut files = vec![client.open("/file").unwrap()];
    assert!(client.close_batch(&mut files).is_err());
    assert_eq!(files[0].path(), std::path::Path::new("/file"));
    client.close_batch(&mut files).unwrap();
    drop(files);
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.close_calls, 2);
    assert!(!backend.open);
}

#[test]
fn successfully_closed_handle_returns_ebadf_instead_of_panicking() {
    let client = FsClient::new(ScalarOnly::default());
    let mut file = client.open("/file").unwrap();
    file.try_close().unwrap();
    for error in [
        client.std_io(&file).read(&mut [0u8; 1]).unwrap_err(),
        client.std_io(&file).write(b"x").unwrap_err(),
    ] {
        let source = error.get_ref().unwrap();
        let error = source
            .downcast_ref::<VfError>()
            .or_else(|| {
                source
                    .downcast_ref::<vfsi_sync::OpError>()
                    .map(vfsi_sync::OpError::cause)
            })
            .unwrap();
        assert_eq!(error.raw_code(), vfsi_sync::ERR_EBADF);
    }
}

#[test]
fn stream_callback_can_reenter_client_and_drop_another_file() {
    #[cfg(feature = "test-support")]
    if vfsi_sync::test_support::supervise_with_deadline(
        "stream_callback_can_reenter_client_and_drop_another_file",
    ) {
        return;
    }
    let client = FsClient::new(ScalarOnly {
        data: b"abcdef".to_vec(),
        ..ScalarOnly::default()
    });
    let mut other = Some(client.open("/other").unwrap());
    let mut received = Vec::new();
    client
        .read_stream_with_options(
            "/file",
            vfsi_sync::StreamOptions::new()
                .with_chunk_size(std::num::NonZeroUsize::new(2).unwrap()),
            |_, data| {
                drop(other.take());
                let file = client.open("/nested")?;
                file.close()?;
                received.extend_from_slice(data);
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap();
    assert_eq!(received, b"abcdef");
}

#[test]
fn stream_callback_errors_keep_nested_operation_indices_and_close_the_stream() {
    let backend = ScalarOnly {
        data: b"payload".to_vec(),
        ..Default::default()
    };
    let reads = Arc::clone(&backend.read_calls);
    let closes = Arc::clone(&backend.close_observed);
    let client = FsClient::new(backend);
    let live = client.open("/nested-live").unwrap();
    let mut closed = client.open("/nested-closed").unwrap();
    closed.try_close().unwrap();
    let nested = [
        live.read_request_at(0, 1),
        live.read_request_at(1, 1),
        closed.read_request_at(0, 1),
    ];
    let expected = client
        .vread_native(&nested)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(expected.index(), Some(2));
    assert_eq!(
        expected.path(),
        Some(std::path::Path::new("/nested-closed"))
    );
    let closes_before = closes.load(Ordering::SeqCst);
    let mut seen = Vec::new();
    let actual = client
        .vstream(
            &["/first", "/second", "/never-started"],
            vfsi_sync::StreamOptions::new(),
            |index, _, _| {
                seen.push(index);
                if index == 1 {
                    client.vread_native(&nested).and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })?;
                }
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap();
    assert_eq!(actual.len(), 3);
    assert_eq!(
        actual[0].as_ref().unwrap(),
        &vfsi_sync::StreamCompletion::Complete
    );
    assert_eq!(
        actual[2].as_ref().unwrap(),
        &vfsi_sync::StreamCompletion::Complete
    );
    let actual = actual.into_iter().nth(1).unwrap().unwrap_err();
    assert_eq!(actual.cause().index(), None);
    assert_eq!(actual.into_cause().with_index(2), expected);
    assert_eq!(seen, [0, 1, 2]);
    // One bounded chunk may be prefetched for later files, but their
    // healthy sibling callbacks still run after the nested error; every handle closes.
    // Each nested vector executes its two valid siblings despite the closed handle.
    assert_eq!(reads.load(Ordering::SeqCst), 7);
    assert_eq!(closes.load(Ordering::SeqCst) - closes_before, 3);
}

#[derive(Default)]
struct StreamBackend {
    scalar: ScalarOnly,
    batches: Vec<(&'static str, usize)>,
    open_error: Option<VfError>,
    read_error: Option<VfError>,
    read_error_prefix: bool,
    close_error: Option<VfError>,
}
handle_contract!(
    StreamBackend,
    fn vclose_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        self.batches.push(("close", files.len()));
        if let Some(error) = &self.close_error {
            let Some(failed) = error.index() else {
                return Err(error.clone());
            };
            return Ok(files
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    if index == failed {
                        Err(vfsi_core::OpError::unknown(error.clone(), None))
                    } else {
                        self.scalar
                            .close_impl(file)
                            .map_err(|cause| vfsi_core::OpError::unknown(cause, None))
                    }
                })
                .collect());
        }
        self.scalar.vclose_impl(files)
    }
);
impl VectorBackend for StreamBackend {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vopen_impl(&mut self, requests: &[OpenOp]) -> vfsi_core::VecResult<FileOperand> {
        self.batches.push(("open", requests.len()));
        if let Some(error) = &self.open_error {
            let Some(failed) = error.index() else {
                return Err(error.clone());
            };
            if failed < requests.len() {
                return Ok(requests
                    .iter()
                    .enumerate()
                    .map(|(index, request)| {
                        if index == failed {
                            Err(if error.is_transport() {
                                vfsi_core::OpError::unknown(error.clone(), None)
                            } else {
                                vfsi_core::OpError::failed(error.clone(), None)
                            })
                        } else {
                            self.scalar
                                .open_impl(request)
                                .map_err(|cause| vfsi_core::OpError::failed(cause, None))
                        }
                    })
                    .collect());
            }
        }
        self.scalar.vopen_impl(requests)
    }
    fn vread_impl(&mut self, requests: &[ReadOp]) -> vfsi_sync::VecResult<ReadResult> {
        self.batches.push(("read", requests.len()));
        if let Some(error) = &self.read_error
            && error.index().is_none()
        {
            return Ok(requests
                .iter()
                .map(|_| Err(vfsi_sync::OpError::unknown(error.clone(), None)))
                .collect());
        }
        let mut outcomes = self.scalar.vread_impl(requests)?;
        if let Some(error) = &self.read_error
            && let Some(index) = error.index().filter(|index| *index < outcomes.len())
        {
            let progress = if self.read_error_prefix {
                let previous = std::mem::replace(
                    &mut outcomes[index],
                    Err(vfsi_sync::OpError::failed(VfError::unsupported(None), None)),
                );
                let mut read = previous.unwrap();
                read.data.truncate(1);
                Some(vfsi_core::IoProgress::Read(
                    vfsi_core::api::ReadResult::owned(read.offset, read.data, false),
                ))
            } else {
                None
            };
            outcomes[index] = Err(if error.is_transport() {
                vfsi_sync::OpError::unknown(error.clone(), progress)
            } else {
                vfsi_sync::OpError::failed(error.clone(), progress)
            });
        }
        Ok(outcomes)
    }
}

#[test]
fn stream_cohorts_batch_small_files_with_bounded_memory_and_ordered_callbacks() {
    let client = FsClient::new(StreamBackend {
        scalar: ScalarOnly {
            data: b"data".to_vec(),
            ..Default::default()
        },
        ..Default::default()
    })
    .with_limits(vfsi_sync::ResourceLimits::new().with_max_read_bytes(16));
    let mut seen = Vec::new();
    let result = client
        .streams_all(
            &["/a", "/b", "/c", "/d", "/e"],
            vfsi_sync::StreamOptions::new()
                .with_chunk_size(std::num::NonZeroUsize::new(8).unwrap()),
            |index, offset, bytes| {
                // Reentry proves callbacks are outside the backend mutex.
                assert_eq!(client.limits().max_read_bytes(), 16);
                client.capabilities()?;
                seen.push((index, offset, bytes.to_vec()));
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap();
    assert_eq!(result, vec![vfsi_sync::StreamCompletion::Complete; 5]);
    assert_eq!(
        seen,
        (0..5)
            .map(|index| (index, 0, b"data".to_vec()))
            .collect::<Vec<_>>()
    );
    let backend = client.into_inner().unwrap();
    assert_eq!(
        backend.batches,
        [
            ("open", 2),
            ("read", 2),
            ("close", 2),
            ("open", 2),
            ("read", 2),
            ("close", 2),
            ("open", 1),
            ("read", 1),
            ("close", 1)
        ]
    );
    assert_eq!(backend.scalar.close_calls, 5);
}

#[test]
fn stream_cohorts_keep_known_failures_and_independent_siblings_on_stop_or_error() {
    for at_open in [false, true] {
        for stop in [false, true] {
            let error = VfError::errno(1, libc::ENOENT as u32);
            let client = FsClient::new(StreamBackend {
                scalar: ScalarOnly {
                    data: b"abcde".to_vec(),
                    ..Default::default()
                },
                open_error: at_open.then(|| error.clone()),
                read_error: (!at_open).then(|| error.clone()),
                ..Default::default()
            });
            let mut seen = Vec::new();
            let result = client.vstream(
                &["/first", "/bad", "/never"],
                vfsi_sync::StreamOptions::new()
                    .with_chunk_size(std::num::NonZeroUsize::new(2).unwrap()),
                |index, offset, bytes| {
                    seen.push((index, offset, bytes.to_vec()));
                    Ok(if stop {
                        std::ops::ControlFlow::Break(())
                    } else {
                        std::ops::ControlFlow::Continue(())
                    })
                },
            );
            let outcomes = result.unwrap();
            assert_eq!(outcomes.len(), 3);
            let failed = outcomes[1].as_ref().unwrap_err();
            assert_eq!(failed.state(), vfsi_core::FailureState::Failed);
            assert_eq!(failed.cause().index(), None);
            assert_eq!(failed.cause().raw_code(), libc::ENOENT as u32);
            if stop {
                assert_eq!(
                    outcomes[0].as_ref().unwrap(),
                    &vfsi_sync::StreamCompletion::Stopped { next_offset: 2 }
                );
                assert_eq!(
                    outcomes[2].as_ref().unwrap(),
                    &vfsi_sync::StreamCompletion::Stopped { next_offset: 0 }
                );
                assert_eq!(seen, [(0, 0, b"ab".to_vec())]);
            } else {
                assert_eq!(
                    outcomes[0].as_ref().unwrap(),
                    &vfsi_sync::StreamCompletion::Complete
                );
                assert_eq!(
                    outcomes[2].as_ref().unwrap(),
                    &vfsi_sync::StreamCompletion::Complete
                );
                assert_eq!(
                    seen,
                    [0, 2]
                        .into_iter()
                        .flat_map(|index| [
                            (index, 0, b"ab".to_vec()),
                            (index, 2, b"cd".to_vec()),
                            (index, 4, b"e".to_vec()),
                        ])
                        .collect::<Vec<_>>()
                );
            }
            let backend = client.into_inner().unwrap();
            assert_eq!(backend.scalar.close_calls, if at_open { 2 } else { 3 });
            assert_eq!(
                backend
                    .batches
                    .iter()
                    .filter(|(kind, _)| *kind == "open")
                    .count(),
                1,
                "successful OPENs are never replayed to recover a prefix"
            );
        }
    }
}

#[test]
fn stream_cohorts_never_replay_transport_failures_and_close_all_opened_files() {
    for index in [None, Some(1)] {
        let expected = VfError::transport(index, "lost read reply");
        let client = FsClient::new(StreamBackend {
            read_error: Some(expected.clone()),
            ..Default::default()
        });
        let error = client
            .streams_all(&["/a", "/b"], vfsi_sync::StreamOptions::new(), |_, _, _| {
                panic!("no data confirmed")
            })
            .unwrap_err();
        assert_eq!(error.index(), Some(index.unwrap_or(0)));
        assert!(error.is_transport());
        assert!(error.to_string().contains("lost read reply"));
        let backend = client.into_inner().unwrap();
        assert_eq!(backend.batches, [("open", 2), ("read", 2), ("close", 2)]);
        assert_eq!(backend.scalar.close_calls, 2);
    }
}

#[test]
fn stream_cohorts_keep_cleanup_failures_and_isolate_callback_errors() {
    for close_index in [Some(0), Some(2), None] {
        let backend = || StreamBackend {
            scalar: ScalarOnly {
                data: b"data".to_vec(),
                ..Default::default()
            },
            close_error: Some(VfError::transport(close_index, "close reply lost")),
            ..Default::default()
        };
        let client = FsClient::new(backend());
        let result = client.vstream(
            &["/a", "/b", "/c"],
            vfsi_sync::StreamOptions::new(),
            |_, _, _| Ok(std::ops::ControlFlow::Break(())),
        );
        let outcomes = result.unwrap();
        assert_eq!(outcomes.len(), 3);
        for (index, outcome) in outcomes.into_iter().enumerate() {
            if close_index.is_none_or(|failed| failed == index) {
                let error = outcome.unwrap_err();
                assert_eq!(
                    error.state(),
                    if close_index.is_some() {
                        vfsi_core::FailureState::Unknown
                    } else {
                        vfsi_core::FailureState::Failed
                    }
                );
                assert_eq!(error.cause().index(), None);
                assert!(error.cause().is_transport());
                assert_eq!(
                    error.progress(),
                    (index == 0).then_some(&vfsi_core::IoProgress::Stream { next_offset: 4 })
                );
            } else {
                assert_eq!(
                    outcome.unwrap(),
                    vfsi_sync::StreamCompletion::Stopped {
                        next_offset: if index == 0 { 4 } else { 0 },
                    }
                );
            }
        }
        assert_eq!(client.into_inner().unwrap().scalar.close_calls, 3);
        let client = FsClient::new(backend());
        let expected = VfError::errno(9, libc::ENOENT as u32).with_context("nested", "/nested");
        let error = client
            .vstream(
                &["/a", "/b", "/c"],
                vfsi_sync::StreamOptions::new(),
                |_, _, _| Err(expected.clone()),
            )
            .unwrap();
        assert_eq!(error.len(), 3);
        for outcome in error {
            let error = outcome.unwrap_err();
            assert_eq!(error.cause().index(), None);
            assert_eq!(error.into_cause().with_index(9), expected);
        }
        assert_eq!(client.into_inner().unwrap().scalar.close_calls, 3);
    }
}

#[test]
fn stream_callback_errors_preserve_indexed_and_unattributed_transport_failures() {
    for index in [Some(7), None] {
        let client = FsClient::new(ScalarOnly {
            data: b"payload".to_vec(),
            ..Default::default()
        });
        let expected = VfError::transport_with_kind(
            index,
            vfsi_sync::TransportKind::Connection,
            "nested operation lost its reply",
        )
        .with_context("nested-operation", "/nested-file");
        let actual = client
            .read_stream("/file", |_, _| Err(expected.clone()))
            .unwrap_err();
        assert_eq!(actual, expected);
        let backend = client.into_inner().unwrap();
        assert_eq!(backend.close_calls, 1);
    }
}

#[test]
fn directory_visit_starts_with_one_entry_and_respects_tight_limits() {
    let page_sizes = Arc::new(Mutex::new(Vec::new()));
    let client = FsClient::new(ScalarOnly {
        directory_entries: 2_100,
        directory_page_sizes: Arc::clone(&page_sizes),
        ..ScalarOnly::default()
    });
    assert_eq!(
        client
            .listdir("/tree", vfsi_core::api::ListDirOptions::new(), |_| Ok(
                vfsi_core::api::WalkControl::Stop
            ))
            .unwrap(),
        vfsi_sync::TraversalCompletion::Stopped
    );
    assert_eq!(*page_sizes.lock().unwrap(), [1]);

    page_sizes.lock().unwrap().clear();
    let mut seen = 0;
    let error = client
        .listdir(
            "/tree",
            vfsi_sync::ListDirOptions::new().with_max_entries(3),
            |_| {
                seen += 1;
                Ok(vfsi_core::api::WalkControl::Continue)
            },
        )
        .unwrap_err();
    assert_eq!(seen, 3);
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    assert_eq!(*page_sizes.lock().unwrap(), [1, 3]);

    page_sizes.lock().unwrap().clear();
    let mut seen = 0;
    let completion = client
        .listdir("/tree", vfsi_core::api::ListDirOptions::new(), |_| {
            seen += 1;
            Ok(vfsi_core::api::WalkControl::Continue)
        })
        .unwrap();
    assert_eq!(completion, vfsi_sync::TraversalCompletion::Complete);
    assert_eq!(seen, 2_100);
    let page_sizes = page_sizes.lock().unwrap();
    assert_eq!(page_sizes.first(), Some(&1));
    assert_eq!(page_sizes.len(), 18);
    assert!(page_sizes[1..].iter().all(|size| *size == 128));
    let empty = FsClient::new(ScalarOnly::default());
    assert_eq!(
        empty
            .listdir("/empty", vfsi_core::api::ListDirOptions::new(), |_| panic!(
                "empty directory has no entry"
            ))
            .unwrap(),
        vfsi_sync::TraversalCompletion::Complete
    );
    let single = FsClient::new(ScalarOnly {
        directory_entries: 1,
        ..Default::default()
    });
    assert_eq!(
        single
            .listdir("/single", vfsi_core::api::ListDirOptions::new(), |_| Ok(
                vfsi_core::api::WalkControl::Stop
            ))
            .unwrap(),
        vfsi_sync::TraversalCompletion::Stopped
    );
}

#[test]
fn visitor_field_selection_is_forwarded_to_each_page() {
    let fields = Arc::new(Mutex::new(Vec::new()));
    let pages = Arc::new(Mutex::new(Vec::new()));
    let client = FsClient::new(ScalarOnly {
        directory_entries: 3,
        directory_fields: fields.clone(),
        directory_page_sizes: pages.clone(),
        ..Default::default()
    });
    let mut seen = 0;
    client
        .listdir(
            "/tree",
            vfsi_sync::ListDirOptions::new().with_fields(vfsi_sync::AttrMask::SIZE),
            |_| {
                seen += 1;
                Ok(vfsi_core::api::WalkControl::Continue)
            },
        )
        .unwrap();
    assert_eq!(seen, 3);
    assert_eq!(
        *fields.lock().unwrap(),
        [vfsi_sync::AttrMask::MODE | vfsi_sync::AttrMask::SIZE; 2]
    );
    assert_eq!(*pages.lock().unwrap(), [1, 128]);
}

#[test]
fn native_read_into_dispatches_without_owned_read_results() {
    let calls = Arc::new(AtomicUsize::new(0));
    let client = FsClient::new(ScalarOnly {
        data: b"abcdef".to_vec(),
        direct_into_only: true,
        into_calls: Arc::clone(&calls),
        ..ScalarOnly::default()
    });
    let file = client.open("/file").unwrap();
    let mut first = [0u8; 3];
    let mut second = [0u8; 3];
    let lengths = client
        .vread_into_native(&mut [
            file.read_request_at_into(0, &mut first),
            file.read_request_at_into(3, &mut second),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(
        lengths
            .iter()
            .map(|result| result.bytes_read)
            .collect::<Vec<_>>(),
        [3, 3]
    );
    assert_eq!(
        lengths
            .iter()
            .map(|result| result.offset)
            .collect::<Vec<_>>(),
        [0, 3]
    );
    assert!(!lengths[0].eof);
    assert!(lengths[1].eof);
    assert_eq!(&first, b"abc");
    assert_eq!(&second, b"def");
    let mut scalar = [0u8; 2];
    let mut io = client.std_io(&file);
    io.seek(SeekFrom::Start(2)).unwrap();
    assert_eq!(io.read(&mut scalar).unwrap(), 2);
    assert_eq!(&scalar, b"cd");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let results = client
        .read_batch(
            [
                vfsi_sync::api::ReadOp::into(&file, 0, &mut first),
                vfsi_sync::api::ReadOp::into(&file, 3, &mut second),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert!(
        results
            .iter()
            .all(|result| result.is_buffered() && result.bytes_read() == 3)
    );
    assert_eq!((&first, &second), (b"abc", b"def"));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

#[test]
fn native_read_into_rejects_malformed_backend_results() {
    let client = FsClient::new(ScalarOnly {
        data: b"abc".to_vec(),
        direct_into_only: true,
        wrong_result_file: true,
        ..ScalarOnly::default()
    });
    let file = client.open("/file").unwrap();
    let mut buffer = [0u8; 3];
    let error = client
        .vread_into_native(&mut [file.read_request_at_into(0, &mut buffer)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(0));

    let backend = ScalarOnly {
        data: b"abc".to_vec(),
        direct_into_only: true,
        ..ScalarOnly::default()
    };
    let limit = Arc::clone(&backend.vector_result_limit);
    let client = FsClient::new(backend);
    let file = client.open("/file").unwrap();
    *limit.lock().unwrap() = Some(0);
    let error = client
        .vread_into_native(&mut [file.read_request_at_into(0, &mut buffer)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(0));
}

#[test]
fn notifications_reenter_and_drop_handles_after_unlock() {
    let backend = ScalarOnly::default();
    let notifications = backend.notifications.clone();
    let observed = backend.close_observed.clone();
    let client = FsClient::new(backend);
    let file = client.open("/file").unwrap();
    let callback_client = client.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    notifications.lock().unwrap().push(Box::new(move || {
        drop(file);
        callback_client.capabilities().unwrap();
        tx.send(()).unwrap();
    }));
    let worker = std::thread::spawn(move || {
        client.capabilities().unwrap();
        client
    });
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .expect("notification reentry must not deadlock");
    let client = worker.join().unwrap();
    client.drain_cleanup().unwrap();
    assert_eq!(observed.load(Ordering::SeqCst), 1);
}

#[test]
fn file_drop_never_waits_for_an_in_flight_rpc() {
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let backend = ScalarOnly {
        data: vec![1],
        blocked_read: Some((entered.clone(), release.clone())),
        ..Default::default()
    };
    let closes = backend.close_observed.clone();
    let client = FsClient::new(backend);
    let reading = client.open("/file").unwrap();
    let dropped = client.open("/other").unwrap();
    let read_client = client.clone();
    let reader = std::thread::spawn(move || read_client.std_io(&reading).read(&mut [0]).unwrap());
    entered.wait();
    let (tx, rx) = std::sync::mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(dropped);
        tx.send(()).unwrap();
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(1));
    let before = closes.load(Ordering::SeqCst);
    release.wait();
    reader.join().unwrap();
    dropper.join().unwrap();
    result.expect("Drop must not wait for the backend mutex");
    assert_eq!(before, 0, "Drop must not issue a remote close");
    client.drain_cleanup().unwrap();
    assert_eq!(closes.load(Ordering::SeqCst), 2);
}

#[test]
fn failed_deferred_close_keeps_ownership_until_drained() {
    let backend = ScalarOnly {
        close_failures_remaining: 1,
        ..Default::default()
    };
    let closes = backend.close_observed.clone();
    let client = FsClient::new(backend);
    drop(client.open("/file").unwrap());
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    let error = client.drain_cleanup().unwrap_err();
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
    assert_eq!(error.operation(), Some("close"));
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    client.drain_cleanup().unwrap();
    client.drain_cleanup().unwrap();
    assert_eq!(closes.load(Ordering::SeqCst), 2);
}

#[test]
fn observer_panic_does_not_poison_the_backend_mutex() {
    let backend = ScalarOnly::default();
    let notifications = backend.notifications.clone();
    let client = FsClient::new(backend);
    notifications
        .lock()
        .unwrap()
        .push(Box::new(|| panic!("observer failure")));
    assert!(client.capabilities().is_ok());
    assert!(client.capabilities().is_ok());
}

#[test]
fn final_owner_drains_deferred_cleanup() {
    let backend = ScalarOnly::default();
    let closes = backend.close_observed.clone();
    let client = FsClient::new(backend);
    drop(client.open("/file").unwrap());
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    drop(client);
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}

#[test]
fn ordinary_operation_drains_dropped_handles() {
    let backend = ScalarOnly::default();
    let closes = backend.close_observed.clone();
    let client = FsClient::new(backend);
    drop(client.open("/file").unwrap());
    client.capabilities().unwrap();
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}

#[test]
fn filesystem_statistics_keep_successes_around_backend_failure_in_one_batch() {
    let backend = ScalarOnly {
        stats_error_index: Some(1),
        ..Default::default()
    };
    let calls = backend.stats_calls.clone();
    let client = FsClient::new(backend);
    let outcomes = client.vstatfs(&["/a", "/missing", "/b"]).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok());
    assert!(outcomes[2].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Failed);
    assert_eq!(error.cause().path(), Some(std::path::Path::new("/missing")));
    assert_eq!(error.cause().index(), None);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn filesystem_statistics_validate_backend_shape_and_isolate_invalid_handles() {
    for count in [0, 1, 3] {
        let client = FsClient::new(ScalarOnly {
            stats_result_count: Some(count),
            ..Default::default()
        });
        assert!(client.statfs_all(&["/a", "/b"]).unwrap_err().is_transport());
    }
    let client = FsClient::new(ScalarOnly {
        stats_error_index: Some(2),
        ..Default::default()
    });
    assert!(client.statfs_all(&["/a", "/b"]).unwrap_err().is_transport());
    let backend = ScalarOnly::default();
    let calls = backend.stats_calls.clone();
    let client = FsClient::new(backend);
    client.statfs_all::<&str>(&[]).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut file = client.open("/file").unwrap();
    file.try_close().unwrap();
    let targets = [
        vfsi_sync::Target::Path(std::path::Path::new("/a")),
        vfsi_sync::Target::File(&file),
    ];
    assert_eq!(client.statfs_all(&targets).unwrap_err().index(), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let other = FsClient::new(ScalarOnly::default());
    let foreign = other.open("/foreign").unwrap();
    let targets = [
        vfsi_sync::Target::Path(std::path::Path::new("/a")),
        vfsi_sync::Target::File(&foreign),
    ];
    assert_eq!(client.statfs_all(&targets).unwrap_err().index(), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn portable_handle_queries_and_sync_preserve_valid_siblings() {
    use vfsi_core::api::{AttrsOptions, SyncMode};
    let backend = AttrsBackend::default();
    let queries = Arc::clone(&backend.metadata_calls);
    let syncs = Arc::clone(&backend.scalar.sync_calls);
    let fs = FsClient::new(backend);
    let a = fs.open("/a").unwrap();
    let mut b = fs.open("/b").unwrap();
    fs.getattrs_all(
        &[
            Target::file(&a),
            Target::Path(std::path::Path::new("/path")),
            Target::file(&b),
        ],
        AttrsOptions::new(),
    )
    .unwrap();
    assert_eq!(queries.lock().unwrap()[0].1.len(), 3);
    assert!(queries.lock().unwrap()[0].1[0].is_fd());
    // No-follow applies to path operands, not retained objects.
    fs.getattrs_all(
        &[
            Target::file(&a),
            Target::Path(std::path::Path::new("/link")),
            Target::file(&b),
        ],
        AttrsOptions::new().with_follow_final_symlink(false),
    )
    .unwrap();
    assert_eq!(
        queries
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>(),
        [true, true, false, true]
    );
    fs.sync_batch(&[&a, &b], SyncMode::All).unwrap();
    assert_eq!(syncs.lock().unwrap()[0].0, SyncMode::All);
    assert_eq!(syncs.lock().unwrap()[0].1.len(), 2);
    let foreign_fs = FsClient::new(AttrsBackend::default());
    let foreign = foreign_fs.open("/foreign").unwrap();
    let before = queries.lock().unwrap().len();
    assert_eq!(
        fs.getattrs_all(
            &[Target::file(&a), Target::file(&foreign)],
            AttrsOptions::new()
        )
        .unwrap_err()
        .index(),
        Some(1)
    );
    assert_eq!(
        fs.sync_batch(&[&a, &foreign], SyncMode::Data)
            .unwrap_err()
            .index(),
        Some(1)
    );
    b.try_close().unwrap();
    assert_eq!(
        fs.getattrs_all(&[Target::file(&a), Target::file(&b)], AttrsOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        fs.sync_batch(&[&a, &b], SyncMode::All).unwrap_err().index(),
        Some(1)
    );
    assert_eq!(queries.lock().unwrap().len(), before + 2);
    assert_eq!(syncs.lock().unwrap().len(), 3);
    assert_eq!(syncs.lock().unwrap()[1].1.len(), 1);
    assert_eq!(syncs.lock().unwrap()[2].1.len(), 1);
    fs.getattrs_all::<&str>(&[], AttrsOptions::new()).unwrap();
    fs.sync_batch(&[], SyncMode::All).unwrap();
    assert_eq!(queries.lock().unwrap().len(), before + 2);
    assert_eq!(syncs.lock().unwrap().len(), 3);
    assert_eq!(syncs.lock().unwrap()[1].1.len(), 1);
    assert_eq!(syncs.lock().unwrap()[2].1.len(), 1);
}
#[test]
fn sync_errors_preserve_valid_indices_and_reject_invalid_backend_indices() {
    #[cfg(feature = "test-support")]
    if vfsi_sync::test_support::supervise_with_deadline(
        "sync_errors_preserve_valid_indices_and_reject_invalid_backend_indices",
    ) {
        return;
    }
    use vfsi_core::api::SyncMode;
    for index in [None, Some(1), Some(2)] {
        let error = index.map_or_else(
            || VfError::transport(None, "sync batch rejected before dispatch"),
            |i| VfError::errno(i, libc::EIO as u32),
        );
        let fs = FsClient::new(DefaultBackend {
            scalar: ScalarOnly {
                sync_failure: Some(error),
                ..Default::default()
            },
            ..Default::default()
        });
        let a = fs.open("/a").unwrap();
        let b = fs.open("/b").unwrap();
        let error = fs.sync_batch(&[&a, &b], SyncMode::Data).unwrap_err();
        let calls = std::cell::Cell::new(0);
        let projected_error = fs
            .vfsync_projected_native(&[&a, &b], SyncMode::Data, |file| {
                calls.set(calls.get() + 1);
                // Projection is caller code: reentry must remain outside the
                // backend lock, including when attributing an indexed error.
                fs.capabilities().unwrap();
                *file
            })
            .and_then(|outcomes| vfsi_sync::internal::collect_vec_result(Ok(outcomes), 2))
            .unwrap_err();
        assert_eq!(calls.get(), 2, "project each input exactly once");
        for error in [error, projected_error] {
            if index == Some(1) {
                assert_eq!(error.index(), Some(1));
                assert_eq!(error.path(), Some(std::path::Path::new("/b")));
            } else {
                assert!(error.is_transport());
                assert_eq!(error.index(), None);
            }
        }
    }
}

#[test]
fn std_io_write_all_retries_interruptions_without_replaying_progress() {
    for append in [false, true] {
        for interrupt_write_call in 0..3 {
            let backend = ScalarOnly {
                data: b"abc".to_vec(),
                max_write_once: Some(2),
                interrupt_write_call: Some(interrupt_write_call),
                ..Default::default()
            };
            let calls = Arc::clone(&backend.write_calls);
            let client = FsClient::new(backend);
            let file = client
                .open_options()
                .write(true)
                .append(append)
                .open("/file")
                .unwrap();
            let mut io = client.std_io(&file);
            io.seek(SeekFrom::Start(3)).unwrap();
            io.write_all(b"abcdef").unwrap();
            assert_eq!(io.stream_position().unwrap(), 9);
            assert_eq!(calls.load(Ordering::SeqCst), 4);
            // An empty completion must issue no I/O or change the cursor.
            io.write_all(b"").unwrap();
            assert_eq!(io.stream_position().unwrap(), 9);
            assert_eq!(calls.load(Ordering::SeqCst), 4);
            drop(io);
            file.close().unwrap();
            assert_eq!(client.into_inner().unwrap().data, b"abcabcdef");
        }
    }
}

#[test]
fn std_io_write_all_reports_write_zero_without_retrying() {
    let backend = ScalarOnly {
        data: b"abc".to_vec(),
        max_write_once: Some(0),
        ..Default::default()
    };
    let calls = Arc::clone(&backend.write_calls);
    let client = FsClient::new(backend);
    let file = client.open_options().append(true).open("/file").unwrap();
    let mut io = client.std_io(&file);
    io.seek(SeekFrom::Start(3)).unwrap();
    assert_eq!(
        io.write_all(b"data").unwrap_err().kind(),
        std::io::ErrorKind::WriteZero
    );
    assert_eq!(io.stream_position().unwrap(), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(io);
    file.close().unwrap();
    assert_eq!(client.into_inner().unwrap().data, b"abc");
}

#[test]
fn append_adapters_share_completion_and_report_actual_end_after_short_writes() {
    for vector_open in [false, true] {
        let client = FsClient::new(ScalarOnly {
            data: b"abc".to_vec(),
            max_write_once: Some(2),
            ..Default::default()
        });
        let file = if vector_open {
            client
                .open_batch(&[OpenOp::new("/file", OpenFlags::READ | OpenFlags::APPEND)])
                .unwrap()
                .remove(0)
        } else {
            client
                .open_options()
                .read(true)
                .append(true)
                .open("/file")
                .unwrap()
        };
        let mut io = client.std_io(&file);
        assert_eq!(io.write(b"XY").unwrap(), 2);
        assert_eq!(io.stream_position().unwrap(), 5);
        io.seek(SeekFrom::Start(0)).unwrap();
        let mut prefix = [0; 3];
        io.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"abc");
        io.write_all(b"12345").unwrap();
        assert_eq!(io.stream_position().unwrap(), 10);
        io.write_all(b"").unwrap();
        assert_eq!(io.stream_position().unwrap(), 10);
        drop(io);
        file.close().unwrap();
        assert_eq!(client.into_inner().unwrap().data, b"abcXY12345");
    }
    // Interleaving outside appends must not leave the adapter at first-offset
    // plus payload length instead of the last acknowledged end.
    let client = FsClient::new(WriteBoundaryProbe {
        append_end: Some(3),
        append_gap: 3,
        ..Default::default()
    });
    let file = client.open_options().append(true).open("/file").unwrap();
    let mut io = client.std_io(&file);
    io.write_all(b"abcdef").unwrap();
    assert_eq!(io.stream_position().unwrap(), 15); // [3..5), [8..10), [13..15)
}

#[test]
fn complete_append_vectors_order_short_writes_and_never_replay_failures() {
    let client = FsClient::new(ScalarOnly {
        data: b"abc".to_vec(),
        max_write_once: Some(2),
        ..Default::default()
    });
    let file = client.open_options().append(true).open("/file").unwrap();
    let results = client
        .write_batch(
            &[
                vfsi_core::api::WriteOp::at(&file, 0, b"AAAA"),
                vfsi_core::api::WriteOp::at(&file, 100, b"BBBB"),
            ],
            vfsi_core::api::WriteOptions::new().with_write_all(true),
        )
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|r| (r.offset, r.bytes_written))
            .collect::<Vec<_>>(),
        [(3, 4), (7, 4)]
    );
    file.close().unwrap();
    assert_eq!(client.into_inner().unwrap().data, b"abcAAAABBBB");
    for failure_after in [0, 1] {
        for error in [
            VfError::errno(0, libc::ENOSPC as u32),
            VfError::transport(None, "lost append reply"),
        ] {
            let backend = WriteBoundaryProbe {
                append_end: Some(3),
                failure_after,
                failure: Some(error.clone()),
                ..Default::default()
            };
            let calls = Arc::clone(&backend.calls);
            let client = FsClient::new(backend);
            let file = client.open_options().append(true).open("/file").unwrap();
            let data = b"abcdef";
            let mut io = client.std_io(&file);
            let actual = io.write_all(data).unwrap_err();
            assert_eq!(
                io.stream_position().unwrap(),
                if failure_after == 0 { 0 } else { 5 }
            );
            assert_eq!(actual.kind(), error.kind());
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), failure_after + 1);
            for (wave, call) in calls.iter().enumerate() {
                assert_eq!(call[0].offset, VfOffset::End);
                assert_eq!(call[0].pointer, data[2 * wave..].as_ptr() as usize);
            }
        }
    }
}

#[derive(Default)]
struct DirectoryProbe {
    scalar: ScalarOnly,
    opens: Arc<AtomicUsize>,
    open_shape: u8,
    removed: Arc<Mutex<Vec<i32>>>,
    closed: Arc<Mutex<Vec<i32>>>,
    failure: Option<(i32, VfError)>,
}
handle_contract!(DirectoryProbe);
impl VectorBackend for DirectoryProbe {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, _: &[ReadOp]) -> vfsi_sync::VecResult<vfsi_sync::ReadResult> {
        unreachable!("directory test must not issue file reads")
    }
    fn vopen_dirs_impl(
        &mut self,
        paths: &[&std::path::Path],
    ) -> vfsi_sync::VecResult<vfsi_sync::VfDir> {
        use vfsi_sync::{IoProgress, OpError, VfDir};
        self.opens.fetch_add(1, Ordering::SeqCst);
        if self.open_shape == 4 {
            return Err(VfError::unsupported(None));
        }
        let mut results: Vec<_> = paths
            .iter()
            .map(|path| {
                path.to_str()
                    .unwrap()
                    .trim_start_matches('/')
                    .parse::<i32>()
                    .map(|fd| VfDir::Descriptor { fd, owner: 1 })
                    .map_err(|_| OpError::failed(VfError::errno(None, libc::ENOENT as u32), None))
            })
            .collect();
        match self.open_shape {
            1 => results.truncate(1),
            2 => results.push(Ok(VfDir::Descriptor { fd: 3, owner: 1 })),
            3 => {
                results[1] = Err(OpError::failed(
                    VfError::errno(None, libc::EACCES as u32),
                    Some(IoProgress::Write { bytes_written: 1 }),
                ))
            }
            5 => results[2] = Ok(VfDir::Descriptor { fd: 0, owner: 1 }),
            6 => results[2] = Ok(VfDir::Descriptor { fd: -1, owner: 1 }),
            _ => {}
        }
        Ok(results)
    }
    fn remove_dir_contents_handles_with_options_impl(
        &mut self,
        dirs: &[&vfsi_sync::VfDir],
        _: vfsi_sync::RemoveOptions,
    ) -> vfsi_sync::VecResult<()> {
        use vfsi_sync::OpError;
        let mut fatal: Option<VfError> = None;
        Ok(dirs
            .iter()
            .map(|dir| {
                if let Some(cause) = &fatal {
                    return Err(OpError::not_attempted(cause.clone()));
                }
                let vfsi_sync::VfDir::Descriptor { fd, .. } = dir else {
                    panic!("path fallback");
                };
                self.removed.lock().unwrap().push(*fd);
                if let Some((failed, cause)) = &self.failure
                    && fd == failed
                {
                    return Err(if cause.is_transport() {
                        fatal = Some(cause.clone());
                        OpError::unknown(cause.clone(), None)
                    } else {
                        OpError::failed(cause.clone(), None)
                    });
                }
                Ok(())
            })
            .collect())
    }
    fn close_dir_impl(&mut self, dir: &vfsi_sync::VfDir) -> VfResult<()> {
        let vfsi_sync::VfDir::Descriptor { fd, .. } = dir else {
            panic!("path fallback");
        };
        self.closed.lock().unwrap().push(*fd);
        Ok(())
    }
}

#[test]
fn directory_vectors_preflight_all_handles_and_use_retained_descriptors() {
    use vfsi_sync::RemoveOptions;
    let backend = DirectoryProbe::default();
    let removed = Arc::clone(&backend.removed);
    let closed = Arc::clone(&backend.closed);
    let fs = FsClient::new(backend);
    assert!(fs.open_dirs::<&str>(&[]).unwrap().is_empty());
    fs.vremove_dir_contents(&[], RemoveOptions::default())
        .unwrap();
    let mut dirs = fs.open_dirs(&["/0", "/1"]).unwrap();
    let other = FsClient::new(DirectoryProbe::default());
    let foreign = other.open_dirs(&["/1"]).unwrap().remove(0);
    let outcomes = fs
        .vremove_dir_contents(&[&dirs[0], &foreign], RemoveOptions::default())
        .unwrap();
    assert!(outcomes[0].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        vfsi_sync::FailureState::NotAttempted
    );
    assert_eq!(*removed.lock().unwrap(), [0]);
    dirs[1].try_close().unwrap();
    let outcomes = fs
        .vremove_dir_contents(
            &[&dirs[1], &dirs[0]],
            RemoveOptions::new().with_continue_on_error(true),
        )
        .unwrap();
    assert_eq!(
        outcomes[0].as_ref().unwrap_err().cause().raw_code(),
        libc::EBADF as u32
    );
    assert!(outcomes[1].is_ok());
    assert_eq!(*removed.lock().unwrap(), [0, 0]);
    let second = fs.open_dirs(&["/2"]).unwrap().remove(0);
    fs.clone()
        .vremove_dir_contents(&[&dirs[0], &second], RemoveOptions::default())
        .unwrap();
    fs.vremove_dir_contents(
        &[&second],
        RemoveOptions::new().with_batch_size(std::num::NonZeroUsize::new(1)),
    )
    .unwrap();
    assert_eq!(*removed.lock().unwrap(), [0, 0, 0, 2, 2]);
    for dir in dirs {
        dir.close().unwrap();
    }
    second.close().unwrap();
    assert_eq!(*closed.lock().unwrap(), [1, 0, 2]);
}

#[test]
fn directory_vector_errors_preserve_index_and_do_not_replay_transport_failures() {
    for (keep_going, transport, expected) in [
        (false, false, vec![0, 1, 2]),
        (true, false, vec![0, 1, 2]),
        (true, true, vec![0, 1]),
    ] {
        let backend = DirectoryProbe {
            failure: Some((
                1,
                if transport {
                    VfError::transport(Some(0), "reply lost")
                } else {
                    VfError::errno(0, libc::EACCES as u32)
                },
            )),
            ..Default::default()
        };
        let removed = Arc::clone(&backend.removed);
        let fs = FsClient::new(backend);
        let dirs = fs.open_dirs(&["/0", "/1", "/2"]).unwrap();
        let outcomes = fs
            .vremove_dir_contents(
                &dirs.iter().collect::<Vec<_>>(),
                vfsi_sync::RemoveOptions::new().with_continue_on_error(keep_going),
            )
            .unwrap();
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes[0].is_ok());
        let error = outcomes[1].as_ref().unwrap_err();
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(std::path::Path::new("/1")));
        assert_eq!(
            error.state(),
            if transport {
                vfsi_sync::FailureState::Unknown
            } else {
                vfsi_sync::FailureState::Failed
            }
        );
        if transport {
            assert_eq!(
                outcomes[2].as_ref().unwrap_err().state(),
                vfsi_sync::FailureState::NotAttempted
            );
        } else {
            assert!(outcomes[2].is_ok());
        }
        assert_eq!(*removed.lock().unwrap(), expected);
    }
}

#[test]
fn strict_directory_open_cleans_up_all_successful_siblings() {
    let backend = DirectoryProbe::default();
    let closed = Arc::clone(&backend.closed);
    let fs = FsClient::new(backend);
    assert_eq!(
        fs.open_dirs(&["/0", "/missing", "/2"]).unwrap_err().index(),
        Some(1)
    );
    fs.drain_cleanup().unwrap();
    let mut closed = closed.lock().unwrap().clone();
    closed.sort_unstable();
    assert_eq!(closed, [0, 2]);
}

#[test]
fn portable_open_builder_preflights_and_preserves_all_flag_combinations() {
    for bits in 0..64 {
        let [read, write, append, truncate, create, exclusive] =
            [0, 1, 2, 3, 4, 5].map(|bit| bits & (1 << bit) != 0);
        let backend = ScalarOnly::default();
        let opened = Arc::clone(&backend.open_requests);
        let client = FsClient::new(backend);
        // Both paths name the same portable builder type.
        let mut options: vfsi_core::api::OpenOptions<'_, _> = client.open_options();
        options
            .read(read)
            .write(write)
            .append(append)
            .truncate(truncate)
            .create(create)
            .create_new(exclusive)
            .mode(0o640);
        let cloned: vfsi_sync::OpenOptions<'_, _> = options.clone();
        let one = options.open("/one");
        let many = cloned.vopen(&["/two", "/three"]);
        let writable = write || append;
        if (!read && !writable) || ((truncate || create || exclusive) && !writable) {
            assert_eq!(one.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(many.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                opened.lock().unwrap().is_empty(),
                "invalid flags reached backend"
            );
        } else {
            one.unwrap().close().unwrap();
            client.close_files(many.unwrap()).unwrap();
            let opened = opened.lock().unwrap();
            assert_eq!(
                opened
                    .iter()
                    .map(|op| op.path().to_str().unwrap())
                    .collect::<Vec<_>>(),
                ["/one", "/two", "/three"]
            );
            for op in opened.iter() {
                for (flag, enabled) in [
                    (OpenFlags::READ, read),
                    (OpenFlags::WRITE, write),
                    (OpenFlags::APPEND, append),
                    (OpenFlags::TRUNCATE, truncate),
                    (OpenFlags::CREATE, create),
                    (OpenFlags::CREATE_NEW, exclusive),
                ] {
                    assert_eq!(op.flags().contains(flag), enabled);
                }
                assert_eq!(op.creation_mode(), 0o640);
            }
        }
    }
}

#[test]
fn write_allv_orders_aliases_and_unknown_objects_across_short_writes() {
    for (alias_objects, unknown_objects, unknown_object_fd) in [
        (true, false, None),
        (false, true, None),
        (false, false, Some(2)),
    ] {
        let backend = WriteBoundaryProbe {
            scalar: ScalarOnly {
                alias_objects,
                unknown_objects,
                unknown_object_fd,
                ..Default::default()
            },
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let client = FsClient::new(backend);
        let first = client.open("/original").unwrap();
        let alias = client.open("/renamed-or-hardlink").unwrap();
        let payloads: Vec<_> = (0..64).map(|index| [index as u8; 4]).collect();
        let requests: Vec<_> = payloads
            .iter()
            .enumerate()
            .map(|(index, data)| {
                // Discover an unknown alias only after the prefix was indexed.
                if index < 32 {
                    first.write_request_at(0, data)
                } else {
                    alias.write_request_at(2, data)
                }
            })
            .collect();
        client
            .vwrite_all_native(&requests)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let waves = calls.lock().unwrap();
        assert_eq!(waves.len(), 128);
        assert!(waves.iter().all(|wave| wave.len() == 1));
        for (index, _) in payloads.iter().enumerate() {
            for part in 0..2 {
                let op = &waves[2 * index + part][0];
                assert_eq!(
                    op.offset,
                    VfOffset::At(if index < 32 { 0 } else { 2 } + 2 * part as u64)
                );
                assert_eq!(op.pointer, payloads[index][2 * part..].as_ptr() as usize);
                assert_eq!(op.length, 4 - 2 * part);
            }
        }
    }
}

#[test]
fn indexed_write_frontier_stops_at_late_short_write_failure_without_replay() {
    for error in [
        VfError::errno(0, libc::ENOSPC as u32),
        VfError::transport(None, "lost indexed write reply"),
    ] {
        let backend = WriteBoundaryProbe {
            failure: Some(error.clone()),
            failure_after: 53,
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let file = fs.open("/indexed").unwrap();
        let payloads: Vec<_> = (0..64).map(|i| [i as u8; 4]).collect();
        let requests: Vec<_> = payloads
            .iter()
            .map(|data| file.write_request_at(0, data))
            .collect();
        let actual = fs
            .vwrite_all_native(&requests)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap_err();
        assert_eq!(actual.is_transport(), error.is_transport());
        assert_eq!(actual.index(), Some(26));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 54, "failed suffix must not be replayed");
        for (index, wave) in calls.iter().enumerate() {
            assert_eq!(wave.len(), 1);
            let part = index % 2;
            assert_eq!(
                wave[0].pointer,
                payloads[index / 2][2 * part..].as_ptr() as usize
            );
            assert_eq!(wave[0].offset, VfOffset::At(2 * part as u64));
            assert_eq!(wave[0].length, 4 - 2 * part);
        }
    }
}

#[test]
fn projected_metadata_vectors_convert_each_target_once_and_isolate_foreign_mutations() {
    use std::cell::Cell;
    let backend = AttrsBackend::default();
    let queries = backend.metadata_calls.clone();
    let updates = backend.scalar.attrs_calls.clone();
    let fs = FsClient::new(backend);
    let calls = Cell::new(0);
    let targets = [std::path::Path::new("/a"), std::path::Path::new("/b")];
    fs.vgetattrs_projected_native(&targets, vfsi_core::AttrMask::MODE, false, |path| {
        calls.set(calls.get() + 1);
        Target::Path(path)
    })
    .unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(queries.lock().unwrap().len(), 1);
    assert_eq!(queries.lock().unwrap()[0].1.len(), 2);
    calls.set(0);
    let stats_backend = ScalarOnly::default();
    let stats = stats_backend.stats_calls.clone();
    let stats_fs = FsClient::new(stats_backend);
    stats_fs
        .vstatfs_projected_native(&targets, |path| {
            calls.set(calls.get() + 1);
            Target::Path(path)
        })
        .unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(stats.load(Ordering::SeqCst), 1);
    let local = fs.open("/local").unwrap();
    let foreign_fs = FsClient::new(AttrsBackend::default());
    let foreign = foreign_fs.open("/foreign").unwrap();
    let ops = [
        SetAttrsOp::file(&local).len(1),
        SetAttrsOp::file(&foreign).len(2),
    ];
    calls.set(0);
    let outcomes = fs
        .vsetattrs_projected_native(&ops, |target| {
            calls.set(calls.get() + 1);
            *target
        })
        .unwrap();
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().path(), Some(std::path::Path::new("/foreign")));
    // The valid sibling reaches the scalar-only backend and reports its own Unsupported.
    assert_eq!(
        outcomes[0].as_ref().unwrap_err().cause().kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(calls.get(), 2);
    assert!(updates.lock().unwrap().is_empty());
}

#[test]
fn projected_io_errors_allow_client_reentry() {
    #[cfg(feature = "test-support")]
    if vfsi_sync::test_support::supervise_with_deadline("projected_io_errors_allow_client_reentry")
    {
        return;
    }
    let fs = FsClient::new(WriteBoundaryProbe {
        failure: Some(VfError::errno(0, libc::ENOSPC as u32)),
        ..Default::default()
    });
    let file = fs.open("/file").unwrap();
    let reads = [file.read_request_at(0, 1)];
    let projections = std::cell::Cell::new(0);
    let error = fs
        .vread_with_limit_projected_native(&reads, 1, |request| {
            projections.set(projections.get() + 1);
            fs.capabilities().unwrap();
            request
        })
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
    assert_eq!(projections.replace(0), 1);
    let writes = [file.write_request_at(0, b"x")];
    let error = fs
        .vwrite_mapped_native(&writes, |request| {
            projections.set(projections.get() + 1);
            fs.capabilities().unwrap();
            *request
        })
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::ENOSPC as u32);
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
    assert_eq!(projections.get(), 1);
}

#[test]
fn complete_write_projection_can_reenter_during_alias_planning() {
    #[cfg(feature = "test-support")]
    if vfsi_sync::test_support::supervise_with_deadline(
        "complete_write_projection_can_reenter_during_alias_planning",
    ) {
        return;
    }
    for unknown_objects in [false, true] {
        let backend = WriteBoundaryProbe {
            scalar: ScalarOnly {
                unknown_objects,
                ..Default::default()
            },
            ..Default::default()
        };
        let waves = backend.calls.clone();
        let fs = FsClient::new(backend);
        let first = fs.open("/first").unwrap();
        let second = fs.open("/second").unwrap();
        let writes = [
            first.write_request_at(0, b"abc"),
            second.write_request_at(0, b"xyz"),
        ];
        let calls = std::cell::Cell::new(0);
        let results = fs
            .vwrite_all_mapped_native(&writes, |request| {
                calls.set(calls.get() + 1);
                fs.capabilities().unwrap();
                *request
            })
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        assert!(results.iter().all(|result| result.bytes_written == 3));
        assert_eq!(calls.get(), writes.len());
        let waves = waves.lock().unwrap();
        if unknown_objects {
            assert_eq!(waves.len(), 4);
            assert!(waves.iter().all(|wave| wave.len() == 1));
        } else {
            assert_eq!(waves.len(), 2);
            assert!(waves.iter().all(|wave| wave.len() == 2));
        }
    }
}

#[test]
fn malformed_open_results_retain_failed_cleanup_for_retry() {
    for failures in [1, 2] {
        let backend = ScalarOnly {
            vector_result_limit: Arc::new(Mutex::new(Some(1))),
            close_failures_remaining: failures,
            ..Default::default()
        };
        let observed = backend.close_observed.clone();
        let client = FsClient::new(backend);
        let error = client
            .open_batch(&[
                OpenOp::new("/first", OpenFlags::READ),
                OpenOp::new("/second", OpenFlags::READ),
            ])
            .unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.index(), Some(1));
        assert!(error.to_string().contains("missing open outcome"));
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        for _ in 1..failures {
            assert!(client.drain_cleanup().is_err());
        }
        client.drain_cleanup().unwrap();
        assert_eq!(observed.load(Ordering::SeqCst), failures + 1);
        let backend = client.into_inner().unwrap();
        assert!(!backend.open);
        assert_eq!(backend.close_calls, failures + 1);
    }
}

#[test]
fn generic_path_conversion_can_reenter_namespace_operations() {
    #[cfg(feature = "test-support")]
    if vfsi_sync::test_support::supervise_with_deadline(
        "generic_path_conversion_can_reenter_namespace_operations",
    ) {
        return;
    }
    struct ReentrantPath<'a, F: VectorBackend>(&'a FsClient<F>, &'static std::path::Path);
    impl<F: VectorBackend + 'static> AsRef<std::path::Path> for ReentrantPath<'_, F> {
        fn as_ref(&self) -> &std::path::Path {
            self.0.capabilities().unwrap();
            self.1
        }
    }
    let fs = FsClient::new(DefaultBackend {
        scalar: ScalarOnly {
            read_failure: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let path = |name: &'static str| ReentrantPath(&fs, std::path::Path::new(name));
    assert!(
        fs.vrename(&[(path("/from"), path("/to"))], Default::default())
            .is_err()
    );
    assert!(
        fs.vmkdir(&[vfsi_core::api::MkDirOp::new(path("/dir"), 0o777)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
            })
            .is_err()
    );
    assert!(
        fs.vcopy(&[(path("/from"), path("/to"))], Default::default())
            .is_err()
    );
    assert!(
        fs.read_files_native(&[path("/file")], Default::default())
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .is_err()
    );
    assert!(fs.ensure_empty_dir(path("/dir")).is_err());

    let fs = FsClient::new(DirectoryProbe::default());
    let closed = fs
        .open_dirs(&[
            ReentrantPath(&fs, std::path::Path::new("/0")),
            ReentrantPath(&fs, std::path::Path::new("/1")),
        ])
        .unwrap();
    assert_eq!(closed.len(), 2);
    for dir in closed {
        dir.close().unwrap();
    }
}

#[test]
fn complete_append_ignores_overflowing_offsets_in_planning_and_short_repairs() {
    use vfsi_core::api::{WriteOp as PublicWrite, WriteOptions};
    for offset in [u64::MAX, u64::MAX - 1] {
        for failure in [None, Some(VfError::transport(None, "lost append reply"))] {
            let backend = WriteBoundaryProbe {
                append_end: Some(3),
                append_gap: 3,
                failure: failure.clone(),
                failure_after: 1,
                ..Default::default()
            };
            let calls = backend.calls.clone();
            let client = FsClient::new(backend);
            let first = client.open_options().append(true).open("/same").unwrap();
            let alias = client.open_options().append(true).open("/same").unwrap();
            let result = client.write_batch(
                &[
                    PublicWrite::at(&first, offset, b"abcde"),
                    PublicWrite::at(&alias, offset, b"XY"),
                ],
                WriteOptions::new().with_write_all(true),
            );
            if let Some(failure) = failure {
                let error = result.unwrap_err();
                assert_eq!(error.transport_message(), failure.transport_message());
                assert_eq!(error.index(), Some(0));
                assert_eq!(
                    calls.lock().unwrap().len(),
                    2,
                    "never replay a failed append"
                );
            } else {
                let values = result.unwrap();
                assert_eq!((values[0].offset, values[0].bytes_written), (9, 5));
                assert_eq!((values[1].offset, values[1].bytes_written), (17, 2));
                let calls = calls.lock().unwrap();
                assert_eq!(calls.len(), 4);
                assert!(
                    calls
                        .iter()
                        .all(|wave| wave.len() == 1 && wave[0].offset == VfOffset::End)
                );
            }
        }
    }
    let client = FsClient::new(ScalarOnly {
        data: b"base".to_vec(),
        max_write_once: Some(2),
        ..Default::default()
    });
    let file = client.open_options().append(true).open("/file").unwrap();
    client
        .write_batch(
            &[PublicWrite::at(&file, u64::MAX, b"abcde")],
            WriteOptions::new().with_write_all(true),
        )
        .unwrap();
    file.close().unwrap();
    assert_eq!(client.into_inner().unwrap().data, b"baseabcde");
}

#[test]
fn mapped_io_projects_each_operand_once_on_success_and_invalid_replies() {
    for invalid_reply in [false, true] {
        let fs = FsClient::new(ScalarOnly {
            data: b"abc".to_vec(),
            wrong_result_offset: invalid_reply,
            ..Default::default()
        });
        let a = fs.open("/a").unwrap();
        let b = fs.open("/b").unwrap();
        let reads = [a.read_request_at(0, 1), b.read_request_at(1, 1)];
        let calls = std::cell::Cell::new(0);
        let result = fs
            .vread_with_limit_projected_native(&reads, 2, |request| {
                calls.set(calls.get() + 1);
                fs.capabilities().unwrap();
                request
            })
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            });
        assert_eq!(calls.get(), 2);
        if invalid_reply {
            assert_eq!(result.unwrap_err().path(), Some(std::path::Path::new("/a")));
        } else {
            assert_eq!(result.unwrap().len(), 2);
        }
        let writes = [a.write_request_at(0, b"x"), b.write_request_at(1, b"y")];
        calls.set(0);
        let result = fs
            .vwrite_mapped_native(&writes, |request| {
                calls.set(calls.get() + 1);
                fs.capabilities().unwrap();
                *request
            })
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            });
        assert_eq!(calls.get(), 2);
        if invalid_reply {
            assert_eq!(result.unwrap_err().path(), Some(std::path::Path::new("/a")));
        } else {
            assert_eq!(result.unwrap().len(), 2);
        }
        a.close().unwrap();
        b.close().unwrap();
    }
}

#[test]
fn directory_projection_does_not_add_or_fabricate_unrequested_size() {
    let fields = Arc::new(Mutex::new(Vec::new()));
    let client = FsClient::new(ScalarOnly {
        directory_entries: 3,
        directory_fields: fields.clone(),
        ..Default::default()
    });
    let options = vfsi_sync::ListDirOptions::new().with_fields(vfsi_sync::AttrMask::MODE);
    let page = client
        .dir_pages_all(
            &["/tree"],
            vec![None],
            options,
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        page.listing
            .entries
            .iter()
            .all(|entry| entry.attrs().len().is_none())
    );
    assert_eq!(*fields.lock().unwrap(), [vfsi_sync::AttrMask::MODE]);
    drop(page);
    fields.lock().unwrap().clear();
    client
        .listdirs_all(&["/tree"], options, |_, page| {
            assert!(
                page.entries
                    .iter()
                    .all(|entry| entry.attrs().len().is_none())
            );
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    assert!(
        fields
            .lock()
            .unwrap()
            .iter()
            .all(|fields| *fields == vfsi_sync::AttrMask::MODE)
    );
}

#[test]
fn metadata_vector_keeps_successes_around_an_item_failure_in_one_call() {
    let backend = AttrsBackend {
        metadata_failure: Some(1),
        ..Default::default()
    };
    let calls = backend.metadata_calls.clone();
    let fs = FsClient::new(backend);
    let outcomes = fs
        .vgetattrs(&["/a", "/bad", "/b"], vfsi_core::api::AttrsOptions::new())
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok());
    assert!(outcomes[2].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().path(), Some(std::path::Path::new("/bad")));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.len(), 3);
}

#[test]
fn native_directory_preflight_propagates_inner_metadata_failure() {
    let mut backend = AttrsBackend {
        metadata_failure: Some(0),
        ..Default::default()
    };
    let error = backend
        .open_dir_impl(std::path::Path::new("/root"))
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::EACCES as u32);
    assert_eq!(error.index(), Some(0));
}

#[derive(Default)]
struct ReadOutcomes {
    scalar: ScalarOnly,
    rejection: Option<VfError>,
    calls: usize,
    lengths: Vec<usize>,
    malformed: bool,
    fail_first: bool,
    fail_once: bool,
    prefix_eof: bool,
    prefix_errno: Option<i32>,
    prefix_failed: bool,
}
handle_contract!(ReadOutcomes);
impl VectorBackend for ReadOutcomes {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> vfsi_sync::VecResult<ReadResult> {
        self.calls += 1;
        self.lengths.push(reads.len());
        if let Some(error) = &self.rejection {
            return Err(error.clone());
        }
        Ok(reads
            .iter()
            .enumerate()
            .map(|(index, read)| {
                if self.fail_once && self.calls > 1 {
                    let VfOffset::At(offset) = read.offset else {
                        unreachable!()
                    };
                    return Ok(ReadResult {
                        file: read.file.clone(),
                        offset,
                        data: Vec::new(),
                        eof: true,
                    });
                }
                if index == 1 || self.fail_first {
                    if self.malformed {
                        return Ok(ReadResult {
                            file: read.file.clone(),
                            offset: 999,
                            data: b"bad".to_vec(),
                            eof: false,
                        });
                    }
                    let cause = self.prefix_errno.map_or_else(
                        || VfError::transport(Some(99), "lost reply after prefix"),
                        |code| VfError::errno(None, code as u32),
                    );
                    let prefix = Some(vfsi_sync::IoProgress::Read(
                        vfsi_core::api::ReadResult::owned(0, b"p".to_vec(), self.prefix_eof),
                    ));
                    return Err(if self.prefix_failed {
                        vfsi_sync::OpError::failed(cause, prefix)
                    } else {
                        vfsi_sync::OpError::unknown(cause, prefix)
                    });
                }
                Ok(ReadResult {
                    file: read.file.clone(),
                    offset: 0,
                    data: b"x".to_vec(),
                    eof: true,
                })
            })
            .collect())
    }
}

#[test]
fn native_read_rejections_never_export_indices_outside_the_submitted_vector() {
    for buffered in [false, true] {
        for index in [None, Some(0), Some(1), Some(99)] {
            let client = FsClient::new(ReadOutcomes {
                rejection: Some(VfError::errno(index, libc::EACCES as u32)),
                ..Default::default()
            });
            let file = client.open("/data").unwrap();
            let error = if buffered {
                let mut buffer = [0xa5; 1];
                let error = client
                    .vread_into_native(&mut [file.read_request_at_into(0, &mut buffer)])
                    .unwrap_err();
                assert_eq!(buffer, [0xa5]);
                error
            } else {
                client
                    .vread_native(&[file.read_request_at(0, 1)])
                    .unwrap_err()
            };
            if index.is_some_and(|index| index >= 1) {
                assert_eq!(error.index(), None);
                assert_eq!(
                    error.transport_kind(),
                    Some(vfsi_core::TransportKind::InvalidReply)
                );
                assert_eq!(error.path(), None);
            } else {
                assert_eq!(error.index(), index);
                assert_eq!(error.raw_code(), libc::EACCES as u32);
                assert_eq!(error.path(), index.map(|_| std::path::Path::new("/data")));
            }
            file.close().unwrap();
            assert_eq!(
                client.into_inner().unwrap().calls,
                1,
                "never replay a rejection"
            );
        }
    }
}

#[test]
fn native_client_reads_keep_successes_and_owned_error_progress() {
    let client = FsClient::new(ReadOutcomes::default());
    let a = client.open("/a").unwrap();
    let b = client.open("/b").unwrap();
    let c = client.open("/c").unwrap();
    let mut outcomes = client
        .vread_native(&[
            a.read_request_at(0, 3),
            b.read_request_at(0, 3),
            c.read_request_at(0, 3),
        ])
        .unwrap();
    assert_eq!(outcomes[0].as_ref().unwrap().data, b"x");
    assert_eq!(outcomes[2].as_ref().unwrap().data, b"x");
    let error = outcomes.remove(1).unwrap_err();
    assert_eq!(error.state(), vfsi_sync::FailureState::Unknown);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().path(), Some(std::path::Path::new("/b")));
    let (_, _, Some(vfsi_sync::IoProgress::Read(prefix))) = error.into_parts() else {
        panic!("confirmed owned prefix missing");
    };
    assert_eq!(prefix.into_data().unwrap(), b"p");
    client.vclose_owned(vec![a, b, c]).unwrap();
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [3]);
}

#[test]
fn native_client_isolates_closed_and_foreign_handles_without_splitting_dispatch() {
    let client = FsClient::new(ReadOutcomes::default());
    let other = FsClient::new(ReadOutcomes::default());
    let live = client.open("/live").unwrap();
    let mut closed = client.open("/closed").unwrap();
    closed.try_close().unwrap();
    let foreign = other.open("/foreign").unwrap();
    let outcomes = client
        .vread_native(&[
            closed.read_request_at(0, 1),
            live.read_request_at(0, 1),
            foreign.read_request_at(0, 1),
        ])
        .unwrap();
    for index in [0, 2] {
        assert_eq!(
            outcomes[index].as_ref().unwrap_err().state(),
            vfsi_sync::FailureState::NotAttempted
        );
    }
    assert_eq!(outcomes[1].as_ref().unwrap().data, b"x");
    assert!(client.vread_native(&[]).unwrap().is_empty());
    live.close().unwrap();
    foreign.close().unwrap();
    drop(closed);
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [1]);
}

#[test]
fn native_client_invalid_read_completion_does_not_discard_valid_siblings() {
    let client = FsClient::new(ReadOutcomes {
        malformed: true,
        ..Default::default()
    });
    let file = client.open("/file").unwrap();
    let outcomes = client
        .vread_native(&[
            file.read_request_at(0, 3),
            file.read_request_at(0, 3),
            file.read_request_at(0, 3),
        ])
        .unwrap();
    assert!(outcomes[0].is_ok());
    assert!(outcomes[2].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_sync::FailureState::Unknown);
    assert!(error.cause().is_transport());
    assert!(error.progress().is_none());
    file.close().unwrap();
    assert_eq!(client.into_inner().unwrap().calls, 1);
}

#[test]
fn native_buffer_client_keeps_confirmed_prefix_and_successful_sibling_storage() {
    let client = FsClient::new(ReadOutcomes::default());
    let file = client.open("/file").unwrap();
    let (mut first, mut failed, mut last) = ([99; 3], [99; 3], [99; 3]);
    let outcomes = client
        .vread_into_native(&mut [
            file.read_request_at_into(0, &mut first),
            file.read_request_at_into(0, &mut failed),
            file.read_request_at_into(0, &mut last),
        ])
        .unwrap();
    assert_eq!(outcomes[0].as_ref().unwrap().bytes_read, 1);
    assert_eq!(outcomes[2].as_ref().unwrap().bytes_read, 1);
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_sync::FailureState::Unknown);
    assert_eq!(error.cause().index(), None);
    let Some(vfsi_sync::IoProgress::Read(prefix)) = error.progress() else {
        panic!("confirmed buffered prefix missing");
    };
    assert!(prefix.is_buffered());
    assert_eq!((prefix.offset(), prefix.bytes_read()), (0, 1));
    assert_eq!(first, [b'x', 99, 99]);
    assert_eq!(failed, [b'p', 99, 99]);
    assert_eq!(last, [b'x', 99, 99]);
    file.close().unwrap();
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [3]);
}

#[test]
fn native_buffer_client_rejected_handles_leave_buffers_untouched() {
    let client = FsClient::new(ReadOutcomes::default());
    let other = FsClient::new(ReadOutcomes::default());
    let live = client.open("/live").unwrap();
    let foreign = other.open("/foreign").unwrap();
    let mut closed = client.open("/closed").unwrap();
    closed.try_close().unwrap();
    let (mut first, mut valid, mut last) = ([99; 3], [99; 3], [99; 3]);
    let outcomes = client
        .vread_into_native(&mut [
            closed.read_request_at_into(0, &mut first),
            live.read_request_at_into(0, &mut valid),
            foreign.read_request_at_into(0, &mut last),
        ])
        .unwrap();
    assert_eq!(outcomes[1].as_ref().unwrap().bytes_read, 1);
    for index in [0, 2] {
        assert_eq!(
            outcomes[index].as_ref().unwrap_err().state(),
            vfsi_sync::FailureState::NotAttempted
        );
    }
    assert_eq!(first, [99; 3]);
    assert_eq!(last, [99; 3]);
    assert_eq!(valid, [b'x', 99, 99]);
    assert!(client.vread_into_native(&mut []).unwrap().is_empty());
    live.close().unwrap();
    foreign.close().unwrap();
    drop(closed);
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [1]);
}

#[test]
fn native_clients_preserve_known_results_when_a_backend_omits_later_outcomes() {
    for buffered in [false, true] {
        let backend = ScalarOnly {
            data: b"abc".to_vec(),
            ..Default::default()
        };
        let limit = Arc::clone(&backend.vector_result_limit);
        let client = FsClient::new(backend);
        let file = client.open("/file").unwrap();
        *limit.lock().unwrap() = Some(1);
        let mut first = [99; 1];
        let mut missing = [99; 1];
        let outcomes = if buffered {
            client
                .vread_into_native(&mut [
                    file.read_request_at_into(0, &mut first),
                    file.read_request_at_into(1, &mut missing),
                ])
                .map(|outcomes| {
                    outcomes
                        .into_iter()
                        .map(|result| result.map(|read| read.bytes_read))
                        .collect::<Vec<_>>()
                })
        } else {
            client
                .vread_native(&[file.read_request_at(0, 1), file.read_request_at(1, 1)])
                .map(|outcomes| {
                    outcomes
                        .into_iter()
                        .map(|result| result.map(|read| read.data.len()))
                        .collect::<Vec<_>>()
                })
        }
        .unwrap();
        assert_eq!(*outcomes[0].as_ref().unwrap(), 1);
        let error = outcomes[1].as_ref().unwrap_err();
        assert_eq!(error.state(), vfsi_sync::FailureState::Unknown);
        assert_eq!(error.cause().index(), None);
        assert!(error.cause().is_transport());
        if buffered {
            assert_eq!(first, *b"a");
            assert_eq!(missing, [99; 1]);
        }
        file.close().unwrap();
    }
}

#[derive(Default)]
struct WholeReadProbe {
    scalar: ScalarOnly,
    waves: Vec<Vec<(FileOperand, VfOffset)>>,
    fail: bool,
    stall: bool,
    shrink: bool,
}
handle_contract!(WholeReadProbe);
impl VectorBackend for WholeReadProbe {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> vfsi_sync::VecResult<ReadResult> {
        self.waves.push(
            reads
                .iter()
                .map(|read| (read.file.clone(), read.offset))
                .collect(),
        );
        Ok(reads
            .iter()
            .map(|read| {
                let VfOffset::At(offset) = read.offset else {
                    panic!("positional whole-file chunk");
                };
                let bad = read.file == FileOperand::from_path("/bad");
                if bad && self.fail && offset == 2 {
                    return Err(vfsi_sync::OpError::unknown(
                        VfError::transport(None, "lost READ suffix"),
                        Some(vfsi_sync::IoProgress::Read(
                            vfsi_core::api::ReadResult::owned(offset, b"c".to_vec(), false),
                        )),
                    ));
                }
                if bad && self.stall && offset == 2 {
                    return Ok(ReadResult {
                        file: read.file.clone(),
                        offset,
                        data: Vec::new(),
                        eof: false,
                    });
                }
                let bytes: &[u8] = if bad {
                    b"abcd"
                } else if self.shrink {
                    b"a"
                } else {
                    b"wxyz"
                };
                let start = (offset as usize).min(bytes.len());
                let end = start.saturating_add(read.length).min(bytes.len());
                Ok(ReadResult {
                    file: read.file.clone(),
                    offset,
                    data: bytes[start..end].to_vec(),
                    eof: end == bytes.len(),
                })
            })
            .collect())
    }
}

#[test]
fn whole_file_vectors_keep_prior_chunks_and_later_siblings_without_replay() {
    let mut backend = WholeReadProbe {
        fail: true,
        ..Default::default()
    };
    let files: Vec<_> = ["/first", "/bad", "/last"]
        .map(FileOperand::from_path)
        .into();
    let outcomes = vfsi_sync::backend::helpers::read_all_vectors(
        &mut backend,
        &files,
        vfsi_sync::ReadAllOptions::new(),
        |pending, _| (pending, 2),
        false,
    )
    .unwrap();
    assert_eq!(outcomes[0].as_ref().unwrap(), b"wxyz");
    assert_eq!(outcomes[2].as_ref().unwrap(), b"wxyz");
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_sync::FailureState::Unknown);
    assert_eq!(error.cause().index(), None);
    let Some(vfsi_sync::IoProgress::Read(prefix)) = error.progress() else {
        panic!("whole-file prefix was lost");
    };
    assert_eq!(prefix.offset(), 0);
    assert_eq!(prefix.data().unwrap(), b"abc");
    assert!(!prefix.eof());
    assert_eq!(backend.waves.len(), 2);
    for (wave, offset) in backend.waves.iter().zip([0, 2]) {
        assert_eq!(wave.len(), 3);
        assert!(
            wave.iter()
                .all(|(_, actual)| *actual == VfOffset::At(offset))
        );
    }
}

#[test]
fn whole_file_budget_counts_successes_and_failed_prefixes_together() {
    let mut backend = WholeReadProbe::default();
    let files: Vec<_> = ["/first", "/bad", "/last"]
        .map(FileOperand::from_path)
        .into();
    let outcomes = vfsi_sync::backend::helpers::read_all_vectors(
        &mut backend,
        &files,
        vfsi_sync::ReadAllOptions::new().with_max_total_bytes(5),
        |pending, remaining| {
            let count = pending.min(remaining.max(1));
            (count, 2.min((remaining / count).max(1)))
        },
        true,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 3);
    let mut retained = 0;
    for outcome in outcomes {
        match outcome {
            Ok(data) => retained += data.len(),
            Err(error) => {
                assert_eq!(error.state(), vfsi_sync::FailureState::Failed);
                assert_eq!(error.cause().raw_code(), libc::EFBIG as u32);
                if let Some(vfsi_sync::IoProgress::Read(prefix)) = error.progress() {
                    retained += prefix.bytes_read();
                    assert!(!prefix.eof());
                }
            }
        }
    }
    assert_eq!(retained, 5);
    let mut seen = Vec::new();
    for request in backend.waves.into_iter().flatten() {
        assert!(!seen.contains(&request), "a completed range was replayed");
        seen.push(request);
    }
}

#[test]
fn default_whole_file_adapter_preserves_each_native_outcome() {
    let mut backend = ReadOutcomes::default();
    let files = [
        FileOperand::from_path("/first"),
        FileOperand::from_path("/bad"),
        FileOperand::from_path("/last"),
    ];
    let outcomes = backend.vread_all_impl(&files).unwrap();
    assert_eq!(outcomes[0].as_ref().unwrap(), b"x");
    assert_eq!(outcomes[2].as_ref().unwrap(), b"x");
    assert!(matches!(outcomes[1].as_ref().unwrap_err().progress(),
        Some(vfsi_sync::IoProgress::Read(prefix)) if prefix.data() == Some(b"p")));
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [3]);
}

#[test]
fn whole_file_zero_progress_keeps_original_slot() {
    let mut backend = WholeReadProbe {
        stall: true,
        shrink: true,
        ..Default::default()
    };
    let files: Vec<_> = ["/a", "/b", "/c", "/d", "/e", "/bad"]
        .map(FileOperand::from_path)
        .into();
    let outcomes = vfsi_sync::backend::helpers::read_all_vectors(
        &mut backend,
        &files,
        vfsi_sync::ReadAllOptions::new(),
        |pending, _| (pending, 2),
        false,
    )
    .unwrap();
    for outcome in &outcomes[..5] {
        assert_eq!(outcome.as_ref().unwrap(), b"a");
    }
    let error = outcomes[5].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_sync::FailureState::Unknown);
    assert!(error.cause().is_transport());
    assert!(
        matches!(error.progress(), Some(vfsi_sync::IoProgress::Read(prefix))
        if prefix.data() == Some(b"ab") && prefix.offset() == 0)
    );
    assert_eq!(backend.waves[0].len(), 6);
    assert_eq!(backend.waves[1], [(files[5].clone(), VfOffset::At(2))]);
}

#[test]
fn whole_file_shrinking_cohorts_keep_data_and_original_positions() {
    let mut backend = WholeReadProbe {
        shrink: true,
        ..Default::default()
    };
    let files: Vec<_> = ["/first", "/bad", "/last"]
        .map(FileOperand::from_path)
        .into();
    let outcomes = vfsi_sync::backend::helpers::read_all_vectors(
        &mut backend,
        &files,
        vfsi_sync::ReadAllOptions::new(),
        |pending, _| (pending, 2),
        false,
    )
    .unwrap();
    assert_eq!(outcomes[0].as_ref().unwrap(), b"a");
    assert_eq!(outcomes[1].as_ref().unwrap(), b"abcd");
    assert_eq!(outcomes[2].as_ref().unwrap(), b"a");
    assert_eq!(backend.waves.len(), 2);
    assert_eq!(backend.waves[1], [(files[1].clone(), VfOffset::At(2))]);
}

#[test]
fn native_whole_file_client_retains_siblings_and_error_progress() {
    let client = FsClient::new(ReadOutcomes::default());
    assert!(
        client
            .read_files_native::<&str>(&[], Default::default())
            .unwrap()
            .is_empty()
    );
    let outcomes = client
        .read_files_native(&["/a", "/bad", "/c"], Default::default())
        .unwrap();
    assert_eq!(outcomes[0].as_ref().unwrap(), b"x");
    assert_eq!(outcomes[2].as_ref().unwrap(), b"x");
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.cause().path(), Some(std::path::Path::new("/bad")));
    assert_eq!(error.cause().index(), None);
    assert!(
        matches!(error.progress(), Some(vfsi_sync::IoProgress::Read(prefix))
        if prefix.data() == Some(b"p"))
    );
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [3]);
}

#[test]
fn public_read_vector_retains_native_outcomes_in_one_batch() {
    let client = FsClient::new(ReadOutcomes::default());
    let files = client
        .open_batch(&[
            OpenOp::new("/a", OpenFlags::READ),
            OpenOp::new("/b", OpenFlags::READ),
            OpenOp::new("/c", OpenFlags::READ),
        ])
        .unwrap();
    let outcomes = client
        .vread(
            files
                .iter()
                .map(|file| vfsi_core::api::ReadOp::range(file, 0, 3)),
            Default::default(),
        )
        .unwrap();
    assert_eq!(outcomes[0].as_ref().unwrap().data().unwrap(), b"x");
    assert_eq!(outcomes[2].as_ref().unwrap().data().unwrap(), b"x");
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.state(), vfsi_sync::FailureState::Unknown);
    assert!(
        matches!(error.progress(), Some(vfsi_sync::IoProgress::Read(prefix))
        if prefix.data() == Some(b"p"))
    );
    client.close_files(files).unwrap();
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [3]);
}

#[test]
fn std_io_advances_only_confirmed_prefix_when_a_read_also_fails() {
    let client = FsClient::new(ReadOutcomes {
        fail_first: true,
        ..Default::default()
    });
    let file = client.open("/file").unwrap();
    let mut buffer = [99; 3];
    let mut io = client.std_io(&file);
    assert_eq!(io.read(&mut buffer).unwrap(), 1);
    assert_eq!(buffer, [b'p', 99, 99]);
    assert!(io.read(&mut [0; 1]).is_err());
    assert_eq!(io.stream_position().unwrap(), 1);
    drop(io);
    file.close().unwrap();
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.calls, 1);
    assert_eq!(backend.lengths, [1]);
}

#[test]
fn synchronization_preserves_known_siblings_and_projects_once_in_one_batch() {
    use vfsi_core::{FailureState, api::SyncMode};
    for transport in [false, true] {
        let backend = ScalarOnly {
            sync_failure: Some(if transport {
                VfError::transport(Some(1), "lost second flush")
            } else {
                VfError::errno(1, libc::EIO as u32)
            }),
            ..Default::default()
        };
        let calls = Arc::clone(&backend.sync_calls);
        let fs = FsClient::new(backend);
        let a = fs.open("/a").unwrap();
        let b = fs.open("/b").unwrap();
        let c = fs.open("/c").unwrap();
        let projected = std::cell::Cell::new(0);
        let outcomes = fs
            .vfsync_projected_native(&[&a, &b, &c], SyncMode::All, |file| {
                projected.set(projected.get() + 1);
                fs.capabilities().unwrap();
                *file
            })
            .unwrap();
        assert_eq!(projected.get(), 3);
        assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
        let error = outcomes[1].as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if transport {
                FailureState::Unknown
            } else {
                FailureState::Failed
            }
        );
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(std::path::Path::new("/b")));
        assert!(error.progress().is_none());
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0].1.len(), 3);
        fs.close_files(vec![a, b, c]).unwrap();
    }
}

#[test]
fn synchronization_shapes_keep_known_slots_and_reject_ambiguous_extras() {
    use vfsi_core::{FailureState, api::SyncMode};
    for count in [1, 3] {
        let backend = ScalarOnly {
            sync_result_count: Some(count),
            ..Default::default()
        };
        let calls = Arc::clone(&backend.sync_calls);
        let fs = FsClient::new(backend);
        let files = fs
            .open_batch(&[
                OpenOp::new("/a", OpenFlags::READ),
                OpenOp::new("/b", OpenFlags::READ),
            ])
            .unwrap();
        let outcomes = fs.vfsync(&[&files[0], &files[1]], SyncMode::All).unwrap();
        assert_eq!(outcomes.len(), 2);
        if count == 1 {
            assert!(outcomes[0].is_ok());
        }
        for outcome in &outcomes[if count == 1 { 1 } else { 0 }..] {
            let error = outcome.as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::Unknown);
            assert_eq!(error.cause().index(), None);
            assert!(error.cause().is_transport());
            assert!(error.progress().is_none());
        }
        assert_eq!(calls.lock().unwrap().len(), 1);
        fs.close_files(files).unwrap();
    }
}

#[derive(Default)]
struct CloseProbe {
    scalar: ScalarOnly,
    response: Option<vfsi_core::VecResult<()>>,
    batches: Arc<Mutex<Vec<Vec<FileOperand>>>>,
}
handle_contract!(
    CloseProbe,
    fn vclose_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        self.batches.lock().unwrap().push(files.to_vec());
        let outcomes = self
            .response
            .take()
            .unwrap_or_else(|| Ok(files.iter().map(|_| Ok(())).collect()))?;
        for (file, result) in files.iter().zip(&outcomes) {
            if result.is_ok() {
                self.scalar.close_impl(file)?;
            }
        }
        Ok(outcomes)
    }
);
impl VectorBackend for CloseProbe {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<vfsi_sync::FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> vfsi_core::VecResult<ReadResult> {
        self.scalar.vread_impl(reads)
    }
}

#[test]
fn close_outcomes_retire_successes_and_keep_each_failure_owner_without_replay() {
    use vfsi_core::{FailureState, OpError};
    let batches = Arc::new(Mutex::new(Vec::new()));
    let client = FsClient::new(CloseProbe {
        response: Some(Ok(vec![
            Ok(()),
            Err(OpError::failed(
                VfError::errno(99, libc::EACCES as u32),
                None,
            )),
            Ok(()),
            Err(OpError::unknown(
                VfError::transport(None, "lost CLOSE reply"),
                None,
            )),
        ])),
        batches: batches.clone(),
        ..Default::default()
    });
    let mut files: Vec<_> = (0..4)
        .map(|index| client.open(format!("/close-{index}")).unwrap())
        .collect();
    let outcomes = client.vclose(&mut files).unwrap();
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        outcomes[3].as_ref().unwrap_err().state(),
        FailureState::Unknown
    );
    assert_eq!(outcomes[1].as_ref().unwrap_err().cause().index(), None);
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().cause().path(),
        Some(std::path::Path::new("/close-1"))
    );
    assert_eq!(
        files
            .iter()
            .map(|file| file.is_closed())
            .collect::<Vec<_>>(),
        [true, false, true, false]
    );
    assert_eq!(batches.lock().unwrap().len(), 1);
    // Already closed siblings are not resubmitted by the strict helper.
    client.close_batch(&mut files).unwrap();
    assert_eq!(
        batches
            .lock()
            .unwrap()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [4, 2]
    );
    drop(files);
    assert_eq!(client.into_inner().unwrap().scalar.close_calls, 4);
}

#[test]
fn close_isolates_foreign_and_closed_handles_and_still_batches_valid_siblings() {
    use vfsi_core::FailureState;
    let batches = Arc::new(Mutex::new(Vec::new()));
    let client = FsClient::new(CloseProbe {
        batches: batches.clone(),
        ..Default::default()
    });
    let other = FsClient::new(CloseProbe::default());
    let mut closed = client.open("/already-closed").unwrap();
    closed.try_close().unwrap();
    batches.lock().unwrap().clear();
    let mut files = [
        client.open("/first").unwrap(),
        other.open("/foreign").unwrap(),
        closed,
        client.open("/last").unwrap(),
    ];
    let outcomes = client.vclose(&mut files).unwrap();
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok() && outcomes[3].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(!files[1].is_closed());
    assert_eq!(
        batches
            .lock()
            .unwrap()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [2]
    );
    assert!(client.vclose(std::iter::empty()).unwrap().is_empty());
    // Foreign handle retains its original owner.
    files[1].try_close().unwrap();
    drop(files);
    assert_eq!(client.into_inner().unwrap().scalar.close_calls, 3);
    assert_eq!(other.into_inner().unwrap().scalar.close_calls, 1);
}

#[test]
fn close_validates_missing_extra_and_impossible_progress_without_losing_owners() {
    use vfsi_core::{FailureState, IoProgress, OpError};
    for response in [
        vec![Ok(())],
        vec![Ok(()), Ok(()), Ok(())],
        vec![
            Ok(()),
            Err(OpError::unknown(
                VfError::transport(None, "bad progress"),
                Some(IoProgress::Write { bytes_written: 1 }),
            )),
        ],
    ] {
        let excess = response.len() > 2;
        let client = FsClient::new(CloseProbe {
            response: Some(Ok(response)),
            ..Default::default()
        });
        let mut files = [
            client.open("/first").unwrap(),
            client.open("/last").unwrap(),
        ];
        let outcomes = client.vclose(&mut files).unwrap();
        assert_eq!(outcomes.len(), 2);
        if excess {
            assert!(
                outcomes
                    .iter()
                    .all(|result| result.as_ref().unwrap_err().state() == FailureState::Unknown)
            );
            assert!(files.iter().all(|file| !file.is_closed()));
        } else {
            assert!(outcomes[0].is_ok() && files[0].is_closed());
            assert_eq!(
                outcomes[1].as_ref().unwrap_err().state(),
                FailureState::Unknown
            );
            assert!(!files[1].is_closed());
        }
        drop(files);
        client.drain_cleanup().unwrap();
    }
}

#[derive(Default)]
struct OwnedOpenProbe {
    scalar: ScalarOnly,
    response: Option<vfsi_core::VecResult<FileOperand>>,
    opens: Arc<Mutex<Vec<usize>>>,
    closes: Arc<Mutex<Vec<usize>>>,
}
handle_contract!(
    OwnedOpenProbe,
    fn vclose_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        self.closes.lock().unwrap().push(files.len());
        self.scalar.vclose_impl(files)
    }
);
impl VectorBackend for OwnedOpenProbe {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_core::VecResult<FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }
    fn vopen_impl(&mut self, requests: &[OpenOp]) -> vfsi_core::VecResult<FileOperand> {
        self.opens.lock().unwrap().push(requests.len());
        self.response
            .take()
            .unwrap_or_else(|| self.scalar.vopen_impl(requests))
    }
    fn vread_impl(&mut self, requests: &[ReadOp]) -> vfsi_core::VecResult<ReadResult> {
        self.scalar.vread_impl(requests)
    }
}
fn owned_open_requests() -> [OpenOp<'static>; 3] {
    ["/first", "/missing", "/last"].map(|path| OpenOp::new(path, OpenFlags::READ))
}
#[test]
fn owned_open_keeps_successful_handles_and_original_error_identity_in_one_batch() {
    use vfsi_core::{FailureState, OpError};
    let backend = OwnedOpenProbe {
        response: Some(Ok(vec![
            Ok(FileOperand::from_fd(1)),
            Err(OpError::failed(
                VfError::errno(77, libc::ENOENT as u32),
                None,
            )),
            Ok(FileOperand::from_fd(3)),
        ])),
        ..Default::default()
    };
    let opens = backend.opens.clone();
    let closes = backend.closes.clone();
    let client = FsClient::new(backend);
    let outcomes = client.vopen(&owned_open_requests()).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(
        outcomes[0].as_ref().unwrap().path(),
        std::path::Path::new("/first")
    );
    assert_eq!(
        outcomes[2].as_ref().unwrap().path(),
        std::path::Path::new("/last")
    );
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().path(), Some(std::path::Path::new("/missing")));
    assert_eq!(*opens.lock().unwrap(), [3]);
    assert!(
        closes.lock().unwrap().is_empty(),
        "core OPEN does not close successful siblings"
    );
    client
        .close_files(outcomes.into_iter().filter_map(Result::ok).collect())
        .unwrap();
    assert_eq!(*closes.lock().unwrap(), [2]);
    assert_eq!(client.into_inner().unwrap().scalar.close_calls, 2);
}
#[test]
fn strict_open_cleanup_batches_successes_and_retains_only_failed_close_owners() {
    use vfsi_core::OpError;
    let backend = OwnedOpenProbe {
        scalar: ScalarOnly {
            close_failures_remaining: 1,
            ..Default::default()
        },
        response: Some(Ok(vec![
            Ok(FileOperand::from_fd(1)),
            Err(OpError::unknown(
                VfError::transport(None, "lost OPEN reply"),
                None,
            )),
            Ok(FileOperand::from_fd(3)),
        ])),
        ..Default::default()
    };
    let opens = backend.opens.clone();
    let closes = backend.closes.clone();
    let observed = backend.scalar.close_observed.clone();
    let client = FsClient::new(backend);
    let error = client.open_batch(&owned_open_requests()).unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(1));
    assert_eq!(*opens.lock().unwrap(), [3]);
    assert_eq!(*closes.lock().unwrap(), [2]);
    assert_eq!(observed.load(Ordering::SeqCst), 2);
    client.drain_cleanup().unwrap();
    assert_eq!(
        client.into_inner().unwrap().scalar.close_calls,
        3,
        "known successful close is not replayed"
    );
}
#[test]
fn owned_open_validates_shapes_and_never_creates_duplicate_cleanup_owners() {
    use vfsi_core::{FailureState, IoProgress, OpError};
    let fd = FileOperand::from_fd;
    let cases = [
        (vec![Ok(fd(1))], vec![true, false, false], 1),
        (
            vec![Ok(fd(1)), Ok(fd(2)), Ok(fd(3)), Ok(fd(4))],
            vec![false; 3],
            4,
        ),
        (
            vec![Ok(fd(7)), Ok(fd(7)), Ok(fd(9))],
            vec![false, false, true],
            2,
        ),
        (vec![Ok(fd(7)), Ok(fd(7)), Ok(fd(7))], vec![false; 3], 1),
        (
            vec![Ok(fd(7)), Ok(fd(7)), Ok(fd(8)), Ok(fd(9))],
            vec![false; 3],
            3,
        ),
        (
            vec![Ok(FileOperand::from_path("/not-fd")), Ok(fd(-1)), Ok(fd(7))],
            vec![false, false, true],
            1,
        ),
        (
            vec![
                Err(OpError::failed(
                    VfError::errno(None, libc::EIO as u32),
                    Some(IoProgress::Write { bytes_written: 1 }),
                )),
                Ok(fd(1)),
                Ok(fd(2)),
            ],
            vec![false, true, true],
            2,
        ),
        (vec![Ok(fd(3)), Ok(fd(1)), Ok(fd(2))], vec![true; 3], 3),
    ];
    for (response, valid, cleanup_count) in cases {
        let client = FsClient::new(OwnedOpenProbe {
            response: Some(Ok(response)),
            ..Default::default()
        });
        let outcomes = client.vopen(&owned_open_requests()).unwrap();
        assert_eq!(outcomes.len(), 3);
        for (outcome, valid) in outcomes.iter().zip(valid) {
            assert_eq!(outcome.is_ok(), valid);
            if let Err(error) = outcome {
                assert_eq!(error.state(), FailureState::Unknown);
                assert!(error.progress().is_none());
            }
        }
        drop(outcomes);
        client.drain_cleanup().unwrap();
        assert_eq!(
            client.into_inner().unwrap().scalar.close_calls,
            cleanup_count
        );
    }
}
#[test]
fn singular_and_builder_opens_delegate_to_the_same_vector_hook() {
    let backend = OwnedOpenProbe::default();
    let opens = backend.opens.clone();
    let client = FsClient::new(backend);
    client.open("/one").unwrap().close().unwrap();
    client
        .open_with(OpenOp::new("/two", OpenFlags::READ))
        .unwrap()
        .close()
        .unwrap();
    let files = client
        .open_options()
        .read(true)
        .vopen(&["/three", "/four"])
        .unwrap();
    client.close_files(files).unwrap();
    assert_eq!(*opens.lock().unwrap(), [1, 1, 2]);
}

#[test]
fn native_write_outcomes_isolate_rejected_handles_without_copying_or_reprojection() {
    use vfsi_sync::FailureState;
    let backend = WriteBoundaryProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let other = FsClient::new(WriteBoundaryProbe::default());
    let first = fs.open("/first").unwrap();
    let last = fs.open("/last").unwrap();
    let mut closed = fs.open("/closed").unwrap();
    closed.try_close().unwrap();
    let foreign = other.open("/foreign").unwrap();
    let requests = [
        first.write_request_at(0, b"ab"),
        foreign.write_request_at(0, b"foreign"),
        closed.write_request_at(0, b"closed"),
        last.write_request_at(u64::MAX, b"overflow"),
        last.write_request_at(2, b"yz"),
    ];
    let projections = std::cell::Cell::new(0);
    let outcomes = fs
        .vwrite_mapped_native(&requests, |request| {
            projections.set(projections.get() + 1);
            fs.capabilities().unwrap(); // projection must run outside the lock
            *request
        })
        .unwrap();
    assert_eq!(projections.get(), requests.len());
    assert_eq!(outcomes.len(), requests.len());
    for index in [0, 4] {
        assert_eq!(outcomes[index].as_ref().unwrap().bytes_written, 2);
    }
    for (index, path) in [(1, "/foreign"), (2, "/closed"), (3, "/last")] {
        let error = outcomes[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(std::path::Path::new(path)));
        assert!(error.progress().is_none());
    }
    let observed = calls.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].len(), 2);
    for (actual, index) in observed[0].iter().zip([0, 4]) {
        assert_eq!(actual.pointer, requests[index].data().as_ptr() as usize);
        assert_eq!(actual.length, requests[index].data().len());
    }
    drop(observed);
    assert!(fs.vwrite_native(&[]).unwrap().is_empty());
    let rejected = fs.vwrite_native(&requests[1..4]).unwrap();
    assert!(rejected.iter().all(Result::is_err));
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "empty/rejected vectors must not dispatch"
    );
}

#[test]
fn native_write_outcomes_keep_confirmed_prefixes_and_independent_successes() {
    use vfsi_sync::{FailureState, IoProgress, OpError};
    for state in [FailureState::Failed, FailureState::Unknown] {
        let cause = VfError::errno(7, libc::ENOSPC as u32);
        let progress = Some(IoProgress::Write { bytes_written: 2 });
        let error = if state == FailureState::Failed {
            OpError::failed(cause, progress)
        } else {
            OpError::unknown(cause, progress)
        };
        let backend = WriteBoundaryProbe {
            write_outcomes: Some(vec![Ok(1), Err(error), Ok(1)]),
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let file = fs.open("/file").unwrap();
        let results = fs
            .vwrite_native(&[
                file.write_request_at(0, b"a"),
                file.write_request_at(10, b"four"),
                file.write_request_at(20, b"z"),
            ])
            .unwrap();
        assert_eq!(results[0].as_ref().unwrap().bytes_written, 1);
        assert_eq!(results[2].as_ref().unwrap().bytes_written, 1);
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.state(), state);
        assert_eq!(error.cause().raw_code(), libc::ENOSPC as u32);
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().operation(), Some("vwrite_native"));
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 2 })
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "errors never replay siblings"
        );
    }
}

#[test]
fn native_write_outcomes_quarantine_only_invalid_progress_or_missing_slots() {
    use vfsi_sync::{FailureState, IoProgress, OpError};
    for malformed in [
        IoProgress::Write { bytes_written: 5 },
        IoProgress::Read(vfsi_core::api::ReadResult::owned(10, b"x".to_vec(), false)),
    ] {
        let fs = FsClient::new(WriteBoundaryProbe {
            write_outcomes: Some(vec![
                Ok(1),
                Err(OpError::failed(
                    VfError::errno(None, libc::EIO as u32),
                    Some(malformed),
                )),
                Ok(1),
            ]),
            ..Default::default()
        });
        let file = fs.open("/file").unwrap();
        let results = fs
            .vwrite_native(&[
                file.write_request_at(0, b"a"),
                file.write_request_at(10, b"four"),
                file.write_request_at(20, b"z"),
            ])
            .unwrap();
        assert!(results[0].is_ok());
        assert!(results[2].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(
            error.cause().transport_kind(),
            Some(vfsi_sync::TransportKind::InvalidReply)
        );
        assert!(error.progress().is_none());
    }
    for count in [1, 4] {
        let fs = FsClient::new(WriteBoundaryProbe {
            write_outcomes: Some((0..count).map(|_| Ok(1)).collect()),
            ..Default::default()
        });
        let file = fs.open("/file").unwrap();
        let results = fs
            .vwrite_native(&[
                file.write_request_at(0, b"a"),
                file.write_request_at(10, b"b"),
                file.write_request_at(20, b"z"),
            ])
            .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].is_ok(), count == 1);
        for result in results.iter().skip(usize::from(count == 1)) {
            assert_eq!(result.as_ref().unwrap_err().state(), FailureState::Unknown);
        }
    }
}

#[test]
fn complete_write_outcomes_keep_independent_waves_and_resolve_only_safe_dependencies() {
    use vfsi_sync::{FailureState, IoProgress, OpError};
    for uncertain in [false, true] {
        let cause = VfError::errno(None, libc::ENOSPC as u32);
        let prefix = Some(IoProgress::Write { bytes_written: 1 });
        let error = if uncertain {
            OpError::unknown(cause, prefix)
        } else {
            OpError::failed(cause, prefix)
        };
        let backend = WriteBoundaryProbe {
            write_outcomes: Some(vec![Err(error), Ok(2), Ok(2)]),
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let a = fs.open("/a").unwrap();
        let b = fs.open("/b").unwrap();
        let c = fs.open("/c").unwrap();
        let data = [b"AAAA".as_slice(), b"zz", b"BBBBB", b"", b"CC"];
        let results = fs
            .vwrite_all_native(&[
                a.write_request_at(0, data[0]),
                a.write_request_at(0, data[1]),
                b.write_request_at(10, data[2]),
                b.write_request_at(20, data[3]),
                c.write_request_at(0, data[4]),
            ])
            .unwrap();
        assert_eq!(results.len(), 5);
        let error = results[0].as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if uncertain {
                FailureState::Unknown
            } else {
                FailureState::Failed
            }
        );
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 1 })
        );
        assert_eq!(error.cause().index(), None);
        assert_eq!(results[2].as_ref().unwrap().bytes_written, 5);
        assert_eq!(results[3].as_ref().unwrap().bytes_written, 0);
        assert_eq!(results[4].as_ref().unwrap().bytes_written, 2);
        if uncertain {
            let blocked = results[1].as_ref().unwrap_err();
            assert_eq!(blocked.state(), FailureState::NotAttempted);
            assert!(blocked.progress().is_none());
        } else {
            assert_eq!(results[1].as_ref().unwrap().bytes_written, 2);
        }
        let calls = calls.lock().unwrap();
        let widths: Vec<_> = calls.iter().map(Vec::len).collect();
        assert_eq!(
            widths,
            if uncertain {
                vec![3, 1, 1]
            } else {
                vec![3, 2, 1]
            }
        );
        let rejected = data[0].as_ptr() as usize..data[0].as_ptr() as usize + data[0].len();
        assert!(
            calls
                .iter()
                .skip(1)
                .flatten()
                .all(|op| !rejected.contains(&op.pointer)),
            "never replay a failed/uncertain payload suffix"
        );
    }
}

#[test]
fn complete_write_outcomes_accumulate_prior_acknowledgments_on_failure_and_malformed_replies() {
    use vfsi_sync::{FailureState, IoProgress, OpError};
    for case in [
        "failed",
        "unknown",
        "not-attempted",
        "missing",
        "excess",
        "zero",
    ] {
        let cause = VfError::errno(None, libc::ENOSPC as u32);
        let second = match case {
            "failed" => vec![
                Err(OpError::failed(
                    cause,
                    Some(IoProgress::Write { bytes_written: 1 }),
                )),
                Ok(2),
            ],
            "unknown" => vec![
                Err(OpError::unknown(
                    cause,
                    Some(IoProgress::Write { bytes_written: 1 }),
                )),
                Ok(2),
            ],
            "not-attempted" => vec![Err(OpError::not_attempted(cause)), Ok(2)],
            "missing" => vec![Ok(2)],
            "excess" => vec![Ok(2), Ok(2), Ok(1)],
            "zero" => vec![Ok(0), Ok(2)],
            _ => unreachable!(),
        };
        let backend = WriteBoundaryProbe {
            write_waves: std::collections::VecDeque::from([vec![Ok(2), Ok(2)], second]),
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let a = fs.open("/a").unwrap();
        let b = fs.open("/b").unwrap();
        let results = fs
            .vwrite_all_native(&[
                a.write_request_at(0, b"AAAA"),
                b.write_request_at(10, b"BBBB"),
            ])
            .unwrap();
        assert_eq!(results.len(), 2);
        if case == "missing" {
            assert_eq!(results[0].as_ref().unwrap().bytes_written, 4);
        } else {
            let error = results[0].as_ref().unwrap_err();
            assert_eq!(
                error.state(),
                if matches!(case, "unknown" | "excess") {
                    FailureState::Unknown
                } else {
                    FailureState::Failed
                },
                "{case}"
            );
            assert_eq!(
                error.progress(),
                Some(&IoProgress::Write {
                    bytes_written: if matches!(case, "failed" | "unknown") {
                        3
                    } else {
                        2
                    }
                }),
                "{case}"
            );
        }
        if matches!(case, "missing" | "excess") {
            let error = results[1].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::Unknown);
            assert_eq!(
                error.progress(),
                Some(&IoProgress::Write { bytes_written: 2 })
            );
        } else {
            assert_eq!(results[1].as_ref().unwrap().bytes_written, 4);
        }
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "no error or omitted slot may be retried"
        );
    }
}

#[test]
fn complete_write_outcomes_isolate_preflight_rejection_and_validate_empty_inputs() {
    use vfsi_sync::FailureState;
    let backend = WriteBoundaryProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let other = FsClient::new(WriteBoundaryProbe::default());
    let local = fs.open("/local").unwrap();
    let foreign = other.open("/foreign").unwrap();
    let mut closed = fs.open("/closed").unwrap();
    closed.try_close().unwrap();
    let results = fs
        .vwrite_all_native(&[
            local.write_request_at(0, b"abcde"),
            foreign.write_request_at(0, b""),
            closed.write_request_at(0, b""),
            local.write_request_at(u64::MAX, b"bad"),
            local.write_request_at(10, b"xy"),
            local.write_request_at(u64::MAX, b""),
        ])
        .unwrap();
    assert_eq!(results[0].as_ref().unwrap().bytes_written, 5);
    assert_eq!(results[4].as_ref().unwrap().bytes_written, 2);
    let empty = results[5].as_ref().unwrap();
    assert_eq!((empty.offset, empty.bytes_written), (u64::MAX, 0));
    for index in [1, 2, 3] {
        let error = results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [2, 1, 1]
    );
    assert!(fs.vwrite_all_native(&[]).unwrap().is_empty());
    assert!(
        fs.vwrite_all_native(&[local.write_request_at(0, b"")])
            .unwrap()[0]
            .is_ok()
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "validated empty requests need no I/O"
    );
}

#[test]
fn standard_write_errors_preserve_progress_and_never_retry_uncertain_interruption() {
    use vfsi_sync::{FailureState, IoProgress, OpError};
    for (state, count) in [
        (FailureState::Unknown, 0),
        (FailureState::Unknown, 2),
        (FailureState::Failed, 2),
    ] {
        let cause = VfError::errno(None, libc::EINTR as u32);
        let progress = (count != 0).then_some(IoProgress::Write {
            bytes_written: count,
        });
        let error = if state == FailureState::Unknown {
            OpError::unknown(cause, progress)
        } else {
            OpError::failed(cause, progress)
        };
        let backend = WriteBoundaryProbe {
            write_outcomes: Some(vec![Err(error)]),
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let file = fs.open("/file").unwrap();
        let mut io = fs.std_io(&file);
        let error = io.write_all(b"data").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        let outcome = error.get_ref().unwrap().downcast_ref::<OpError>().unwrap();
        assert_eq!(outcome.state(), state);
        assert_eq!(
            outcome.progress(),
            (count != 0)
                .then_some(IoProgress::Write {
                    bytes_written: count
                })
                .as_ref()
        );
        if count == 0 {
            assert_eq!(io.stream_position().unwrap(), 0);
        } else {
            assert!(
                io.stream_position().is_err(),
                "a partial failed append has no resolved cursor"
            );
            io.seek(SeekFrom::Start(2)).unwrap();
            assert_eq!(io.stream_position().unwrap(), 2);
        }
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "standard write_all must not replay this input"
        );
    }
    let backend = WriteBoundaryProbe {
        write_outcomes: Some(vec![Err(OpError::failed(
            VfError::errno(None, libc::EINTR as u32),
            None,
        ))]),
        ..Default::default()
    };
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let file = fs.open("/file").unwrap();
    let mut io = fs.std_io(&file);
    io.write_all(b"data").unwrap();
    assert_eq!(io.stream_position().unwrap(), 4);
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "only a zero-progress definite interruption is retryable"
    );
}

#[test]
fn complete_writes_keep_conservative_alias_order_when_identity_discovery_fails() {
    for prefetch in [false, true] {
        let fs = FsClient::new(ScalarOnly {
            identity_preflight_failure: prefetch,
            identity_ref_failure: !prefetch,
            max_write_once: Some(2),
            ..Default::default()
        });
        let first = fs.open("/first").unwrap();
        let second = fs.open("/second").unwrap();
        let results = fs
            .vwrite_all_native(&[
                first.write_request_at(0, b"AAAA"),
                second.write_request_at(2, b"BBBB"),
            ])
            .unwrap();
        assert!(results.iter().all(Result::is_ok));
        first.close().unwrap();
        second.close().unwrap();
        assert_eq!(
            fs.into_inner().unwrap().data,
            b"AABBBB",
            "unavailable identities must not be treated as proof of disjoint objects"
        );
    }
}

#[derive(Default)]
struct NamespaceProbe {
    scalar: ScalarOnly,
    calls: Arc<AtomicUsize>,
    shape: u8,
}
handle_contract!(
    NamespaceProbe;
    fn vsetattrs_impl(
        &mut self,
        updates: &[SetAttrsOp<Target<'_, FileOperand>>],
    ) -> vfsi_sync::VecResult<()> {
        self.respond(updates.len(), || ())
    }
);
impl VectorBackend for NamespaceProbe {
    fn remove_paths_with_options_impl(
        &mut self,
        paths: &[&std::path::Path],
        _: vfsi_core::api::RemoveMode,
        _: vfsi_sync::RemoveOptions,
    ) -> vfsi_sync::VecResult<()> {
        self.respond(paths.len(), || ())
    }

    fn vcopy_impl(
        &mut self,
        pairs: &[vfsi_sync::ExtentPair],
        _: vfsi_core::CopyOptions,
    ) -> vfsi_sync::VecResult<()> {
        self.respond(pairs.len(), || ())
    }

    fn vrename_with_options_impl(
        &mut self,
        pairs: &[(FileOperand, FileOperand)],
        _: vfsi_core::api::RenameMode,
    ) -> vfsi_sync::VecResult<()> {
        self.respond(pairs.len(), || ())
    }

    fn vopen_raw_impl(
        &mut self,
        paths: &[&std::path::Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_sync::VecResult<FileOperand> {
        self.scalar.vopen_raw_impl(paths, flags, modes)
    }
    fn vread_impl(&mut self, requests: &[ReadOp]) -> vfsi_sync::VecResult<ReadResult> {
        self.scalar.vread_impl(requests)
    }
    fn vreadlink_impl(&mut self, paths: &[&std::path::Path]) -> vfsi_sync::VecResult<Vec<u8>> {
        self.respond(paths.len(), || b"../raw-\xff".to_vec())
    }
    fn vmkdir_impl(&mut self, dirs: &[VfAttrs]) -> vfsi_sync::VecResult<()> {
        self.respond(dirs.len(), || ())
    }
    fn vsymlink_impl(
        &mut self,
        targets: &[&std::path::Path],
        paths: &[&std::path::Path],
    ) -> vfsi_sync::VecResult<()> {
        if targets.len() != paths.len() {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        self.respond(paths.len(), || ())
    }
    fn vhardlink_impl(
        &mut self,
        sources: &[&std::path::Path],
        paths: &[&std::path::Path],
    ) -> vfsi_sync::VecResult<()> {
        if sources.len() != paths.len() {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        self.respond(paths.len(), || ())
    }
}

impl NamespaceProbe {
    fn respond<T>(
        &mut self,
        count: usize,
        mut success: impl FnMut() -> T,
    ) -> vfsi_sync::VecResult<T> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.shape == 4 {
            return Err(VfError::unsupported(None));
        }
        let mut results: Vec<_> = (0..count)
            .map(|index| {
                if index == 1 {
                    Err(vfsi_sync::OpError::failed(
                        VfError::errno(99, libc::EACCES as u32),
                        (self.shape == 3)
                            .then_some(vfsi_sync::IoProgress::Write { bytes_written: 1 }),
                    ))
                } else {
                    Ok(success())
                }
            })
            .collect();
        if self.shape == 1 {
            results.truncate(1);
        }
        if self.shape == 2 {
            results.push(Ok(success()));
        }
        Ok(results)
    }
}

#[test]
fn readlink_results_preserve_siblings_validate_shape_and_never_replay() {
    use std::os::unix::ffi::OsStrExt;
    use vfsi_sync::FailureState;
    for shape in 0..5 {
        let backend = NamespaceProbe {
            shape,
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        assert!(fs.vreadlink::<&str>(&[]).unwrap().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let result = fs.vreadlink(&["/first", "/bad", "/last"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if shape == 4 {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
            continue;
        }
        let results = result.unwrap();
        assert_eq!(results.len(), 3);
        if shape != 2 {
            assert_eq!(
                results[0].as_ref().unwrap().as_os_str().as_bytes(),
                b"../raw-\xff"
            );
        }
        if shape == 0 || shape == 3 {
            assert!(results[2].is_ok());
            let error = results[1].as_ref().unwrap_err();
            assert_eq!(
                error.state(),
                if shape == 0 {
                    FailureState::Failed
                } else {
                    FailureState::Unknown
                }
            );
            assert_eq!(error.cause().index(), None);
            assert_eq!(error.cause().path(), Some(std::path::Path::new("/bad")));
            assert!(error.progress().is_none());
        } else {
            let start = usize::from(shape == 1);
            assert!(
                results[start..]
                    .iter()
                    .all(|result| result.as_ref().unwrap_err().state() == FailureState::Unknown)
            );
        }
    }
    let backend = NamespaceProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let error = fs.readlinks_all(&["/first", "/bad", "/last"]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.raw_code(), libc::EACCES as u32);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs.read_link("/single").unwrap().as_os_str().as_bytes(),
        b"../raw-\xff"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn link_results_keep_siblings_and_strict_helpers_dispatch_once() {
    use vfsi_sync::FailureState;
    let pairs = [
        ("/source", "/first"),
        ("/source", "/bad"),
        ("/source", "/last"),
    ];
    for hard in [false, true] {
        for shape in 0..5 {
            let backend = NamespaceProbe {
                shape,
                ..Default::default()
            };
            let calls = backend.calls.clone();
            let fs = FsClient::new(backend);
            let empty = if hard {
                fs.vhardlink::<&str, &str>(&[])
            } else {
                fs.vsymlink::<&str, &str>(&[])
            };
            assert!(empty.unwrap().is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let results = if hard {
                fs.vhardlink(&pairs)
            } else {
                fs.vsymlink(&pairs)
            };
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            if shape == 4 {
                assert_eq!(results.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
                continue;
            }
            let results = results.unwrap();
            assert_eq!(results.len(), 3);
            if shape != 2 {
                assert!(results[0].is_ok());
            }
            if shape == 0 || shape == 3 {
                assert!(results[2].is_ok());
                let error = results[1].as_ref().unwrap_err();
                assert_eq!(
                    error.state(),
                    if shape == 0 {
                        FailureState::Failed
                    } else {
                        FailureState::Unknown
                    }
                );
                assert_eq!(error.cause().path(), Some(std::path::Path::new("/bad")));
                assert_eq!(error.cause().index(), None);
                assert!(error.progress().is_none());
            } else {
                let start = usize::from(shape == 1);
                assert!(results[start..].iter().all(|result| result.as_ref().unwrap_err().state() == FailureState::Unknown));
            }
        }
    }
    let backend = NamespaceProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    for error in [
        fs.symlinks_all(&pairs).unwrap_err(),
        fs.hardlinks_all(&pairs).unwrap_err(),
    ] {
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.raw_code(), libc::EACCES as u32);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    fs.symlink("/source", "/single").unwrap();
    fs.hard_link("/source", "/single-hard").unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[test]
fn directory_results_isolate_duplicates_and_preserve_shape_and_context() {
    use vfsi_sync::{FailureState, MkDirOp};
    let inputs = [
        MkDirOp::new("/first", 0o700),
        MkDirOp::new("/bad", 0o750),
        MkDirOp::new("/last", 0o777),
    ];
    for shape in 0..5 {
        let backend = NamespaceProbe {
            shape,
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        assert!(fs.vmkdir::<&str>(&[]).unwrap().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let results = fs.vmkdir(&inputs);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if shape == 4 {
            assert_eq!(results.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
            continue;
        }
        let results = results.unwrap();
        assert_eq!(results.len(), 3);
        if shape != 2 {
            assert!(results[0].is_ok());
        }
        if shape == 0 || shape == 3 {
            assert!(results[2].is_ok());
            let error = results[1].as_ref().unwrap_err();
            assert_eq!(
                error.state(),
                if shape == 0 {
                    FailureState::Failed
                } else {
                    FailureState::Unknown
                }
            );
            assert_eq!(error.cause().path(), Some(std::path::Path::new("/bad")));
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        } else {
            assert!(
                results[usize::from(shape == 1)..]
                    .iter()
                    .all(|result| result.as_ref().unwrap_err().state() == FailureState::Unknown)
            );
        }
    }
    for shape in [0, 4] {
        let backend = NamespaceProbe {
            shape,
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let inputs = [
            MkDirOp::new("/first", 0o700),
            MkDirOp::new("/first", 0o750),
            MkDirOp::new("/bad", 0o777),
            MkDirOp::new("/last", 0o755),
        ];
        let results = fs.vmkdir(&inputs).unwrap();
        assert_eq!(results.len(), 4);
        let duplicate = results[1].as_ref().unwrap_err();
        assert_eq!(duplicate.state(), FailureState::NotAttempted);
        assert_eq!(duplicate.cause().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            duplicate.cause().path(),
            Some(std::path::Path::new("/first"))
        );
        if shape == 0 {
            assert!(results[0].is_ok() && results[3].is_ok());
            assert_eq!(
                results[2].as_ref().unwrap_err().state(),
                FailureState::Failed
            );
            assert_eq!(
                results[2].as_ref().unwrap_err().cause().path(),
                Some(std::path::Path::new("/bad"))
            );
        } else {
            for index in [0, 2, 3] {
                let error = results[index].as_ref().unwrap_err();
                assert_eq!(error.state(), FailureState::NotAttempted);
                assert_eq!(error.cause().kind(), std::io::ErrorKind::Unsupported);
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    let backend = NamespaceProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    assert_eq!(fs.mkdirs_all(&inputs).unwrap_err().index(), Some(1));
    fs.create_dir("/single").unwrap();
    fs.create_dir_with_mode("/mode", 0o700).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn setattrs_results_validate_shapes_project_once_and_preserve_nonprefix_successes() {
    use vfsi_core::FailureState;
    for shape in 0..5 {
        let backend = NamespaceProbe {
            shape,
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        assert!(fs.vsetattrs::<&str>(&[]).unwrap().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let updates = [
            SetAttrsOp::new("/first").len(1),
            SetAttrsOp::new("/bad").len(2),
            SetAttrsOp::new("/last").len(3),
        ];
        let projected = std::cell::Cell::new(0);
        let outcomes = fs
            .vsetattrs_projected_native(&updates, |path| {
                projected.set(projected.get() + 1);
                fs.capabilities().unwrap(); // Projection must run outside the backend lock.
                Target::Path(std::path::Path::new(*path))
            })
            .unwrap();
        assert_eq!(projected.get(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcomes.len(), 3);
        for (index, outcome) in outcomes.iter().enumerate() {
            let expected = match shape {
                0 if index == 1 => Some(FailureState::Failed),
                0 => None,
                1 if index == 0 => None,
                1 | 2 => Some(FailureState::Unknown),
                3 if index == 1 => Some(FailureState::Unknown),
                3 => None,
                4 => Some(FailureState::NotAttempted),
                _ => unreachable!(),
            };
            if let Some(state) = expected {
                let error = outcome.as_ref().unwrap_err();
                assert_eq!(error.state(), state);
                assert_eq!(error.cause().index(), None);
                assert_eq!(
                    error.cause().path(),
                    Some(std::path::Path::new(*updates[index].target()))
                );
                assert!(error.progress().is_none());
            } else {
                assert!(outcome.is_ok());
            }
        }
    }
    let backend = NamespaceProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let results = fs
        .vsetattrs(&[
            SetAttrsOp::new("/first").len(1),
            SetAttrsOp::new("/invalid").uid(u32::MAX),
            SetAttrsOp::new("/bad").len(2),
            SetAttrsOp::new("/last").len(3),
        ])
        .unwrap();
    assert!(results[0].is_ok() && results[3].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(
        results[2].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let error = fs
        .setattrs_all(&[
            SetAttrsOp::new("/first").len(1),
            SetAttrsOp::new("/bad").len(2),
            SetAttrsOp::new("/last").len(3),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    fs.truncate("/singular", 1).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn rename_results_preserve_siblings_validate_shape_and_dispatch_once() {
    use vfsi_core::{FailureState, api::RenameMode};
    for shape in 0..5 {
        let backend = NamespaceProbe {
            shape,
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        assert!(
            fs.vrename::<&str, &str>(&[], RenameMode::Replace)
                .unwrap()
                .is_empty()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let result = fs.vrename(
            &[("/first", "/one"), ("/bad", "/two"), ("/last", "/three")],
            RenameMode::Replace,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if shape == 4 {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
            continue;
        }
        let results = result.unwrap();
        assert_eq!(results.len(), 3);
        if shape == 0 || shape == 3 {
            assert!(results[0].is_ok() && results[2].is_ok());
            let error = results[1].as_ref().unwrap_err();
            assert_eq!(
                error.state(),
                if shape == 0 {
                    FailureState::Failed
                } else {
                    FailureState::Unknown
                }
            );
            assert_eq!(error.cause().index(), None);
            assert_eq!(error.cause().path(), Some(std::path::Path::new("/bad")));
            assert!(error.progress().is_none());
        } else {
            if shape == 1 {
                assert!(results[0].is_ok());
            }
            assert!(
                results[usize::from(shape == 1)..]
                    .iter()
                    .all(|result| result.as_ref().unwrap_err().state() == FailureState::Unknown)
            );
        }
    }
    let backend = NamespaceProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let error = fs
        .renames_all(
            &[("/first", "/one"), ("/bad", "/two"), ("/last", "/three")],
            RenameMode::Replace,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    fs.rename("/single", "/moved").unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn rename_projects_each_path_once_before_backend_locking() {
    use std::cell::Cell;
    use vfsi_core::api::RenameMode;
    struct CountedPath<'a> {
        calls: &'a Cell<usize>,
        client: &'a FsClient<NamespaceProbe>,
        name: &'static str,
    }
    impl AsRef<std::path::Path> for CountedPath<'_> {
        fn as_ref(&self) -> &std::path::Path {
            self.calls.set(self.calls.get() + 1);
            self.client.capabilities().unwrap();
            std::path::Path::new(self.name)
        }
    }
    let backend = NamespaceProbe::default();
    let batches = backend.calls.clone();
    let fs = FsClient::new(backend);
    let calls = Cell::new(0);
    let path = |name| CountedPath {
        calls: &calls,
        client: &fs,
        name,
    };
    let pairs = [
        (path("/first"), path("/one")),
        (path("/bad"), path("/two")),
        (path("/last"), path("/three")),
    ];
    let results = fs.vrename(&pairs, RenameMode::Replace).unwrap();
    assert!(results[0].is_ok() && results[2].is_ok());
    assert!(results[1].is_err());
    assert_eq!(calls.get(), 6);
    assert_eq!(batches.load(Ordering::SeqCst), 1);
}

#[test]
fn copy_results_preserve_siblings_validate_shape_and_dispatch_once() {
    use vfsi_core::{FailureState, api::CopyOptions};
    for shape in 0..5 {
        let backend = NamespaceProbe {
            shape,
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        assert!(
            fs.vcopy::<&str, &str>(&[], CopyOptions::new())
                .unwrap()
                .is_empty()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let result = fs.vcopy(
            &[("/first", "/one"), ("/bad", "/two"), ("/last", "/three")],
            CopyOptions::new(),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if shape == 4 {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
            continue;
        }
        let results = result.unwrap();
        assert_eq!(results.len(), 3);
        if shape == 0 || shape == 3 {
            assert!(results[0].is_ok() && results[2].is_ok());
            let error = results[1].as_ref().unwrap_err();
            assert_eq!(
                error.state(),
                if shape == 0 {
                    FailureState::Failed
                } else {
                    FailureState::Unknown
                }
            );
            assert_eq!(error.cause().index(), None);
            assert_eq!(error.cause().path(), Some(std::path::Path::new("/two")));
            assert!(error.progress().is_none());
        } else {
            if shape == 1 {
                assert!(results[0].is_ok());
            }
            assert!(
                results[usize::from(shape == 1)..]
                    .iter()
                    .all(|result| result.as_ref().unwrap_err().state() == FailureState::Unknown)
            );
        }
    }
    let backend = NamespaceProbe::default();
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let error = fs
        .copies_all(
            &[("/first", "/one"), ("/bad", "/two"), ("/last", "/three")],
            CopyOptions::new(),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    fs.copy("/single", "/moved").unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn copy_projects_each_path_once_before_backend_locking() {
    use std::cell::Cell;
    use vfsi_core::api::CopyOptions;
    struct CountedPath<'a> {
        calls: &'a Cell<usize>,
        client: &'a FsClient<NamespaceProbe>,
        name: &'static str,
    }
    impl AsRef<std::path::Path> for CountedPath<'_> {
        fn as_ref(&self) -> &std::path::Path {
            self.calls.set(self.calls.get() + 1);
            self.client.capabilities().unwrap();
            std::path::Path::new(self.name)
        }
    }
    let backend = NamespaceProbe::default();
    let batches = backend.calls.clone();
    let fs = FsClient::new(backend);
    let calls = Cell::new(0);
    let path = |name| CountedPath {
        calls: &calls,
        client: &fs,
        name,
    };
    let pairs = [
        (path("/first"), path("/one")),
        (path("/bad"), path("/two")),
        (path("/last"), path("/three")),
    ];
    let results = fs.vcopy(&pairs, CopyOptions::new()).unwrap();
    assert!(results[0].is_ok() && results[2].is_ok());
    assert!(results[1].is_err());
    assert_eq!(calls.get(), 6);
    assert_eq!(batches.load(Ordering::SeqCst), 1);
}

#[test]
fn recursive_removal_requires_successful_root_metadata_before_mutation() {
    let mut backend = AttrsBackend {
        metadata_failure: Some(0),
        ..Default::default()
    };
    let error = vfsi_core::internal::collect_vec_result(
        vfsi_sync::backend::helpers::remove_paths_impl_default(
            &mut backend,
            &[std::path::Path::new("/protected")],
            true,
        ),
        1,
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(error.index(), Some(0));
    assert_eq!(
        backend.removals, 0,
        "failed metadata must never fall through to REMOVE"
    );
}

#[test]
fn removal_results_validate_shapes_and_preserve_siblings_in_one_dispatch() {
    use vfsi_core::{
        FailureState,
        api::{RemoveMode, RemoveOptions},
    };
    for mode in [RemoveMode::Entry, RemoveMode::Tree, RemoveMode::Contents] {
        for shape in 0..5 {
            let backend = NamespaceProbe {
                shape,
                ..Default::default()
            };
            let calls = backend.calls.clone();
            let fs = FsClient::new(backend);
            assert!(
                fs.vremove::<&str>(&[], mode, RemoveOptions::default())
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let results = fs.vremove(&["/first", "/bad", "/last"], mode, RemoveOptions::default());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            if shape == 4 {
                assert_eq!(results.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
                continue;
            }
            let results = results.unwrap();
            assert_eq!(results.len(), 3);
            if shape != 2 {
                assert!(results[0].is_ok());
            }
            if shape == 0 || shape == 3 {
                assert!(results[2].is_ok());
                let error = results[1].as_ref().unwrap_err();
                assert_eq!(
                    error.state(),
                    if shape == 0 {
                        FailureState::Failed
                    } else {
                        FailureState::Unknown
                    }
                );
                assert_eq!(error.cause().index(), None);
                assert_eq!(error.cause().path(), Some(std::path::Path::new("/bad")));
                assert!(error.progress().is_none());
            } else {
                let start = usize::from(shape == 1);
                assert!(
                    results[start..]
                        .iter()
                        .all(|r| r.as_ref().unwrap_err().state() == FailureState::Unknown)
                );
            }
        }
        let backend = NamespaceProbe::default();
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let error = fs
            .removes_all(&["/first", "/bad", "/last"], mode, RemoveOptions::default())
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.raw_code(), libc::EACCES as u32);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn removal_projects_each_path_once_before_backend_locking() {
    use std::cell::Cell;
    use vfsi_core::api::{RemoveMode, RemoveOptions};
    struct CountedPath<'a> {
        calls: &'a Cell<usize>,
        client: &'a FsClient<NamespaceProbe>,
        name: &'static str,
    }
    impl AsRef<std::path::Path> for CountedPath<'_> {
        fn as_ref(&self) -> &std::path::Path {
            self.calls.set(self.calls.get() + 1);
            self.client.capabilities().unwrap();
            std::path::Path::new(self.name)
        }
    }
    let backend = NamespaceProbe::default();
    let batches = backend.calls.clone();
    let fs = FsClient::new(backend);
    let calls = Cell::new(0);
    let path = |name| CountedPath {
        calls: &calls,
        client: &fs,
        name,
    };
    let paths = [path("/first"), path("/bad"), path("/last")];
    let results = fs
        .vremove(&paths, RemoveMode::Tree, RemoveOptions::default())
        .unwrap();
    assert!(results[0].is_ok() && results[2].is_ok());
    assert!(results[1].is_err());
    assert_eq!(calls.get(), 3);
    assert_eq!(batches.load(Ordering::SeqCst), 1);
}

#[test]
fn directory_open_results_validate_owned_resources_and_preserve_known_siblings() {
    use vfsi_sync::FailureState;
    for shape in 0..7 {
        let backend = DirectoryProbe {
            open_shape: shape,
            ..Default::default()
        };
        let opens = backend.opens.clone();
        let closed = backend.closed.clone();
        let fs = FsClient::new(backend);
        assert!(fs.vopen_dirs::<&str>(&[]).unwrap().is_empty());
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        let results = fs.vopen_dirs(&["/0", "/1", "/2"]);
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        if shape == 4 {
            assert_eq!(results.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
            assert!(closed.lock().unwrap().is_empty());
            continue;
        }
        let results = results.unwrap();
        assert_eq!(results.len(), 3);
        match shape {
            0 => assert!(results.iter().all(Result::is_ok)),
            1 => {
                assert!(results[0].is_ok());
                assert!(
                    results[1..]
                        .iter()
                        .all(|r| r.as_ref().unwrap_err().state() == FailureState::Unknown)
                );
            }
            2 => assert!(
                results
                    .iter()
                    .all(|r| r.as_ref().unwrap_err().state() == FailureState::Unknown)
            ),
            3 => {
                assert!(results[0].is_ok() && results[2].is_ok());
                let error = results[1].as_ref().unwrap_err();
                assert_eq!(error.state(), FailureState::Unknown);
                assert!(error.progress().is_none());
                assert_eq!(error.cause().path(), Some(std::path::Path::new("/1")));
            }
            5 => {
                assert_eq!(
                    results[0].as_ref().unwrap_err().state(),
                    FailureState::Unknown
                );
                assert!(results[1].is_ok());
                assert_eq!(
                    results[2].as_ref().unwrap_err().state(),
                    FailureState::Unknown
                );
            }
            6 => {
                assert!(results[0].is_ok() && results[1].is_ok());
                assert_eq!(
                    results[2].as_ref().unwrap_err().state(),
                    FailureState::Unknown
                );
            }
            _ => unreachable!(),
        }
        drop(results);
        fs.drain_cleanup().unwrap();
        let mut closed = closed.lock().unwrap().clone();
        closed.sort_unstable();
        assert_eq!(
            closed,
            match shape {
                0 => vec![0, 1, 2],
                1 => vec![0],
                2 => vec![0, 1, 2, 3],
                3 => vec![0, 2],
                5 | 6 => vec![0, 1],
                _ => unreachable!(),
            },
            "every distinct valid owner closes exactly once: shape {shape}"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1, "no replay");
    }

    let backend = DirectoryProbe::default();
    let opens = backend.opens.clone();
    let closed = backend.closed.clone();
    let fs = FsClient::new(backend);
    let results = fs.vopen_dirs(&["/0", "/missing", "/2"]).unwrap();
    assert!(results[0].is_ok() && results[2].is_ok());
    let error = results[1].as_ref().unwrap_err();
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    drop(results);
    fs.drain_cleanup().unwrap();
    let mut actual = closed.lock().unwrap().clone();
    actual.sort_unstable();
    assert_eq!(actual, [0, 2]);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
}

#[test]
fn directory_open_projects_each_path_once_before_backend_locking() {
    use std::cell::Cell;
    struct CountedPath<'a> {
        calls: &'a Cell<usize>,
        client: &'a FsClient<DirectoryProbe>,
        name: &'static str,
    }
    impl AsRef<std::path::Path> for CountedPath<'_> {
        fn as_ref(&self) -> &std::path::Path {
            self.calls.set(self.calls.get() + 1);
            self.client.capabilities().unwrap();
            std::path::Path::new(self.name)
        }
    }
    let backend = DirectoryProbe::default();
    let batches = backend.opens.clone();
    let fs = FsClient::new(backend);
    let calls = Cell::new(0);
    let path = |name| CountedPath {
        calls: &calls,
        client: &fs,
        name,
    };
    let paths = [path("/0"), path("/missing"), path("/2")];
    let results = fs.vopen_dirs(&paths).unwrap();
    assert!(results[0].is_ok() && results[2].is_ok());
    assert!(results[1].is_err());
    assert_eq!(calls.get(), 3);
    assert_eq!(batches.load(Ordering::SeqCst), 1);
}

#[test]
fn stream_read_error_prefixes_are_delivered_once_without_replaying_siblings() {
    for stop in [false, true] {
        let client = FsClient::new(StreamBackend {
            scalar: ScalarOnly {
                data: b"abcde".to_vec(),
                ..Default::default()
            },
            read_error: Some(VfError::transport(1, "lost read suffix")),
            read_error_prefix: true,
            ..Default::default()
        });
        let mut received = [Vec::new(), Vec::new(), Vec::new()];
        let outcomes = client
            .vstream(
                &["/first", "/partial", "/last"],
                vfsi_sync::StreamOptions::new()
                    .with_chunk_size(std::num::NonZeroUsize::new(2).unwrap()),
                |index, offset, bytes| {
                    assert_eq!(offset, received[index].len() as u64);
                    received[index].extend_from_slice(bytes);
                    Ok(if stop && index == 1 {
                        std::ops::ControlFlow::Break(())
                    } else {
                        std::ops::ControlFlow::Continue(())
                    })
                },
            )
            .unwrap();
        assert_eq!(outcomes.len(), 3);
        assert_eq!(
            outcomes[0].as_ref().unwrap(),
            &vfsi_sync::StreamCompletion::Complete
        );
        let error = outcomes[1].as_ref().unwrap_err();
        assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
        assert!(error.cause().is_transport());
        assert_eq!(error.cause().index(), None);
        assert_eq!(
            error.progress(),
            Some(&vfsi_core::IoProgress::Stream { next_offset: 1 })
        );
        assert_eq!(received[0], b"abcde");
        assert_eq!(received[1], b"a");
        if stop {
            assert_eq!(
                outcomes[2].as_ref().unwrap(),
                &vfsi_sync::StreamCompletion::Stopped { next_offset: 0 }
            );
            assert!(received[2].is_empty());
        } else {
            assert_eq!(
                outcomes[2].as_ref().unwrap(),
                &vfsi_sync::StreamCompletion::Complete
            );
            assert_eq!(received[2], b"abcde");
        }
        let backend = client.into_inner().unwrap();
        assert_eq!(backend.scalar.close_calls, 3);
        assert_eq!(
            backend
                .batches
                .iter()
                .filter(|(kind, _)| *kind == "open")
                .count(),
            1
        );
        assert_eq!(
            backend
                .batches
                .iter()
                .filter(|(kind, width)| *kind == "read" && *width == 3)
                .count(),
            1
        );
    }
}

#[test]
fn stream_budget_rejection_is_before_io_and_oversized_hints_stay_bounded() {
    let client = FsClient::new(StreamBackend::default())
        .with_limits(vfsi_sync::ResourceLimits::new().with_max_read_bytes(0));
    assert!(
        client
            .vstream::<&str>(&[], vfsi_sync::StreamOptions::new(), |_, _, _| panic!(
                "empty"
            ))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        client
            .vstream(&["/a"], vfsi_sync::StreamOptions::new(), |_, _, _| panic!(
                "rejected before callbacks"
            ))
            .unwrap_err()
            .raw_code(),
        libc::EFBIG as u32
    );
    assert!(client.into_inner().unwrap().batches.is_empty());
    let client = FsClient::new(StreamBackend {
        scalar: ScalarOnly {
            data: b"abcdef".to_vec(),
            ..Default::default()
        },
        ..Default::default()
    })
    .with_limits(vfsi_sync::ResourceLimits::new().with_max_read_bytes(2));
    let mut received = Vec::new();
    let result = client
        .streams_all(
            &["/a"],
            vfsi_sync::StreamOptions::new(),
            |_, offset, bytes| {
                assert_eq!(offset, received.len() as u64);
                assert!(bytes.len() <= 2);
                received.extend_from_slice(bytes);
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap();
    assert_eq!(result, [vfsi_sync::StreamCompletion::Complete]);
    assert_eq!(received, b"abcdef");
    let backend = client.into_inner().unwrap();
    assert_eq!(backend.scalar.close_calls, 1);
    assert_eq!(
        backend.batches,
        [
            ("open", 1),
            ("read", 1),
            ("read", 1),
            ("read", 1),
            ("close", 1)
        ]
    );
}

#[test]
fn standard_single_write_acknowledges_prefix_and_defers_original_error() {
    use vfsi_sync::{FailureState, IoProgress, OpError};
    for append in [false, true] {
        for unknown in [false, true] {
            let cause = VfError::errno(None, libc::EINTR as u32);
            let progress = Some(IoProgress::Write { bytes_written: 2 });
            let error = if unknown {
                OpError::unknown(cause, progress)
            } else {
                OpError::failed(cause, progress)
            };
            let backend = WriteBoundaryProbe {
                write_outcomes: Some(vec![Err(error)]),
                ..Default::default()
            };
            let calls = backend.calls.clone();
            let fs = FsClient::new(backend);
            let file = fs
                .open_options()
                .write(true)
                .append(append)
                .open("/file")
                .unwrap();
            let mut io = fs.std_io(&file);
            assert_eq!(io.write(b"data").unwrap(), 2);
            let error = io.write(b"ta").unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Other);
            let original = error.get_ref().unwrap().downcast_ref::<OpError>().unwrap();
            assert_eq!(
                original.state(),
                if unknown {
                    FailureState::Unknown
                } else {
                    FailureState::Failed
                }
            );
            assert_eq!(
                original.progress(),
                Some(&IoProgress::Write { bytes_written: 2 })
            );
            assert!(io.stream_position().is_err());
            assert!(io.read(&mut [0; 1]).is_err());
            assert!(io.write(b"ta").is_err());
            assert_eq!(
                calls.lock().unwrap().len(),
                1,
                "no suffix replay before reconciliation"
            );
            io.seek(SeekFrom::Start(19)).unwrap();
            assert_eq!(io.stream_position().unwrap(), 19);
            assert_eq!(
                calls.lock().unwrap().len(),
                1,
                "explicit cursor recovery performs no write"
            );
        }
    }
}

#[test]
fn standard_full_prefix_write_reports_deferred_error_on_flush_read_or_seek() {
    use vfsi_sync::{IoProgress, OpError};
    for operation in 0..3 {
        let backend = WriteBoundaryProbe {
            write_outcomes: Some(vec![Err(OpError::unknown(
                VfError::transport(None, "reply lost after acknowledged payload"),
                Some(IoProgress::Write { bytes_written: 4 }),
            ))]),
            ..Default::default()
        };
        let calls = backend.calls.clone();
        let fs = FsClient::new(backend);
        let file = fs.open("/file").unwrap();
        let mut io = fs.std_io(&file);
        assert_eq!(io.write(b"data").unwrap(), 4);
        let error = match operation {
            0 => io.flush().unwrap_err(),
            1 => io.read(&mut [0; 1]).unwrap_err(),
            _ => io.seek(SeekFrom::Start(0)).unwrap_err(),
        };
        let original = error.get_ref().unwrap().downcast_ref::<OpError>().unwrap();
        assert_eq!(
            original.progress(),
            Some(&IoProgress::Write { bytes_written: 4 })
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(io.stream_position().is_err());
        io.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(io.stream_position().unwrap(), 0);
    }
}

#[test]
fn standard_write_all_reports_failure_after_full_acknowledgement_without_retry() {
    use vfsi_sync::{IoProgress, OpError};
    let backend = WriteBoundaryProbe {
        write_outcomes: Some(vec![Err(OpError::unknown(
            VfError::transport(None, "reply lost after acknowledged payload"),
            Some(IoProgress::Write { bytes_written: 4 }),
        ))]),
        ..Default::default()
    };
    let calls = backend.calls.clone();
    let fs = FsClient::new(backend);
    let file = fs.open("/file").unwrap();
    let mut io = fs.std_io(&file);
    let error = io.write_all(b"data").unwrap_err();
    let original = error.get_ref().unwrap().downcast_ref::<OpError>().unwrap();
    assert_eq!(
        original.progress(),
        Some(&IoProgress::Write { bytes_written: 4 })
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(io.stream_position().is_err());
    io.write_all(b"").unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn std_io_read_prefix_errors_are_reported_before_further_io() {
    for errno in [libc::EIO, libc::EINTR] {
        let client = FsClient::new(ReadOutcomes {
            fail_first: true,
            fail_once: true,
            prefix_errno: Some(errno),
            ..Default::default()
        });
        let file = client.open("/file").unwrap();
        let mut io = client.std_io(&file);
        let mut buffer = [99; 3];
        assert_eq!(io.read(&mut buffer).unwrap(), 1);
        assert_eq!(buffer, [b'p', 99, 99]);
        let error = io.read(&mut [0; 1]).unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(io.stream_position().unwrap(), 1);
        assert_eq!(io.read(&mut [0; 1]).unwrap(), 0);
        drop(io);
        file.close().unwrap();
        assert_eq!(client.into_inner().unwrap().calls, 2);
    }
}

#[test]
fn std_io_read_collectors_retain_prefix_and_report_its_failure_even_at_eof() {
    for eof in [false, true] {
        for failed in [false, true] {
            for errno in [libc::EIO, libc::EINTR] {
                for string in [false, true] {
                    let client = FsClient::new(ReadOutcomes {
                        fail_first: true,
                        fail_once: true,
                        prefix_eof: eof,
                        prefix_errno: Some(errno),
                        prefix_failed: failed,
                        ..Default::default()
                    });
                    let file = client.open("/file").unwrap();
                    let mut io = client.std_io(&file);
                    let error = if string {
                        let mut output = String::from("existing");
                        let error = io.read_to_string(&mut output).unwrap_err();
                        assert_eq!(output, "existingp");
                        error
                    } else {
                        let mut output = b"existing".to_vec();
                        let error = io.read_to_end(&mut output).unwrap_err();
                        assert_eq!(output, b"existingp");
                        error
                    };
                    assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
                    assert_eq!(io.stream_position().unwrap(), 1);
                    drop(io);
                    file.close().unwrap();
                    assert_eq!(client.into_inner().unwrap().calls, 1);
                }
            }
        }
    }
}

#[test]
fn std_io_read_to_string_keeps_valid_text_before_a_later_io_error() {
    for (data, expected) in [
        (b"abcdef".as_slice(), "existingabc"),
        (b"a\xffcdef", "existing"),
    ] {
        let client = FsClient::new(ScalarOnly {
            data: data.to_vec(),
            read_failure_call: Some(2),
            ..Default::default()
        })
        .with_limits(vfsi_sync::ResourceLimits::new().with_max_read_bytes(3));
        let file = client.open("/file").unwrap();
        let mut io = client.std_io(&file);
        let mut output = String::from("existing");
        assert!(io.read_to_string(&mut output).is_err());
        assert_eq!(output, expected);
        assert_eq!(io.stream_position().unwrap(), 3);
    }
}

#[test]
fn cleanup_outcomes_and_bounded_history_preserve_ownership_and_primary_io() {
    let backend = ScalarOnly {
        close_failures_remaining: 100,
        ..Default::default()
    };
    let closes = backend.close_observed.clone();
    let client = FsClient::new(backend);
    drop(client.open("/file").unwrap());
    let outcomes = client.drain_cleanup_outcomes().unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].is_err());
    for _ in 0..39 {
        assert!(client.drain_cleanup().is_err());
    }
    let (errors, dropped) = client.take_cleanup_errors();
    assert_eq!(errors.len(), 32);
    assert_eq!(dropped, 8);
    assert!(errors.iter().all(|e| e.operation() == Some("close")));
    assert!(client.take_cleanup_errors().0.is_empty());
    assert_eq!(closes.load(Ordering::SeqCst), 40);
    // Diagnostics do not relinquish the failed cleanup owner.
    assert!(client.drain_cleanup_outcomes().unwrap()[0].is_err());
    assert_eq!(closes.load(Ordering::SeqCst), 41);
}

#[test]
fn scalar_metadata_defaults_preserve_targets_fields_following_and_failures() {
    use vfsi_sync::backend::helpers::{fstat_impl_default, lstat_impl_default, stat_impl_default};
    let mut backend = AttrsBackend::default();
    let path = std::path::Path::new("/link");
    let handle = FileOperand::from_fd(7);
    let path_operand = FileOperand::from_os_path(path);
    assert_eq!(
        stat_impl_default(&mut backend, path).unwrap().file,
        path_operand
    );
    assert_eq!(
        lstat_impl_default(&mut backend, path).unwrap().file,
        path_operand
    );
    assert_eq!(
        fstat_impl_default(&mut backend, &handle).unwrap().file,
        handle
    );
    assert_eq!(
        backend.queries,
        [
            (true, path_operand.clone(), vfsi_core::AttrMask::stat()),
            (false, path_operand, vfsi_core::AttrMask::stat()),
            (true, handle.clone(), vfsi_core::AttrMask::stat()),
        ]
    );
    backend.metadata_failure = Some(0);
    for error in [
        stat_impl_default(&mut backend, path).unwrap_err(),
        lstat_impl_default(&mut backend, path).unwrap_err(),
        fstat_impl_default(&mut backend, &handle).unwrap_err(),
    ] {
        assert_eq!(error.index(), Some(0));
        assert_eq!(error.raw_code(), libc::EACCES as u32);
        assert_eq!(error.operation(), None);
    }
}
