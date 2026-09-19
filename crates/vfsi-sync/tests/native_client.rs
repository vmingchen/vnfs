use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use vfsi_sync::{
    Capabilities, FileSystem, FsClient, MetadataQuery, OpenFlags, OpenRequest, ReadOp, ReadResult,
    SetAttributes, VectorFileSystem, VfError, VfFile, VfOffset, VfResult, WriteOpRef, WriteResult,
};

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

    fn write_one(&mut self, request: WriteOpRef<'_>) -> VfResult<WriteResult> {
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
        self.data
            .resize(self.data.len().max(start + request.data.len()), 0);
        self.data[start..start + request.data.len()].copy_from_slice(request.data);
        self.cursor = offset + request.data.len() as u64;
        Ok(WriteResult {
            file: request.file.clone(),
            offset,
            written: request.data.len(),
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
    assert_eq!(error.index_opt(), None);
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
    assert_eq!(error.index_opt(), None);
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
    assert_eq!(read_error.index_opt(), None);

    let write_error = client
        .writev(&[
            files[0].write_request_at(0, b"a"),
            files[1].write_request_at(0, b"b"),
        ])
        .unwrap_err();
    assert!(write_error.is_transport());
    assert_eq!(write_error.index_opt(), None);

    client.closev(files).unwrap();
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
        assert_eq!(error.index_opt(), Some(0));
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
    assert_eq!(error.index_opt(), Some(0));
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
