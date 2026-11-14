use super::*;
use vfsi_core::{FailureState, IoProgress, OpError, OpResult};

impl SmbVecFs {
    pub(super) fn raw_read(
        &mut self,
        file_id: FileId,
        offset: u64,
        length: usize,
    ) -> OpResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        if length > isize::MAX as usize {
            return Err(OpError::not_attempted(VfError::errno(
                None,
                libc::EFBIG as u32,
            )));
        }
        let length_u64 = u64::try_from(length)
            .map_err(|_| OpError::not_attempted(VfError::errno(None, libc::EOVERFLOW as u32)))?;
        checked_offset(offset, length_u64, 0).map_err(OpError::not_attempted)?;
        let max_read = self
            .client
            .params()
            .map(|params| params.max_read_size as usize)
            .unwrap_or(65_536)
            .max(1);
        let tree_id = self.tree.tree_id;
        let mut data = Vec::with_capacity(length);
        let mut position = offset;
        #[cfg(feature = "test-faults")]
        let mut chunk_index = 0;
        while data.len() < length {
            #[cfg(feature = "test-faults")]
            {
                if let Err(cause) =
                    self.inject_open_fault(OpenFaultPoint::BeforeReadChunk { chunk: chunk_index })
                {
                    return Err(if data.is_empty() {
                        OpError::not_attempted(cause)
                    } else {
                        OpError::failed(
                            cause,
                            Some(vfsi_core::IoProgress::Read(
                                vfsi_core::api::ReadResult::owned(offset, data, false),
                            )),
                        )
                    });
                }
                chunk_index += 1;
            }
            let chunk = (length - data.len()).min(max_read).min(u32::MAX as usize) as u32;
            let request = ReadRequest {
                padding: 0x50,
                flags: 0,
                length: chunk,
                offset: position,
                file_id,
                minimum_count: 0,
                channel: SMB2_CHANNEL_NONE,
                remaining_bytes: 0,
                read_channel_info: Vec::new(),
            };
            let result = (|| -> VfResult<Vec<u8>> {
                let frame = self
                    .runtime
                    .block_on(self.client.connection_mut().execute_with_credits(
                        Command::Read,
                        &request,
                        Some(tree_id),
                        CreditCharge(credit_charge(chunk as usize)),
                    ))
                    .map_err(|error| smb_error(error, 0))?;
                decode_read_response(&frame, chunk as usize, 0)
            })();
            let response = match result {
                Ok(response) => response,
                Err(cause) => {
                    return Err(read_wire_error(
                        cause,
                        offset,
                        (!data.is_empty()).then_some(data),
                    ));
                }
            };
            let count = response.len();
            data.extend_from_slice(&response);
            position += count as u64; // whole requested range was preflighted
            if count < chunk as usize {
                break;
            }
        }
        Ok(data)
    }

    pub(super) fn raw_write(
        &mut self,
        file_id: FileId,
        offset: u64,
        data: &[u8],
    ) -> OpResult<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let length = u64::try_from(data.len())
            .map_err(|_| OpError::not_attempted(VfError::errno(None, libc::EOVERFLOW as u32)))?;
        checked_offset(offset, length, 0).map_err(OpError::not_attempted)?;
        if self.client.is_disconnected() {
            return Err(OpError::not_attempted(VfError::transport_with_kind(
                None,
                vfsi_core::TransportKind::Connection,
                "SMB connection is disconnected",
            )));
        }
        let max_write = self
            .client
            .params()
            .map(|p| p.max_write_size as usize)
            .unwrap_or(65_536)
            .max(1);
        let tree_id = self.tree.tree_id;
        let mut written = 0usize;
        #[cfg(feature = "test-faults")]
        let mut chunk_index = 0usize;
        while written < data.len() {
            #[cfg(feature = "test-faults")]
            if let Err(cause) =
                self.inject_open_fault(OpenFaultPoint::BeforeWriteChunk { chunk: chunk_index })
            {
                return Err(if written == 0 {
                    OpError::not_attempted(cause)
                } else {
                    OpError::failed(
                        cause,
                        Some(IoProgress::Write {
                            bytes_written: written,
                        }),
                    )
                });
            }
            let end = (written + max_write).min(data.len());
            let chunk = &data[written..end];
            let request = WriteRequest {
                data_offset: 0x70,
                offset: offset + written as u64,
                file_id,
                channel: 0,
                remaining_bytes: (data.len() - end).min(u32::MAX as usize) as u32,
                write_channel_info_offset: 0,
                write_channel_info_length: 0,
                flags: SMB2_WRITEFLAG_WRITE_THROUGH,
                data: chunk.to_vec(),
            };
            let frame = self
                .runtime
                .block_on(self.client.connection_mut().execute_with_credits(
                    Command::Write,
                    &request,
                    Some(tree_id),
                    CreditCharge(credit_charge(chunk.len())),
                ))
                .map_err(|e| write_wire_error(smb_error(e, 0), written))?;
            require_status(&frame, Command::Write, 0)
                .map_err(|cause| write_wire_error(cause, written))?;
            let response = WriteResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(|e| write_wire_error(smb_error(e, 0), written))?;
            let count = response.count as usize;
            if count == 0 {
                return Err(OpError::failed(
                    VfError::errno(None, ERR_IO),
                    (written != 0).then_some(IoProgress::Write {
                        bytes_written: written,
                    }),
                ));
            }
            if count > chunk.len() {
                return Err(write_wire_error(
                    VfError::invalid_reply(
                        None,
                        "SMB server reported writing more bytes than requested",
                    ),
                    written,
                ));
            }
            written += count;
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::AfterWriteChunk { chunk: chunk_index })
                .map_err(|cause| {
                    OpError::failed(
                        cause,
                        Some(IoProgress::Write {
                            bytes_written: written,
                        }),
                    )
                })?;
            #[cfg(feature = "test-faults")]
            {
                chunk_index += 1;
            }
        }
        Ok(written)
    }

    /// Read a range from a path as CREATE + READ + CLOSE in one related
    /// compound. This is the SMB counterpart of a path-based NFSv4 COMPOUND.
    pub(super) fn compound_read_path(
        &mut self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> OpResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        checked_offset(offset, length as u64, 0).map_err(OpError::not_attempted)?;
        let max_read = self
            .client
            .params()
            .map(|params| params.max_read_size as usize)
            .unwrap_or(65_536);
        if length > max_read || length > u32::MAX as usize {
            let (file_id, _) = self
                .raw_open(path, libc::O_RDONLY)
                .map_err(|cause| read_wire_error(cause, offset, None))?;
            let result = self.raw_read(file_id, offset, length);
            let close = self.close_temporary_handle(file_id);
            return finish_read_result(result, close, None, offset, length);
        }
        let create = self
            .open_request(path, libc::O_RDONLY)
            .map_err(OpError::not_attempted)?;
        let tree = self.tree.clone();
        let mut deferred_close = None;
        let result = self.runtime.block_on(concurrent_compound_read_inner(
            self.client.connection_mut().clone(),
            tree.clone(),
            create,
            offset,
            length,
            &mut deferred_close,
        ));
        self.deferred_closes
            .extend(deferred_close.map(|state| DeferredClose::new(self.tree.clone(), state)));
        result
    }

    /// Write a range as CREATE + WRITE + FLUSH + CLOSE in one related
    /// compound when it fits the negotiated MaxWriteSize.
    pub(super) fn compound_write_path(
        &mut self,
        path: &str,
        offset: u64,
        data: &[u8],
        creation: bool,
        truncate: bool,
    ) -> OpResult<usize> {
        checked_offset(offset, data.len() as u64, 0).map_err(OpError::not_attempted)?;
        let max_write = self
            .client
            .params()
            .map(|params| params.max_write_size as usize)
            .unwrap_or(65_536);
        let flags = libc::O_WRONLY
            | if creation { libc::O_CREAT } else { 0 }
            | if truncate { libc::O_TRUNC } else { 0 };
        if data.is_empty() || data.len() > max_write || data.len() > u32::MAX as usize {
            let (file_id, _) = self
                .raw_open(path, flags)
                .map_err(|cause| write_wire_error(cause, 0))?;
            let result = self.raw_write(file_id, offset, data).and_then(|written| {
                self.raw_flush(file_id)
                    .map(|()| written)
                    .map_err(|cause| write_wire_error(cause, written))
            });
            let close = self.close_temporary_handle(file_id);
            return finish_write_result(result, close, None);
        }
        let create = self
            .open_request(path, flags)
            .map_err(OpError::not_attempted)?;
        let tree = self.tree.clone();
        let mut deferred_close = None;
        let result = self.runtime.block_on(concurrent_compound_write_inner(
            self.client.connection_mut().clone(),
            tree.clone(),
            create,
            offset,
            data.to_vec(),
            &mut deferred_close,
        ));
        self.deferred_closes
            .extend(deferred_close.map(|state| DeferredClose::new(self.tree.clone(), state)));
        result
    }

    /// Reconnect only before any bytes are confirmed. A pathname can identify
    /// a replacement object after reconnect, so never append its bytes to an
    /// earlier prefix or replay a completed read whose temporary CLOSE failed.
    fn recover_path_read(
        &mut self,
        path: &str,
        offset: u64,
        length: usize,
        error: OpError,
    ) -> OpResult<Vec<u8>> {
        if !self.client.is_disconnected()
            || error.progress().is_some_and(|progress| {
                matches!(progress, IoProgress::Read(read)
                if read.eof() || read.bytes_read() > 0 || read.bytes_read() >= length)
            })
        {
            return Err(error);
        }
        let (_, original_state, progress) = error.into_parts();
        let mut prefix = match progress {
            Some(IoProgress::Read(read)) => read.into_data().ok_or_else(|| {
                OpError::unknown(
                    VfError::invalid_reply(None, "SMB path read returned buffered progress"),
                    None,
                )
            })?,
            Some(_) => {
                return Err(OpError::unknown(
                    VfError::invalid_reply(None, "invalid SMB read progress"),
                    None,
                ));
            }
            None => Vec::new(),
        };
        if let Err(cause) = self.client_stat(path) {
            let uncertain = cause.is_transport() || original_state == FailureState::Unknown;
            let progress = (!prefix.is_empty()).then(|| {
                IoProgress::Read(vfsi_core::api::ReadResult::owned(offset, prefix, false))
            });
            return Err(if uncertain {
                OpError::unknown(cause, progress)
            } else {
                OpError::failed(cause, progress)
            });
        }
        let remaining = length - prefix.len();
        let retry = self.compound_read_path(path, offset + prefix.len() as u64, remaining);
        match retry {
            Ok(data) => {
                if prefix.is_empty() {
                    return Ok(data);
                }
                prefix.extend_from_slice(&data);
                Ok(prefix)
            }
            Err(error) => {
                let (cause, state, progress) = error.into_parts();
                let mut eof = false;
                let had_progress = progress.is_some();
                if let Some(IoProgress::Read(read)) = progress {
                    eof = read.eof();
                    let Some(data) = read.into_data() else {
                        return Err(OpError::unknown(
                            VfError::invalid_reply(None, "invalid SMB retry progress"),
                            None,
                        ));
                    };
                    if prefix.is_empty() {
                        prefix = data;
                    } else {
                        prefix.extend_from_slice(&data);
                    }
                }
                let progress = (!prefix.is_empty() || had_progress).then(|| {
                    IoProgress::Read(vfsi_core::api::ReadResult::owned(offset, prefix, eof))
                });
                Err(
                    if state == FailureState::Unknown || original_state == FailureState::Unknown {
                        OpError::unknown(cause, progress)
                    } else {
                        OpError::failed(cause, progress)
                    },
                )
            }
        }
    }

    pub(super) fn read_one(&mut self, op: &ReadOp) -> OpResult<ReadResult> {
        if op.length > isize::MAX as usize {
            return Err(OpError::not_attempted(VfError::errno(
                None,
                libc::EFBIG as u32,
            )));
        }
        if let VfOffset::At(offset) = op.offset {
            checked_offset(offset, op.length as u64, 0).map_err(OpError::not_attempted)?;
        }
        let Some(fd) = op.file.fd() else {
            if matches!(op.file, FileOperand::Saved) {
                return Err(OpError::not_attempted(VfError::unsupported(None)));
            }
            let path = self
                .path_string(&self.file_path(&op.file).map_err(OpError::not_attempted)?)
                .map_err(OpError::not_attempted)?;
            let offset = match op.offset {
                VfOffset::At(offset) => offset,
                VfOffset::End => {
                    self.client_stat(&path)
                        .map_err(|cause| read_wire_error(cause, 0, None))?
                        .size
                }
                _ => {
                    return Err(OpError::not_attempted(VfError::from_kind(
                        None,
                        std::io::ErrorKind::InvalidInput,
                    )));
                }
            };
            let result = match self.compound_read_path(&path, offset, op.length) {
                Ok(data) => Ok(data),
                Err(error) => self.recover_path_read(&path, offset, op.length, error),
            };
            let data = result.map_err(|error| {
                if op.offset == VfOffset::End && error.state() == FailureState::NotAttempted {
                    OpError::failed(error.into_cause(), None)
                } else {
                    error
                }
            })?;
            return Ok(ReadResult {
                file: op.file.clone(),
                offset,
                eof: op.length > 0 && data.len() < op.length,
                data,
            });
        };
        let open = self.live_file(fd).map_err(OpError::not_attempted)?;
        if !open.readable {
            return Err(OpError::not_attempted(VfError::errno(None, ERR_EBADF)));
        }
        let file_id = open.file_id;
        let size = self
            .query_size(file_id)
            .map_err(|cause| read_wire_error(cause, 0, None))?;
        let offset = self
            .resolve_offset(Some(fd), op.offset, size)
            .map_err(|cause| OpError::failed(cause, None))?;
        let result = self.raw_read(file_id, offset, op.length).map_err(|error| {
            if error.state() == FailureState::NotAttempted {
                OpError::failed(error.into_cause(), None)
            } else {
                error
            }
        });
        let count = match &result {
            Ok(data) => data.len(),
            Err(error) => match error.progress() {
                Some(IoProgress::Read(read)) => read.bytes_read(),
                _ => 0,
            },
        };
        let end = offset + count as u64; // raw_read preflights the whole range
        if op.offset == VfOffset::Cur
            && let Some(open) = self.open_files.get_mut(&fd)
        {
            open.cur_offset = end;
        }
        let data = result?;
        let eof = !data.is_empty() && end >= size || op.length > 0 && data.len() < op.length;
        Ok(ReadResult {
            file: op.file.clone(),
            offset,
            data,
            eof,
        })
    }

    pub(super) fn write_one(&mut self, op: WriteOp<&FileOperand, &[u8]>) -> OpResult<WriteResult> {
        let Some(fd) = op.file().fd() else {
            if matches!(op.file(), FileOperand::Saved) {
                return Err(OpError::not_attempted(VfError::unsupported(None)));
            }
            let path = self
                .file_path(op.file())
                .and_then(|path| self.path_string(&path))
                .map_err(OpError::not_attempted)?;
            let offset = match op.offset() {
                VfOffset::At(offset) => offset,
                VfOffset::End => {
                    self.client_stat(&path)
                        .map_err(OpError::not_attempted)?
                        .size
                }
                VfOffset::Cur => {
                    return Err(OpError::not_attempted(VfError::from_kind(
                        None,
                        std::io::ErrorKind::InvalidInput,
                    )));
                }
                _ => {
                    return Err(OpError::not_attempted(VfError::from_kind(
                        None,
                        std::io::ErrorKind::InvalidInput,
                    )));
                }
            };
            let written =
                self.compound_write_path(&path, offset, op.data(), op.creates(), op.truncates())?;
            return Ok(WriteResult {
                file: op.file().clone(),
                offset,
                bytes_written: written,
                stable: true,
            });
        };
        let open = self.live_file(fd).map_err(OpError::not_attempted)?;
        if !open.writable {
            return Err(OpError::not_attempted(VfError::errno(None, ERR_EBADF)));
        }
        let (file_id, append) = (open.file_id, open.append);
        let size = if append || op.offset() == VfOffset::End {
            self.query_size(file_id).map_err(OpError::not_attempted)?
        } else {
            0
        };
        let offset = if append {
            size
        } else {
            self.resolve_offset(Some(fd), op.offset(), size)
                .map_err(OpError::not_attempted)?
        };
        let write_result = self.raw_write(file_id, offset, op.data());
        let confirmed = match &write_result {
            Ok(written) => *written,
            Err(error) => match error.progress() {
                Some(IoProgress::Write { bytes_written }) => *bytes_written,
                _ => 0,
            },
        };
        if (append || !matches!(op.offset(), VfOffset::At(_)))
            && let Some(open) = self.open_files.get_mut(&fd)
        {
            open.cur_offset = offset + confirmed as u64;
        }
        let written = write_result?;
        self.raw_flush(file_id)
            .map_err(|cause| write_wire_error(cause, written))?;
        Ok(WriteResult {
            file: op.file().clone(),
            offset,
            bytes_written: written,
            stable: true,
        })
    }

    pub(super) fn resolve_offset(
        &self,
        fd: Option<Fd>,
        offset: VfOffset,
        size: u64,
    ) -> VfResult<u64> {
        match offset {
            VfOffset::At(offset) => Ok(offset),
            VfOffset::End => Ok(size),
            VfOffset::Cur => fd
                .and_then(|fd| self.open_files.get(&fd).map(|open| open.cur_offset))
                .ok_or_else(|| VfError::from_kind(0, std::io::ErrorKind::InvalidInput)),
            _ => Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput)),
        }
    }
}
