use std::io::{Read, Seek, SeekFrom, Write};

use vfsi_sync::{
    Capabilities, FileSystem, FsClient, MetadataQuery, OpenFlags, OpenRequest, ReadOp, ReadResult,
    SetAttributes, VfError, VfFile, VfOffset, VfResult, WriteOpRef, WriteResult,
};

#[derive(Default)]
struct ScalarOnly {
    data: Vec<u8>,
    cursor: u64,
    open: bool,
    oversized_read: bool,
    oversized_write_count: bool,
    read_failure: bool,
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
