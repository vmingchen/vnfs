use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use vfsi_sync::{
    Capabilities, DirEntry, DirPageCursor, DirectoryFileSystem, FileSystem, FsClient,
    MetadataQuery, OpenFlags, OpenRequest, ReadDirOptions, ReadIntoResult, ReadOp, ReadResult,
    SetAttributes, VectorFileSystem, VfAttrs, VfError, VfFile, VfOffset, VfResult, WriteOpRef,
    WriteResult,
};

#[test]
fn client_policy_applies_to_scalar_vector_and_into_reads_before_dispatch() {
    let backend = ScalarOnly {
        data: b"abcdef".to_vec(),
        ..ScalarOnly::default()
    };
    let read_calls = Arc::clone(&backend.read_calls);
    let client = FsClient::new(backend).with_limits(vfsi_sync::ResourceLimits {
        max_read_bytes: 3,
        stream_chunk_bytes: 2,
        ..vfsi_sync::ResourceLimits::default()
    });
    assert_eq!(client.clone().limits(), client.limits());
    assert_eq!(client.capabilities().unwrap(), Capabilities::empty());
    assert_eq!(
        client.read("/file").unwrap_err().kind(),
        std::io::ErrorKind::FileTooLarge
    );
    assert_eq!(client.read_with_limit("/file", 6).unwrap(), b"abcdef");
    let file = client.open("/file").unwrap();
    let before = read_calls.load(Ordering::SeqCst);
    assert!(client.readv(&[file.read_request_at(0, 4)]).is_err());
    let mut buffer = [0; 4];
    assert!(
        client
            .readv_into(&mut [file.read_request_at_into(0, &mut buffer)])
            .is_err()
    );
    assert_eq!(read_calls.load(Ordering::SeqCst), before);
    let result = client
        .readv_into_with_limit(&mut [file.read_request_at_into(0, &mut buffer)], 4)
        .unwrap();
    assert_eq!(result[0].read, 4);
    assert_eq!(&buffer, b"abcd");
    buffer.fill(0xff);
    let before = read_calls.load(Ordering::SeqCst);
    assert!(
        client
            .readv_into_with_limit(&mut [file.read_request_at_into(0, &mut buffer)], 3)
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
fn optimized_scalar_read_preserves_cleanup_retry_and_primary_error() {
    for read_failure in [false, true] {
        let client = FsClient::new(ScalarOnly {
            data: b"data".to_vec(),
            close_failures_remaining: 1,
            read_failure,
            ..ScalarOnly::default()
        });
        let error = client.read("/file").unwrap_err();
        if read_failure {
            assert!(!error.is_transport());
            assert_eq!(error.operation(), Some("read"));
        } else {
            assert!(error.is_transport());
            assert_eq!(error.operation(), Some("close"));
        }
        let backend = client.into_inner().unwrap();
        assert_eq!(backend.close_calls, 2);
        assert!(!backend.open);
    }
}

#[derive(Default)]
struct ScalarOnly {
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
}

impl VectorFileSystem for ScalarOnly {
    fn open_many(&mut self, requests: &[OpenRequest]) -> VfResult<Vec<VfFile>> {
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
            .map(|request| self.open_one(request))
            .collect()
    }

    fn close_many(&mut self, files: &[VfFile]) -> VfResult<()> {
        for file in files {
            self.close_one(file)?;
        }
        Ok(())
    }

    fn read_many(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        let mut results: Vec<_> = requests
            .iter()
            .take(limit)
            .map(|request| self.read_one(request))
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

    fn read_many_into(
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
                self.read_one_into(request, buffer)
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

    fn write_many(&mut self, requests: &[WriteOpRef<'_>]) -> VfResult<Vec<WriteResult>> {
        let limit = self
            .vector_result_limit
            .lock()
            .expect("vector limit poisoned")
            .unwrap_or(usize::MAX);
        let mut results: Vec<_> = requests
            .iter()
            .take(limit)
            .map(|request| self.write_one(*request))
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
}

impl FileSystem for ScalarOnly {
    fn capabilities(&self) -> Capabilities {
        Capabilities::empty()
    }

    fn open_one(&mut self, _: &OpenRequest) -> VfResult<VfFile> {
        self.open = true;
        Ok(VfFile::from_fd(1))
    }

    fn close_one(&mut self, _: &VfFile) -> VfResult<()> {
        self.close_calls += 1;
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

    fn read_one(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
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

    fn read_one_into(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        if !self.direct_into_only {
            let result = self.read_one(request)?;
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

    fn write_one(&mut self, request: WriteOpRef<'_>) -> VfResult<WriteResult> {
        self.write_calls.fetch_add(1, Ordering::SeqCst);
        if self.oversized_write_count {
            return Ok(WriteResult {
                file: request.file.clone(),
                offset: 0,
                written: request.data.len() + 1,
                stable: true,
            });
        }
        let offset = match request.offset {
            VfOffset::At(value) => value,
            VfOffset::Cur => self.cursor,
            VfOffset::End => self.data.len() as u64,
            _ => 0,
        };
        let start = offset as usize;
        let length = request
            .data
            .len()
            .min(self.max_write_once.unwrap_or(usize::MAX));
        self.data.resize(self.data.len().max(start + length), 0);
        self.data[start..start + length].copy_from_slice(&request.data[..length]);
        self.cursor = offset + length as u64;
        Ok(WriteResult {
            file: request.file.clone(),
            offset,
            written: length,
            stable: true,
        })
    }

    fn seek_one(&mut self, _: &VfFile, position: SeekFrom) -> VfResult<u64> {
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

    fn metadata(&mut self, _: MetadataQuery) -> VfResult<vfsi_sync::VfAttrs> {
        Err(VfError::unsupported(0))
    }

    fn set_attributes(&mut self, _: SetAttributes) -> VfResult<()> {
        Err(VfError::unsupported(0))
    }
}

#[test]
fn owned_client_accepts_a_scalar_only_backend() {
    let client = FsClient::new(ScalarOnly::default());
    let mut file = client
        .open_with(OpenRequest::new(
            "/file",
            OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
        ))
        .unwrap();
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
        .open_with(OpenRequest::new("/file", OpenFlags::READ))
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
        .open_with(OpenRequest::new("/file", OpenFlags::WRITE))
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
    let error = client.read_with_limit("/file", 3).unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(error.operation(), Some("read"));
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));

    let client = FsClient::new(ScalarOnly {
        data: b"four".to_vec(),
        ..ScalarOnly::default()
    });
    assert_eq!(client.read_with_limit("/file", 4).unwrap(), b"four");

    let client = FsClient::new(ScalarOnly {
        data: b"utf8".to_vec(),
        ..ScalarOnly::default()
    });
    assert_eq!(
        client.read_to_string_with_limit("/file", 4).unwrap(),
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
        .openv(&[
            OpenRequest::new("/first", OpenFlags::READ),
            OpenRequest::new("/second", OpenFlags::READ),
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
        .openv(&[
            OpenRequest::new("/first", OpenFlags::READ),
            OpenRequest::new("/second", OpenFlags::READ),
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
        .openv(&[
            OpenRequest::new("/first", OpenFlags::READ | OpenFlags::WRITE),
            OpenRequest::new("/second", OpenFlags::READ | OpenFlags::WRITE),
        ])
        .unwrap();
    *vector_result_limit.lock().unwrap() = Some(1);

    let read_error = client
        .readv(&[
            files[0].read_request_at(0, 1),
            files[1].read_request_at(0, 1),
        ])
        .unwrap_err();
    assert!(read_error.is_transport());
    assert_eq!(read_error.index(), None);

    let write_error = client
        .writev(&[
            files[0].write_request_at(0, b"a"),
            files[1].write_request_at(0, b"b"),
        ])
        .unwrap_err();
    assert!(write_error.is_transport());
    assert_eq!(write_error.index(), None);

    client.closev(files).unwrap();
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
        .readv_with_limit(&[file.read_request_at(0, 3), file.read_request_at(3, 3)], 5)
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(error.path(), Some(std::path::Path::new("/file")));
    assert_eq!(read_calls.load(Ordering::SeqCst), 0);

    let error = client
        .readv(&[file.read_request_at(0, vfsi_sync::DEFAULT_READV_MAX_TOTAL_BYTES + 1)])
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(read_calls.load(Ordering::SeqCst), 0);

    let results = client
        .readv_with_limit(&[file.read_request_at(0, 3)], 3)
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
        .open_with(OpenRequest::new("/file", OpenFlags::WRITE))
        .unwrap();
    let results = client
        .write_allv(&[
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
        .open_with(OpenRequest::new("/file", OpenFlags::WRITE))
        .unwrap();
    let results = client
        .write_allv(&[
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
        .open_with(OpenRequest::new("/file", OpenFlags::WRITE))
        .unwrap();
    client
        .write_allv(&[
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
        .open_with(OpenRequest::new("/file", OpenFlags::WRITE))
        .unwrap();
    let error = client
        .write_allv(&[file.write_request_at(0, b"data")])
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
        .open_with(OpenRequest::new("/local", OpenFlags::WRITE))
        .unwrap();
    let foreign_file = other
        .open_with(OpenRequest::new("/foreign", OpenFlags::WRITE))
        .unwrap();
    let error = client
        .write_allv(&[
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
        let error = client.readv(&[file.read_request_at(0, 1)]).unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.index(), Some(0));
        assert_eq!(error.operation(), Some("readv"));
        drop(file);
    }

    let client = FsClient::new(ScalarOnly {
        oversized_write_count: true,
        ..ScalarOnly::default()
    });
    let file = client
        .open_with(OpenRequest::new("/file", OpenFlags::WRITE))
        .unwrap();
    let error = client
        .writev(&[file.write_request_at(0, b"x")])
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.operation(), Some("writev"));
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
    assert!(client.try_closev(&mut files).is_err());
    assert_eq!(files[0].path(), std::path::Path::new("/file"));
    client.try_closev(&mut files).unwrap();
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
    let client = FsClient::new(ScalarOnly {
        data: b"abcdef".to_vec(),
        ..ScalarOnly::default()
    });
    let mut other = Some(client.open("/other").unwrap());
    let mut received = Vec::new();
    client
        .read_stream_with_options(
            "/file",
            vfsi_sync::ReadStreamOptions::new().chunk_size(2),
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

impl DirectoryFileSystem for ScalarOnly {
    fn create_dir_one(&mut self, _: &std::path::Path, _: u32) -> VfResult<()> {
        Ok(())
    }

    fn read_dir_one(&mut self, _: &std::path::Path, _: ReadDirOptions) -> VfResult<Vec<DirEntry>> {
        unreachable!("the visitor must use paged enumeration")
    }

    fn read_dir_page(
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
                    VfAttrs::default().into(),
                )
            })
            .collect();
        let next = (end < ceiling).then(|| DirPageCursor::new(end));
        Ok((entries, next))
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
    client.visit_dir("/tree", |_| Ok(false)).unwrap();
    assert_eq!(*page_sizes.lock().unwrap(), [1]);

    page_sizes.lock().unwrap().clear();
    let mut seen = 0;
    let error = client
        .visit_dir_with_options("/tree", ReadDirOptions::new().max_entries(3), |_| {
            seen += 1;
            Ok(true)
        })
        .unwrap_err();
    assert_eq!(seen, 3);
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(*page_sizes.lock().unwrap(), [1, 3]);

    page_sizes.lock().unwrap().clear();
    let mut seen = 0;
    client
        .visit_dir("/tree", |_| {
            seen += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(seen, 2_100);
    assert_eq!(*page_sizes.lock().unwrap(), [1, 1024, 1024, 1024]);
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
        .readv_into(&mut [
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
        .readv_into(&mut [file.read_request_at_into(0, &mut buffer)])
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
        .readv_into(&mut [file.read_request_at_into(0, &mut buffer)])
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
}
