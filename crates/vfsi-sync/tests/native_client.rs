use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use vfsi_sync::api::ReadOptions;
use vfsi_sync::{
    Backend, Capabilities, DirEntry, DirPageCursor, FileSystem, FsClient, ListDirOptions,
    OpenFlags, OpenOp, ReadIntoResult, ReadOp, ReadResult, SetAttrsOp, Target, VfAttrs, VfError,
    VfFile, VfOffset, VfResult, Vfsi, VfsiExt, WriteOp, WriteResult,
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
            .max_read_bytes(3)
            .stream_chunk_bytes(std::num::NonZeroUsize::new(2).unwrap()),
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
                ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(6))
            )
            .unwrap(),
        b"abcdef"
    );
    let file = client.open("/file").unwrap();
    let before = read_calls.load(Ordering::SeqCst);
    assert!(client.vread_native(&[file.read_request_at(0, 4)]).is_err());
    let mut buffer = [0; 4];
    assert!(
        client
            .vread_into_native(&mut [file.read_request_at_into(0, &mut buffer)])
            .is_err()
    );
    assert_eq!(read_calls.load(Ordering::SeqCst), before);
    let result = client
        .vread_into_with_limit_native(&mut [file.read_request_at_into(0, &mut buffer)], 4)
        .unwrap();
    assert_eq!(result[0].read, 4);
    assert_eq!(&buffer, b"abcd");
    buffer.fill(0xff);
    let before = read_calls.load(Ordering::SeqCst);
    assert!(
        client
            .vread_into_with_limit_native(&mut [file.read_request_at_into(0, &mut buffer)], 3)
            .is_err()
    );
    assert_eq!(read_calls.load(Ordering::SeqCst), before);
    assert_eq!(buffer, [0xff; 4]);
    let mut offsets = Vec::new();
    let result = client
        .read_stream("/file", |offset, bytes| {
            offsets.push((offset, bytes.len()));
            Ok(offset == 0)
        })
        .unwrap();
    assert_eq!(offsets, [(0, 2), (2, 2)]);
    assert_eq!(
        result,
        vfsi_sync::StreamCompletion::Stopped { next_offset: 4 }
    );
    assert_eq!(
        client.read_stream("/file", |_, _| Ok(true)).unwrap(),
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

type Notifications = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;
type MetadataCalls = Arc<Mutex<Vec<(bool, Vec<VfFile>)>>>;
type SyncCalls = Arc<Mutex<Vec<(vfsi_core::api::SyncMode, Vec<VfFile>)>>>;

type AttributeCalls = Arc<Mutex<Vec<(bool, Vec<Option<u64>>)>>>;

#[derive(Default)]
struct ScalarOnly {
    sync_calls: SyncCalls,
    sync_failure: Option<VfError>,
    attrs_calls: AttributeCalls,
    attrs_failure: Option<(usize, VfError)>,
    stats_calls: Arc<AtomicUsize>,
    stats_result_count: Option<usize>,
    stats_error_index: Option<usize>,
    notifications: Notifications,
    blocked_read: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    close_observed: Arc<AtomicUsize>,
    data: Vec<u8>,
    cursor: u64,
    open: bool,
    oversized_read: bool,
    oversized_write_count: bool,
    read_failure: bool,
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
    directory_entries: usize,
    directory_page_sizes: Arc<Mutex<Vec<usize>>>,
    directory_fields: Arc<Mutex<Vec<vfsi_sync::AttrMask>>>,
}

#[test]
fn opened_file_read_to_end_has_explicit_limits_and_cursor_semantics() {
    for limit in [0, 3, 6, 7] {
        let client = FsClient::new(ScalarOnly {
            data: b"abcdef".to_vec(),
            ..Default::default()
        })
        .with_limits(vfsi_sync::ResourceLimits::new().max_read_bytes(1));
        let mut file = client.open("/file").unwrap();
        let result = file.read_to_end_with_limit(limit);
        if limit < 6 {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
            assert_eq!(error.operation(), Some("read_to_end_with_limit"));
            assert_eq!(
                file.seek_native(SeekFrom::Current(0)).unwrap(),
                (limit + 1) as u64
            );
        } else {
            assert_eq!(result.unwrap(), b"abcdef");
            assert_eq!(file.seek_native(SeekFrom::Current(0)).unwrap(), 6);
        }
        file.seek_native(SeekFrom::Start(3)).unwrap();
        assert_eq!(file.read_to_end_with_limit(3).unwrap(), b"def");
        assert_eq!(file.read_to_end_with_limit(0).unwrap(), b"");
        file.try_close().unwrap();
        assert_eq!(
            file.read_to_end_with_limit(0).unwrap_err().err_no(),
            libc::EBADF as u32
        );
    }
}

impl Backend for ScalarOnly {
    fn vopen_impl(&mut self, requests: &[OpenOp]) -> VfResult<Vec<VfFile>> {
        if self.transport_failure {
            return Err(VfError::transport(None, "reply lost"));
        }
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        requests
            .iter()
            .take(limit)
            .map(|request| self.open_impl(request))
            .collect()
    }

    fn vclose_impl(&mut self, files: &[VfFile]) -> VfResult<()> {
        for file in files {
            self.close_impl(file)?;
        }
        Ok(())
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
                file: VfFile::from_os_path(&dir.join(format!("item-{index:04}"))),
                masks,
                mode: 0o100644,
                size: index as u64,
                ..VfAttrs::default()
            })
            .collect();
        Ok((entries, (end < count).then(|| DirPageCursor::new(end))))
    }

    fn vread_impl(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
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
                result.file = VfFile::from_fd(999);
            }
            if self.wrong_result_offset {
                result.offset = result.offset.saturating_add(1);
            }
        }
        Ok(results)
    }

    fn vread_into_impl(
        &mut self,
        requests: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        if requests.len() != buffers.len() {
            return Err(VfError::client(0, libc::EINVAL as u32));
        }
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        let mut results: Vec<ReadIntoResult> = requests
            .iter()
            .zip(buffers.iter_mut())
            .take(limit)
            .enumerate()
            .map(|(index, (request, buffer))| {
                self.read_into_impl(request, buffer)
                    .map_err(|error| error.with_index(index))
            })
            .collect::<VfResult<_>>()?;
        if let Some(result) = results.first_mut() {
            if self.wrong_result_file {
                result.file = VfFile::from_fd(999);
            }
            if self.wrong_result_offset {
                result.offset = result.offset.saturating_add(1);
            }
        }
        Ok(results)
    }

    fn vwrite_impl(&mut self, requests: &[WriteOp<&VfFile, &[u8]>]) -> VfResult<Vec<WriteResult>> {
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        let mut results: Vec<_> = requests
            .iter()
            .take(limit)
            .map(|request| self.write_impl(*request))
            .collect::<VfResult<_>>()?;
        if let Some(result) = results.first_mut() {
            if self.wrong_result_file {
                result.file = VfFile::from_fd(999);
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
struct HandleOnly {
    scalar: ScalarOnly,
}

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
    ($backend:ty) => {
        impl FileSystem for $backend {
            fn open_impl(&mut self, request: &OpenOp) -> VfResult<VfFile> {
                self.scalar.open_impl(request)
            }
            fn open_path_impl(
                &mut self,
                base: vfsi_sync::VfPathBase,
                path: &std::path::Path,
                flags: i32,
                mode: u32,
            ) -> VfResult<VfFile> {
                let _ = (base, flags, mode);
                self.open_impl(&OpenOp::new(path, OpenFlags::READ))
            }
            fn close_impl(&mut self, file: &VfFile) -> VfResult<()> {
                self.scalar.close_impl(file)
            }
            fn vfsync_impl(
                &mut self,
                files: &[VfFile],
                mode: vfsi_core::api::SyncMode,
            ) -> VfResult<()> {
                self.scalar.vfsync_impl(files, mode)
            }
            fn sync_all(&mut self, file: &VfFile) -> VfResult<()> {
                self.scalar.sync_all(file)
            }
            fn sync_data(&mut self, file: &VfFile) -> VfResult<()> {
                self.scalar.sync_data(file)
            }
            fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
                self.scalar.read_impl(request)
            }
            fn write_impl(&mut self, request: WriteOp<&VfFile, &[u8]>) -> VfResult<WriteResult> {
                self.scalar.write_impl(request)
            }
            fn seek_impl(&mut self, file: &VfFile, position: SeekFrom) -> VfResult<u64> {
                self.scalar.seek_impl(file, position)
            }
            fn metadata_impl(
                &mut self,
                target: Target<'_, VfFile>,
                options: vfsi_core::api::AttrsOptions,
            ) -> VfResult<VfAttrs> {
                self.scalar.metadata_impl(target, options)
            }
            fn set_attributes_impl(
                &mut self,
                update: &SetAttrsOp<Target<'_, VfFile>>,
            ) -> VfResult<()> {
                self.scalar.set_attributes_impl(update)
            }
        }
    };
}

handle_contract!(HandleOnly);
handle_contract!(DefaultBackend);
handle_contract!(PagedBackend);

#[derive(Default)]
struct LegacyDirectoryBackend {
    scalar: ScalarOnly,
    calls: usize,
}
handle_contract!(LegacyDirectoryBackend);
impl Backend for LegacyDirectoryBackend {
    fn vread_impl(&mut self, _: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        panic!("unexpected read")
    }
    fn listdir_impl(
        &mut self,
        _: &std::path::Path,
        _: vfsi_sync::AttrMask,
        _: usize,
        _: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        self.calls += 1;
        Ok(Vec::new())
    }
}

#[test]
fn legacy_directory_fallback_fails_closed_without_no_follow_support() {
    let mut backend = LegacyDirectoryBackend::default();
    let dir = std::path::Path::new("/tree");
    let page = backend
        .vlistdir_pages_impl(&[dir], vfsi_sync::AttrMask::MODE, vec![None], 1, 4, true)
        .unwrap();
    assert!(page[0].0.is_empty());
    assert_eq!(backend.calls, 1);
    let error = backend
        .vlistdir_pages_impl(&[dir], vfsi_sync::AttrMask::MODE, vec![None], 1, 4, false)
        .err()
        .expect("legacy listing cannot provide no-follow guarantees");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
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
    failure: Option<VfError>,
    failure_after: usize,
}
handle_contract!(WriteBoundaryProbe);
impl Backend for WriteBoundaryProbe {
    fn vread_impl(&mut self, _: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        Err(VfError::unsupported(0))
    }
    fn vwrite_impl(&mut self, writes: &[WriteOp<&VfFile, &[u8]>]) -> VfResult<Vec<WriteResult>> {
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
                written: op.data().len().min(2),
                stable: true,
            })
            .collect())
    }
}

#[test]
fn shared_write_operations_keep_storage_borrowed_through_dynamic_vector_dispatch() {
    let mut concrete = WriteBoundaryProbe::default();
    let calls = Arc::clone(&concrete.calls);
    let backend: &mut dyn Backend = &mut concrete;
    let storage = [
        WriteOp::from_path("/created", VfOffset::At(0), b"abc".to_vec())
            .with_creation()
            .with_truncate(),
        WriteOp::from_fd(7, VfOffset::Cur, b"def".to_vec()),
    ];
    let writes: Vec<_> = storage.iter().map(vfsi_core::WriteOp::borrowed).collect();
    let results = backend.vwrite_impl(&writes).unwrap();
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
        Some(VfError::client(1, libc::ENOSPC as u32)),
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
        let result = client.vwrite_all_native(&ops);
        let calls = calls.lock().unwrap();
        if let Some(error) = failure {
            let actual = result.unwrap_err();
            assert_eq!(actual.index(), error.index());
            assert_eq!(actual.is_transport(), error.is_transport());
            if error.index().is_some() {
                assert_eq!(actual.path(), Some(std::path::Path::new("/second")));
            }
            assert_eq!(
                calls.len(),
                1,
                "indexed and ambiguous failures must never replay"
            );
        } else {
            assert!(result.unwrap().iter().all(|r| r.written == data.len()));
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
    metadata_calls: MetadataCalls,
    scalar: ScalarOnly,
    calls: Vec<(bool, Vec<VfAttrs>)>,
    failure: Option<(usize, VfError)>,
    queries: Vec<(bool, VfFile, vfsi_core::AttrMask)>,
}
handle_contract!(AttrsBackend);
impl Backend for AttrsBackend {
    fn vread_impl(&mut self, _: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        Err(VfError::unsupported(0))
    }
    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> VfResult<()> {
        self.metadata_calls
            .lock()
            .unwrap()
            .push((true, attrs.iter().map(|a| a.file.clone()).collect()));
        self.queries
            .push((true, attrs[0].file.clone(), attrs[0].masks));
        Ok(())
    }
    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> VfResult<()> {
        self.metadata_calls
            .lock()
            .unwrap()
            .push((false, attrs.iter().map(|a| a.file.clone()).collect()));
        self.queries
            .push((false, attrs[0].file.clone(), attrs[0].masks));
        Ok(())
    }
    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> VfResult<()> {
        self.record_attrs(true, attrs)
    }
    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> VfResult<()> {
        self.record_attrs(false, attrs)
    }
}
impl AttrsBackend {
    fn record_attrs(&mut self, follow: bool, attrs: &[VfAttrs]) -> VfResult<()> {
        self.calls.push((follow, attrs.to_vec()));
        if let Some((call, error)) = &self.failure
            && self.calls.len() == *call
        {
            return Err(error.clone());
        }
        Ok(())
    }
}

#[test]
fn shared_attribute_ops_preserve_all_fields_at_the_raw_backend_boundary() {
    use vfsi_sync::backend_helpers::{native_metadata_impl_default, vsetattrs_typed_default};
    use vfsi_sync::{AttrMask, Permissions};
    let mut concrete = AttrsBackend::default();
    let backend: &mut dyn Backend = &mut concrete;
    let file = VfFile::from_fd(7);
    let op = SetAttrsOp::file(&file)
        .len(0)
        .permissions(Permissions::from_mode(0o640))
        .uid(42)
        .gid(43)
        .accessed(std::time::UNIX_EPOCH + std::time::Duration::new(123, 456))
        .modified(std::time::UNIX_EPOCH - std::time::Duration::new(1, 25))
        .follow_symlinks(false);
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
            .fields(fields)
            .follow_symlinks(false),
    )
    .unwrap();
    assert_eq!(attrs.file, file);
    assert_eq!(attrs.masks, fields);
    native_metadata_impl_default(
        backend,
        Target::Path(std::path::Path::new("/link")),
        vfsi_core::api::AttrsOptions::new().fields(AttrMask::SIZE),
    )
    .unwrap();
    assert_eq!(concrete.calls.len(), 2);
    for (follow, attrs) in &concrete.calls {
        assert!(!follow);
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
            (true, VfFile::from_path("/link"), AttrMask::SIZE),
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
        (false, VfFile::from_path("/link"), AttrMask::MODE)
    );
}

#[test]
fn shared_native_attribute_batches_preflight_and_remap_policy_run_errors() {
    use vfsi_sync::backend_helpers::vsetattrs_typed_default;
    let ops = [
        SetAttrsOp::new(Target::Path(std::path::Path::new("/a"))).len(1),
        SetAttrsOp::new(Target::Path(std::path::Path::new("/b")))
            .len(2)
            .follow_symlinks(false),
        SetAttrsOp::new(Target::Path(std::path::Path::new("/c")))
            .len(3)
            .follow_symlinks(false),
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
        let error = vsetattrs_typed_default(&mut backend, &[ops[0], invalid]).unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.path(), Some(std::path::Path::new("/a")));
        assert!(backend.calls.is_empty());
    }
    for failure in [
        VfError::client(1, libc::ENOENT as u32),
        VfError::client(2, libc::ENOENT as u32),
        VfError::transport(None, "reply lost"),
    ] {
        backend.failure = Some((2, failure.clone()));
        backend.calls.clear();
        let error = vsetattrs_typed_default(&mut backend, &ops).unwrap_err();
        assert_eq!(backend.calls.len(), 2);
        if failure.index() == Some(1) {
            assert_eq!(error.index(), Some(2));
            assert_eq!(error.err_no(), libc::ENOENT as u32);
        } else {
            assert!(error.is_transport());
            assert_eq!(error.index(), None);
        }
    }
}

impl Backend for PagedBackend {
    fn vread_impl(&mut self, _: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
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
                file: VfFile::from_os_path(&dir.join(format!("item-{index}"))),
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
    let backend: &mut dyn Backend = &mut concrete;
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
    let backend: &mut dyn Backend = &mut concrete;
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
                .fields(vfsi_sync::AttrMask::SIZE)
                .max_entries(3),
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

impl Backend for WorkflowOverride {
    fn vread_impl(&mut self, _: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        panic!("specialized workflows must not fall back to generic vector reads")
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
        _: &[VfFile],
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
        _: &[&std::path::Path],
        _: bool,
        _: vfsi_sync::RemoveOptions,
    ) -> vfsi_sync::VfRes {
        self.calls.push("remove");
        Ok(())
    }

    fn vcopy_impl(
        &mut self,
        _: &[vfsi_sync::ExtentPair],
        _: vfsi_sync::CopyOption,
    ) -> vfsi_sync::VfRes {
        self.calls.push("copy");
        Ok(())
    }
}

#[test]
fn shared_workflows_keep_dynamic_backend_overrides_reachable() {
    let mut concrete = WorkflowOverride::default();
    let backend: &mut dyn Backend = &mut concrete;
    let files = [VfFile::from_fd(1)];
    assert_eq!(
        backend.vread_all_impl(&files).unwrap(),
        [b"override".to_vec()]
    );
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
    backend
        .vcopy_impl(
            &[vfsi_sync::ExtentPair::from_os_paths(
                std::path::Path::new("/a"),
                0,
                std::path::Path::new("/b"),
                0,
                None,
            )],
            vfsi_sync::CopyOption::default(),
        )
        .unwrap();
    let (entries, next) = backend
        .read_dir_page_impl(std::path::Path::new("/tree"), None, 1, 4)
        .unwrap();
    assert!(entries.is_empty());
    assert!(next.is_none());
    assert_eq!(
        concrete.calls,
        ["stream", "walk", "remove", "copy", "directory"]
    );
}

impl Backend for DefaultBackend {
    fn vread_impl(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        self.vector_reads += 1;
        requests
            .iter()
            .map(|request| self.read_impl(request))
            .collect()
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
    let backend: &mut dyn Backend = &mut concrete;
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
            .seek_raw_impl(&VfFile::from_fd(0), 0, vfsi_sync::SeekFrom::Set)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    let files = backend
        .vopen_impl(&[OpenOp::new("/file", OpenFlags::READ)])
        .unwrap();
    let data = backend
        .vread_all_with_options_impl(&files, vfsi_sync::ReadAllOptions::new().max_total_bytes(6))
        .unwrap();
    assert_eq!(data, [b"abcdef".to_vec()]);
    assert_eq!(
        backend
            .vread_all_with_options_impl(
                &files,
                vfsi_sync::ReadAllOptions::new().max_total_bytes(3),
            )
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
        .unwrap();
    assert_eq!(buffer, *b"cde");
    assert_eq!(result[0].read, 3);
    let paths = [(VfFile::from_path("/source"), VfFile::from_path("/target"))];
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
        backend
            .vrename_with_options_impl(&paths, vfsi_core::api::RenameOptions::NoReplace)
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
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    backend.sync_all(&files[0]).unwrap();
    backend.close_deferred(&files[0]).unwrap();
    let files = backend
        .vopen_impl(&[OpenOp::new("/file", OpenFlags::READ)])
        .unwrap();
    backend.vclose_impl(&files).unwrap();
    assert!(!concrete.scalar.open);
    assert_eq!(concrete.scalar.close_calls, 2);
    assert!(concrete.vector_reads >= 5);
}

impl FileSystem for ScalarOnly {
    fn vfsync_impl(&mut self, files: &[VfFile], mode: vfsi_core::api::SyncMode) -> VfResult<()> {
        self.sync_calls.lock().unwrap().push((mode, files.to_vec()));
        self.sync_failure.clone().map_or(Ok(()), Err)
    }
    fn vsetattrs_impl(&mut self, updates: &[SetAttrsOp<Target<'_, VfFile>>]) -> VfResult<()> {
        let mut calls = self.attrs_calls.lock().unwrap();
        calls.push((
            updates[0].follows_symlinks(),
            updates.iter().map(|op| op.requested_len()).collect(),
        ));
        if let Some((call, error)) = &self.attrs_failure
            && calls.len() == *call
        {
            return Err(error.clone());
        }
        Ok(())
    }
    fn vstatfs_impl(&mut self, files: &[VfFile]) -> VfResult<Vec<vfsi_sync::FilesystemStats>> {
        self.stats_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(index) = self.stats_error_index {
            return Err(VfError::client(index, libc::ENOENT as u32));
        }
        Ok(vec![
            vfsi_sync::FilesystemStats::default();
            self.stats_result_count.unwrap_or(files.len())
        ])
    }

    fn take_notifications(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        std::mem::take(&mut *self.notifications.lock().unwrap())
    }
    fn open_impl(&mut self, _: &OpenOp) -> VfResult<VfFile> {
        self.open = true;
        Ok(VfFile::from_fd(1))
    }

    fn close_impl(&mut self, _: &VfFile) -> VfResult<()> {
        self.close_calls += 1;
        self.close_observed.fetch_add(1, Ordering::SeqCst);
        if self.close_failures_remaining != 0 {
            self.close_failures_remaining -= 1;
            return Err(VfError::transport(None, "injected close failure"));
        }
        self.open = false;
        Ok(())
    }
    fn sync_data(&mut self, _: &VfFile) -> VfResult<()> {
        Ok(())
    }
    fn sync_all(&mut self, _: &VfFile) -> VfResult<()> {
        Ok(())
    }

    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        if let Some((entered, release)) = self.blocked_read.take() {
            entered.wait();
            release.wait();
        }
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        assert!(!self.direct_into_only, "owned read path must not run");
        if self.read_failure {
            return Err(VfError::failure(0, libc::EACCES as u32));
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
                return Err(VfError::client(0, vfsi_sync::ERR_IO));
            }
            buffer[..result.data.len()].copy_from_slice(&result.data);
            return Ok(ReadIntoResult {
                file: result.file,
                offset: result.offset,
                read: result.data.len(),
                eof: result.eof,
            });
        }
        if request.length != buffer.len() {
            return Err(VfError::client(0, libc::EINVAL as u32));
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
            read: data.len(),
            eof: end == self.data.len(),
        })
    }

    fn write_impl(&mut self, request: WriteOp<&VfFile, &[u8]>) -> VfResult<WriteResult> {
        self.write_calls.fetch_add(1, Ordering::SeqCst);
        if self.oversized_write_count {
            return Ok(WriteResult {
                file: request.file().clone(),
                offset: 0,
                written: request.data().len() + 1,
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
            written: length,
            stable: true,
        })
    }

    fn seek_impl(&mut self, _: &VfFile, position: SeekFrom) -> VfResult<u64> {
        let next = match position {
            SeekFrom::Start(value) => value as i128,
            SeekFrom::Current(value) => self.cursor as i128 + value as i128,
            SeekFrom::End(value) => self.data.len() as i128 + value as i128,
        };
        if next < 0 || next > u64::MAX as i128 {
            return Err(VfError::failure(0, libc::EINVAL as u32));
        }
        self.cursor = next as u64;
        Ok(self.cursor)
    }

    fn metadata_impl(
        &mut self,
        _: Target<'_, VfFile>,
        _: vfsi_core::api::AttrsOptions,
    ) -> VfResult<vfsi_sync::VfAttrs> {
        Err(VfError::unsupported(0))
    }

    fn set_attributes_impl(&mut self, _: &SetAttrsOp<Target<'_, VfFile>>) -> VfResult<()> {
        Err(VfError::unsupported(0))
    }
}

#[test]
fn setattrs_ops_batch_policy_runs_in_order_and_preflight_before_dispatch() {
    use vfsi_core::SetAttrsOp;
    let backend = ScalarOnly::default();
    let calls = Arc::clone(&backend.attrs_calls);
    let fs = FsClient::new(backend);
    fs.vsetattrs::<&str>(&[]).unwrap();
    assert!(calls.lock().unwrap().is_empty());
    fs.vsetattrs(&[
        SetAttrsOp::new("/a").len(1),
        SetAttrsOp::new("/b").len(2),
        SetAttrsOp::new("/c").len(3).follow_symlinks(false),
        SetAttrsOp::new("/d").len(4).follow_symlinks(false),
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
        .vsetattrs(&[
            SetAttrsOp::new("/a").len(6),
            SetAttrsOp::new("/b").uid(u32::MAX).follow_symlinks(false),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EINVAL as u32);
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn setattrs_ops_remap_later_run_failures_without_replaying_or_dispatching_suffix() {
    use vfsi_core::SetAttrsOp;
    for failure in [
        VfError::client(1, libc::ENOENT as u32),
        VfError::client(2, libc::ENOENT as u32),
        VfError::transport(None, "lost attribute reply"),
    ] {
        let backend = ScalarOnly {
            attrs_failure: Some((2, failure.clone())),
            ..Default::default()
        };
        let calls = Arc::clone(&backend.attrs_calls);
        let fs = FsClient::new(backend);
        let error = fs
            .vsetattrs(&[
                SetAttrsOp::new("/a").len(1),
                SetAttrsOp::new("/b").len(2).follow_symlinks(false),
                SetAttrsOp::new("/c").len(3).follow_symlinks(false),
                SetAttrsOp::new("/d").len(4),
            ])
            .unwrap_err();
        assert_eq!(calls.lock().unwrap().len(), 2);
        if failure.index() == Some(1) {
            assert_eq!(error.index(), Some(2));
            assert_eq!(error.err_no(), libc::ENOENT as u32);
            assert_eq!(error.path(), Some(std::path::Path::new("/c")));
        } else {
            assert!(error.is_transport());
            assert_eq!(error.index(), None);
        }
    }
}

#[test]
fn owned_client_accepts_a_scalar_only_backend() {
    // This wrapper implements FileSystem only, not Backend. Keeping the vector
    // probe directly here would fail to guard the narrow handle boundary.
    let client = FsClient::new(HandleOnly::default());
    let mut options = client.open_options();
    options.read(true).write(true).create(true);
    let mut file = options.open("/file").unwrap();
    file.write_all(b"scalar").unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut output = String::new();
    file.read_to_string(&mut output).unwrap();
    assert_eq!(output, "scalar");
    file.close().unwrap();
}

#[test]
fn owned_file_rejects_backend_results_that_violate_io_contracts() {
    let client = FsClient::new(ScalarOnly {
        oversized_read: true,
        ..ScalarOnly::default()
    });
    let mut file = client
        .open_with(OpenOp::new("/file", OpenFlags::READ))
        .unwrap();
    let error = file.read_native(&mut [0; 4]).unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::ERR_IO);
    assert_eq!(error.operation(), Some("read"));
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));

    let client = FsClient::new(ScalarOnly {
        oversized_write_count: true,
        ..ScalarOnly::default()
    });
    let mut file = client
        .open_with(OpenOp::new("/file", OpenFlags::WRITE))
        .unwrap();
    let error = file.write_native(b"data").unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::ERR_IO);
    assert_eq!(error.operation(), Some("write"));
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
}

#[test]
fn native_file_errors_retain_operation_and_path_context() {
    let client = FsClient::new(ScalarOnly {
        read_failure: true,
        ..ScalarOnly::default()
    });
    let file = client.open("/important").unwrap();
    let error = file.read_at(&mut [0; 1], 0).unwrap_err();
    assert_eq!(error.err_no(), libc::EACCES as u32);
    assert_eq!(error.operation(), Some("read"));
    assert_eq!(error.path(), Some(std::path::Path::new("/important")));
}

#[test]
fn convenience_string_errors_retain_operation_and_path_context() {
    let client = FsClient::new(ScalarOnly {
        data: vec![0xff],
        ..ScalarOnly::default()
    });
    let error = client.read_to_string("/not-utf8").unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::ERR_INVAL);
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
            ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(3)),
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
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
                ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(4))
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
                ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(4))
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
        .vopen(&[
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
        .vopen(&[
            OpenOp::new("/first", OpenFlags::READ),
            OpenOp::new("/second", OpenFlags::READ),
        ])
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
    assert!(error.to_string().contains("1 results for 2 requests"));
    assert!(!client.into_inner().unwrap().open);
}

#[test]
fn readv_and_writev_reject_wrong_result_counts() {
    let backend = ScalarOnly::default();
    let vector_result_limit = Arc::clone(&backend.vector_result_limit);
    let client = FsClient::new(backend);
    let files = client
        .vopen(&[
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
        .unwrap_err();
    assert!(read_error.is_transport());
    assert_eq!(read_error.index(), None);

    let write_error = client
        .vwrite_native(&[
            files[0].write_request_at(0, b"a"),
            files[1].write_request_at(0, b"b"),
        ])
        .unwrap_err();
    assert!(write_error.is_transport());
    assert_eq!(write_error.index(), None);

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
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
    assert_eq!(read_calls.load(Ordering::SeqCst), 0);

    let error = client
        .vread_native(&[file.read_request_at(0, vfsi_sync::DEFAULT_READV_MAX_TOTAL_BYTES + 1)])
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(read_calls.load(Ordering::SeqCst), 0);

    let results = client
        .vread_with_limit_native(&[file.read_request_at(0, 3)], 3)
        .unwrap();
    assert_eq!(results[0].data, b"abc");
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
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.written)
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
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.written)
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
        .unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::ERR_IO);
    assert_eq!(error.index(), Some(0));
}

#[test]
fn write_allv_validates_every_request_before_writing_a_prefix() {
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
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(write_calls.load(Ordering::SeqCst), 0);
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
    assert!(client.vclose(&mut files).is_err());
    assert_eq!(files[0].path(), std::path::Path::new("/file"));
    client.vclose(&mut files).unwrap();
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
    let error = file.read_native(&mut [0u8; 1]).unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::ERR_EBADF);
    let error = file.write_native(b"x").unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::ERR_EBADF);
    let error = file.seek_native(SeekFrom::Start(0)).unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::ERR_EBADF);
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
            vfsi_sync::StreamOptions::new().chunk_size(2),
            |_, data| {
                drop(other.take());
                let file = client.open("/nested")?;
                file.close()?;
                received.extend_from_slice(data);
                Ok(true)
            },
        )
        .unwrap();
    assert_eq!(received, b"abcdef");
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
            vfsi_sync::ListDirOptions::new().max_entries(3),
            |_| {
                seen += 1;
                Ok(vfsi_core::api::WalkControl::Continue)
            },
        )
        .unwrap_err();
    assert_eq!(seen, 3);
    assert_eq!(error.err_no(), libc::EFBIG as u32);
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
            vfsi_sync::ListDirOptions::new().fields(vfsi_sync::AttrMask::SIZE),
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
        .unwrap();
    assert_eq!(
        lengths.iter().map(|result| result.read).collect::<Vec<_>>(),
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
    assert_eq!(file.read_at(&mut scalar, 2).unwrap(), 2);
    assert_eq!(&scalar, b"cd");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let results = client
        .vread(
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
            .all(|result| result.is_buffered() && result.read() == 3)
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
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
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
    let mut reading = client.open("/file").unwrap();
    let dropped = client.open("/other").unwrap();
    let reader = std::thread::spawn(move || reading.read_native(&mut [0]).unwrap());
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
fn filesystem_statistics_validate_backend_shape_and_preflight_all_handles() {
    for count in [0, 1, 3] {
        let client = FsClient::new(ScalarOnly {
            stats_result_count: Some(count),
            ..Default::default()
        });
        assert!(client.vstatfs(&["/a", "/b"]).unwrap_err().is_transport());
    }
    let client = FsClient::new(ScalarOnly {
        stats_error_index: Some(2),
        ..Default::default()
    });
    assert!(client.vstatfs(&["/a", "/b"]).unwrap_err().is_transport());
    let backend = ScalarOnly::default();
    let calls = backend.stats_calls.clone();
    let client = FsClient::new(backend);
    client.vstatfs::<&str>(&[]).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut file = client.open("/file").unwrap();
    file.try_close().unwrap();
    let targets = [
        vfsi_sync::Target::Path(std::path::Path::new("/a")),
        vfsi_sync::Target::File(&file),
    ];
    assert_eq!(client.vstatfs(&targets).unwrap_err().index(), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let other = FsClient::new(ScalarOnly::default());
    let foreign = other.open("/foreign").unwrap();
    let targets = [
        vfsi_sync::Target::Path(std::path::Path::new("/a")),
        vfsi_sync::Target::File(&foreign),
    ];
    assert_eq!(client.vstatfs(&targets).unwrap_err().index(), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn portable_handle_queries_and_sync_batch_and_preflight_every_handle() {
    use vfsi_core::api::{AttrsOptions, SyncMode};
    let backend = AttrsBackend::default();
    let queries = Arc::clone(&backend.metadata_calls);
    let syncs = Arc::clone(&backend.scalar.sync_calls);
    let fs = FsClient::new(backend);
    let a = fs.open("/a").unwrap();
    let mut b = fs.open("/b").unwrap();
    fs.vgetattrs(
        &[
            Target::file(&a),
            Target::Path(std::path::Path::new("/path")),
            Target::file(&b),
        ],
        AttrsOptions::new(),
    )
    .unwrap();
    assert_eq!(queries.lock().unwrap()[0].1.len(), 3);
    assert!(queries.lock().unwrap()[0].1[0].is_descriptor());
    // No-follow applies to path operands, not retained objects.
    fs.vgetattrs(
        &[
            Target::file(&a),
            Target::Path(std::path::Path::new("/link")),
            Target::file(&b),
        ],
        AttrsOptions::new().follow_symlinks(false),
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
    fs.vfsync(&[&a, &b], SyncMode::All).unwrap();
    assert_eq!(syncs.lock().unwrap()[0].0, SyncMode::All);
    assert_eq!(syncs.lock().unwrap()[0].1.len(), 2);
    let foreign_fs = FsClient::new(AttrsBackend::default());
    let foreign = foreign_fs.open("/foreign").unwrap();
    let before = queries.lock().unwrap().len();
    assert_eq!(
        fs.vgetattrs(
            &[Target::file(&a), Target::file(&foreign)],
            AttrsOptions::new()
        )
        .unwrap_err()
        .index(),
        Some(1)
    );
    assert_eq!(
        fs.vfsync(&[&a, &foreign], SyncMode::Data)
            .unwrap_err()
            .index(),
        Some(1)
    );
    b.try_close().unwrap();
    assert_eq!(
        fs.vgetattrs(&[Target::file(&a), Target::file(&b)], AttrsOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        fs.vfsync(&[&a, &b], SyncMode::All).unwrap_err().index(),
        Some(1)
    );
    assert_eq!(queries.lock().unwrap().len(), before);
    assert_eq!(syncs.lock().unwrap().len(), 1);
    fs.vgetattrs::<&str>(&[], AttrsOptions::new()).unwrap();
    fs.vfsync(&[], SyncMode::All).unwrap();
    assert_eq!(queries.lock().unwrap().len(), before);
    assert_eq!(syncs.lock().unwrap().len(), 1);
}
#[test]
fn sync_errors_preserve_valid_indices_and_reject_invalid_backend_indices() {
    use vfsi_core::api::SyncMode;
    for index in [None, Some(1), Some(2)] {
        let error = index.map_or_else(
            || VfError::transport(None, "lost sync reply"),
            |i| VfError::client(i, libc::EIO as u32),
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
        let error = fs.vfsync(&[&a, &b], SyncMode::Data).unwrap_err();
        if index == Some(1) {
            assert_eq!(error.index(), Some(1));
            assert_eq!(error.path(), Some(std::path::Path::new("/b")));
        } else {
            assert!(error.is_transport());
            assert_eq!(error.index(), None);
        }
    }
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
                .vopen(&[OpenOp::new("/file", OpenFlags::READ | OpenFlags::APPEND)])
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
        let mut io = client.file_io(&file);
        assert_eq!(io.write(b"XY").unwrap(), 2);
        assert_eq!(io.position(), 5);
        io.seek(SeekFrom::Start(0)).unwrap();
        let mut prefix = [0; 3];
        io.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"abc");
        io.write_all(b"12345").unwrap();
        assert_eq!(io.position(), 10);
        io.write_all(b"").unwrap();
        assert_eq!(io.position(), 10);
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
    let mut io = client.file_io(&file);
    io.write_all(b"abcdef").unwrap();
    assert_eq!(io.position(), 15); // [3..5), [8..10), [13..15)
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
        .vwrite(
            &[
                vfsi_core::api::WriteOp::at(&file, 0, b"AAAA"),
                vfsi_core::api::WriteOp::at(&file, 100, b"BBBB"),
            ],
            vfsi_core::api::WriteOptions::new().write_all(true),
        )
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|r| (r.offset, r.written))
            .collect::<Vec<_>>(),
        [(3, 4), (7, 4)]
    );
    file.close().unwrap();
    assert_eq!(client.into_inner().unwrap().data, b"abcAAAABBBB");
    for failure_after in [0, 1] {
        for error in [
            VfError::client(0, libc::ENOSPC as u32),
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
            let actual = client.file_io(&file).write_all(data).unwrap_err();
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
