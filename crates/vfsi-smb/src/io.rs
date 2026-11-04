use super::*;

impl SmbVecFs {
    pub(super) fn raw_read(
        &mut self,
        file_id: FileId,
        offset: u64,
        length: usize,
    ) -> VfResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let length_u64 =
            u64::try_from(length).map_err(|_| VfError::errno(0, libc::EOVERFLOW as u32))?;
        checked_offset(offset, length_u64, 0)?;
        let max_read = self
            .client
            .params()
            .map(|p| p.max_read_size as usize)
            .unwrap_or(65_536)
            .max(1);
        let tree_id = self.tree.tree_id;
        let mut data = Vec::with_capacity(length);
        let mut position = offset;
        while data.len() < length {
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
            let frame = self
                .runtime
                .block_on(self.client.connection_mut().execute_with_credits(
                    Command::Read,
                    &request,
                    Some(tree_id),
                    CreditCharge(credit_charge(chunk as usize)),
                ))
                .map_err(|e| smb_error(e, 0))?;
            if frame.header.status == NtStatus::END_OF_FILE {
                break;
            }
            require_status(&frame, Command::Read, 0)?;
            let response = ReadResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(|e| smb_error(e, 0))?;
            let response_data = checked_read_data(response.data, chunk as usize, 0)?;
            let got = response_data.len();
            data.extend_from_slice(&response_data);
            position = checked_offset(position, got as u64, 0)?;
            if got < chunk as usize {
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
    ) -> VfResult<usize> {
        self.raw_write_progress(file_id, offset, data, |_| {})
    }

    pub(super) fn raw_write_progress(
        &mut self,
        file_id: FileId,
        offset: u64,
        data: &[u8],
        mut progress: impl FnMut(usize),
    ) -> VfResult<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let length =
            u64::try_from(data.len()).map_err(|_| VfError::errno(0, libc::EOVERFLOW as u32))?;
        checked_offset(offset, length, 0)?;
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
            let end = (written + max_write).min(data.len());
            let chunk = &data[written..end];
            let request = WriteRequest {
                data_offset: 0x70,
                offset: checked_offset(offset, written as u64, 0)?,
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
                .map_err(|e| smb_error(e, 0))?;
            require_status(&frame, Command::Write, 0)?;
            let response = WriteResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(|e| smb_error(e, 0))?;
            let count = response.count as usize;
            if count == 0 {
                return Err(VfError::invalid_reply(
                    None,
                    "SMB server reported a zero-byte successful write",
                ));
            }
            if count > chunk.len() {
                return Err(VfError::invalid_reply(
                    None,
                    "SMB server reported writing more bytes than requested",
                ));
            }
            written = written
                .checked_add(count)
                .ok_or_else(|| VfError::errno(0, libc::EOVERFLOW as u32))?;
            progress(written);
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::AfterWriteChunk { chunk: chunk_index })?;
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
    ) -> VfResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let max_read = self
            .client
            .params()
            .map(|p| p.max_read_size as usize)
            .unwrap_or(65_536);
        if length > max_read || length > u32::MAX as usize {
            let (file_id, _) = self.raw_open(path, libc::O_RDONLY)?;
            let result = self.raw_read(file_id, offset, length);
            let close = self.close_temporary_handle(file_id);
            return match result {
                Ok(data) => close.map(|_| data),
                Err(error) => {
                    let _ = close;
                    Err(error)
                }
            };
        }
        let create = self.open_request(path, libc::O_RDONLY)?;
        let read = ReadRequest {
            padding: 0x50,
            flags: 0,
            length: length as u32,
            offset,
            file_id: FileId::SENTINEL,
            minimum_count: 0,
            channel: SMB2_CHANNEL_NONE,
            remaining_bytes: 0,
            read_channel_info: Vec::new(),
        };
        let close = CloseRequest {
            flags: 0,
            file_id: FileId::SENTINEL,
        };
        let tree_id = self.tree.tree_id;
        let operations = [
            CompoundOp {
                command: Command::Create,
                body: &create,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::Read,
                body: &read,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(credit_charge(length)),
            },
            CompoundOp {
                command: Command::Close,
                body: &close,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
        ];
        let responses = self
            .runtime
            .block_on(self.client.connection_mut().execute_compound(&operations))
            .map_err(|e| smb_error(e, 0))?;
        let OpenCompoundResponseState {
            file_id,
            create_response,
            operation_responses,
            close_succeeded,
            cardinality_error,
        } = decode_open_compound_responses(
            responses,
            operations.len(),
            |response| {
                require_status(response, Command::Create, 0)?;
                CreateResponse::unpack(&mut ReadCursor::new(&response.body))
                    .map(|opened| opened.file_id)
                    .map_err(|error| smb_error(error, 0))
            },
            |response| response.header.status == NtStatus::SUCCESS,
            |error| smb_error(error, 0),
        )?;
        let data = if let Some(error) = cardinality_error {
            Err(error)
        } else {
            match operation_responses.into_iter().next() {
                Some(Ok(response)) if response.header.status == NtStatus::END_OF_FILE => {
                    Ok(Vec::new())
                }
                Some(Ok(response)) => require_status(&response, Command::Read, 0).and_then(|()| {
                    ReadResponse::unpack(&mut ReadCursor::new(&response.body))
                        .map_err(|error| smb_error(error, 0))
                        .and_then(|response| checked_read_data(response.data, length, 0))
                }),
                Some(Err(error)) => Err(smb_error(error, 0)),
                None => Err(compound_cardinality_error(1, operations.len())),
            }
        };
        let close = self.close_compound_temporary_handle(file_id, close_succeeded);
        // Release the response buffers promptly; a large read can otherwise
        // remain duplicated until this stack frame returns.
        drop(create_response);
        match data {
            Ok(data) => close.map(|()| data),
            Err(error) => {
                let _ = close;
                Err(error)
            }
        }
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
    ) -> VfResult<usize> {
        let max_write = self
            .client
            .params()
            .map(|p| p.max_write_size as usize)
            .unwrap_or(65_536);
        let mut flags = libc::O_WRONLY;
        if creation {
            flags |= libc::O_CREAT;
        }
        if truncate {
            flags |= libc::O_TRUNC;
        }
        if data.is_empty() {
            let (file_id, _) = self.raw_open(path, flags)?;
            let result = self.raw_flush(file_id);
            let close = self.close_temporary_handle(file_id);
            return result.and(close).map(|()| 0);
        }
        if data.len() > max_write || data.len() > u32::MAX as usize {
            let (file_id, _) = self.raw_open(path, flags)?;
            let result = self
                .raw_write(file_id, offset, data)
                .and_then(|written| self.raw_flush(file_id).map(|_| written));
            let close = self.close_temporary_handle(file_id);
            return match result {
                Ok(written) => close.map(|_| written),
                Err(error) => {
                    let _ = close;
                    Err(error)
                }
            };
        }
        let create = self.open_request(path, flags)?;
        let write = WriteRequest {
            data_offset: 0x70,
            offset,
            file_id: FileId::SENTINEL,
            channel: 0,
            remaining_bytes: 0,
            write_channel_info_offset: 0,
            write_channel_info_length: 0,
            flags: 0,
            data: data.to_vec(),
        };
        let flush = FlushRequest {
            file_id: FileId::SENTINEL,
        };
        let close = CloseRequest {
            flags: 0,
            file_id: FileId::SENTINEL,
        };
        let tree_id = self.tree.tree_id;
        let operations = [
            CompoundOp {
                command: Command::Create,
                body: &create,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::Write,
                body: &write,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(credit_charge(data.len())),
            },
            CompoundOp {
                command: Command::Flush,
                body: &flush,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::Close,
                body: &close,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
        ];
        let responses = self
            .runtime
            .block_on(self.client.connection_mut().execute_compound(&operations))
            .map_err(|e| smb_error(e, 0))?;
        let OpenCompoundResponseState {
            file_id,
            create_response,
            operation_responses,
            close_succeeded,
            cardinality_error,
        } = decode_open_compound_responses(
            responses,
            operations.len(),
            |response| {
                require_status(response, Command::Create, 0)?;
                CreateResponse::unpack(&mut ReadCursor::new(&response.body))
                    .map(|opened| opened.file_id)
                    .map_err(|error| smb_error(error, 0))
            },
            |response| response.header.status == NtStatus::SUCCESS,
            |error| smb_error(error, 0),
        )?;
        let write_result = if let Some(error) = cardinality_error {
            Err(error)
        } else {
            (|| {
                let mut responses = operation_responses.into_iter();
                let write = responses
                    .next()
                    .expect("complete compound contains WRITE response")
                    .map_err(|error| smb_error(error, 0))?;
                let flush = responses
                    .next()
                    .expect("complete compound contains FLUSH response")
                    .map_err(|error| smb_error(error, 0))?;
                let written = require_status(&write, Command::Write, 0).and_then(|()| {
                    WriteResponse::unpack(&mut ReadCursor::new(&write.body))
                        .map(|response| response.count as usize)
                        .map_err(|error| smb_error(error, 0))
                        .and_then(|count| {
                            if count > data.len() {
                                Err(VfError::invalid_reply(
                                    None,
                                    "SMB server reported writing more bytes than requested",
                                ))
                            } else {
                                Ok(count)
                            }
                        })
                })?;
                require_status(&flush, Command::Flush, 0)?;
                Ok(written)
            })()
        };
        let close_result = self.close_compound_temporary_handle(file_id, close_succeeded);
        drop(create_response);
        match write_result {
            Ok(written) => close_result.map(|()| written),
            Err(error) => {
                let _ = close_result;
                Err(error)
            }
        }
    }

    pub(super) fn read_one(&mut self, op: &ReadOp) -> VfResult<ReadResult> {
        if !op.file.is_fd() {
            if matches!(op.file, FileOperand::Saved) {
                return Err(VfError::unsupported(0));
            }
            let path = self.path_string(&self.file_path(&op.file)?)?;
            let offset = match op.offset {
                VfOffset::At(offset) => offset,
                VfOffset::End => self.client_stat(&path)?.size,
                VfOffset::Cur => {
                    return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
                }
                _ => return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput)),
            };
            let data = match self.compound_read_path(&path, offset, op.length) {
                Ok(data) => data,
                Err(_) if self.client.is_disconnected() => {
                    // Let the high-level stat operation recover the SMB
                    // session/tree, then retry only the requested range.
                    // Reading the complete file here made a tiny range read
                    // capable of allocating an unbounded remote-controlled
                    // amount of memory after reconnect.
                    self.client_stat(&path)?;
                    self.compound_read_path(&path, offset, op.length)?
                }
                Err(error) => return Err(error),
            };
            let eof = op.length > 0 && data.len() < op.length;
            return Ok(ReadResult {
                file: op.file.clone(),
                offset,
                data,
                eof,
            });
        }
        let (file_id, temporary, descriptor) = match &op.file {
            FileOperand::Fd(fd) => {
                let open = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::errno(0, ERR_EBADF))?;
                if !open.readable {
                    return Err(VfError::errno(0, ERR_EBADF));
                }
                (open.file_id, false, Some(*fd))
            }
            FileOperand::Saved => return Err(VfError::unsupported(0)),
            _ => {
                let path = self.path_string(&self.file_path(&op.file)?)?;
                let (id, _) = self.raw_open(&path, libc::O_RDONLY)?;
                (id, true, None)
            }
        };
        let size = self.query_size(file_id);
        let result = size.and_then(|size| {
            let offset = self.resolve_offset(descriptor, op.offset, size)?;
            let data = self.raw_read(file_id, offset, op.length)?;
            let end = checked_offset(offset, data.len() as u64, 0)?;
            let eof = !data.is_empty() && end >= size || op.length > 0 && data.len() < op.length;
            Ok((offset, data, eof, end))
        });
        let close_result = temporary.then(|| self.close_temporary_handle(file_id));
        let (offset, data, eof, end) = result?;
        if let Some(close) = close_result {
            close?;
        }
        if op.offset == VfOffset::Cur
            && let Some(fd) = descriptor
            && let Some(open) = self.open_files.get_mut(&fd)
        {
            open.cur_offset = end;
        }
        Ok(ReadResult {
            file: op.file.clone(),
            offset,
            data,
            eof,
        })
    }

    pub(super) fn write_one(&mut self, op: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteResult> {
        if !op.file().is_fd() {
            if matches!(op.file(), FileOperand::Saved) {
                return Err(VfError::unsupported(0));
            }
            let path = self.path_string(&self.file_path(op.file())?)?;
            let offset = match op.offset() {
                VfOffset::At(offset) => offset,
                VfOffset::End => self.client_stat(&path)?.size,
                VfOffset::Cur => {
                    return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
                }
                _ => return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput)),
            };
            let written =
                self.compound_write_path(&path, offset, op.data(), op.creates(), op.truncates())?;
            return Ok(WriteResult {
                file: op.file().clone(),
                offset,
                bytes_written: written,
                stable: true,
            });
        }
        let (file_id, temporary, descriptor, append) = match op.file() {
            FileOperand::Fd(fd) => {
                let open = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::errno(0, ERR_EBADF))?;
                if !open.writable {
                    return Err(VfError::errno(0, ERR_EBADF));
                }
                (open.file_id, false, Some(*fd), open.append)
            }
            FileOperand::Saved => return Err(VfError::unsupported(0)),
            _ => {
                let path = self.path_string(&self.file_path(op.file())?)?;
                let mut flags = libc::O_WRONLY;
                if op.creates() {
                    flags |= libc::O_CREAT;
                }
                if op.truncates() {
                    flags |= libc::O_TRUNC;
                }
                let (id, _) = self.raw_open(&path, flags)?;
                (id, true, None, false)
            }
        };
        let result = (|| {
            let size = if append || op.offset() == VfOffset::End {
                self.query_size(file_id)?
            } else {
                0
            };
            let offset = if append {
                size
            } else {
                self.resolve_offset(descriptor, op.offset(), size)?
            };
            let mut confirmed = 0usize;
            let write_result = self.raw_write_progress(file_id, offset, op.data(), |written| {
                confirmed = written;
            });
            if (append || !matches!(op.offset(), VfOffset::At(_)))
                && let Some(fd) = descriptor
                && let Some(open) = self.open_files.get_mut(&fd)
            {
                open.cur_offset = checked_offset(offset, confirmed as u64, 0)?;
            }
            let written = write_result?;
            self.raw_flush(file_id)?;
            Ok((offset, written))
        })();
        let close_result = temporary.then(|| self.close_temporary_handle(file_id));
        let (offset, written) = result?;
        if let Some(close) = close_result {
            close?;
        }
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
