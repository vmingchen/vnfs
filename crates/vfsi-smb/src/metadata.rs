use super::*;

impl SmbVecFs {
    pub(super) fn query_size(&mut self, file_id: FileId) -> VfResult<u64> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeSizeQuery)?;
        use smb2::msg::query_info::{InfoType, QueryInfoRequest, QueryInfoResponse};
        let request = QueryInfoRequest {
            info_type: InfoType::File,
            file_info_class: 5,
            output_buffer_length: 24,
            additional_information: 0,
            flags: 0,
            file_id,
            input_buffer: Vec::new(),
        };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::QueryInfo,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::QueryInfo, 0)?;
        let response = QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(|e| smb_error(e, 0))?;
        if response.output_buffer.len() < 16 {
            return Err(VfError::invalid_reply(
                None,
                "short SMB FileStandardInformation response",
            ));
        }
        Ok(u64::from_le_bytes(
            response.output_buffer[8..16]
                .try_into()
                .expect("length checked"),
        ))
    }

    /// Query the retained object, never its possibly stale saved pathname.
    pub(super) fn query_handle_info(
        &mut self,
        file_id: FileId,
    ) -> VfResult<smb2::client::FileInfo> {
        let requests =
            [(FILE_BASIC_INFORMATION, 40), (5, 24)].map(|(class, length)| QueryInfoRequest {
                info_type: smb2::msg::query_info::InfoType::File,
                file_info_class: class,
                output_buffer_length: length,
                additional_information: 0,
                flags: 0,
                file_id,
                input_buffer: Vec::new(),
            });
        let operations = requests.each_ref().map(|request| CompoundOp {
            command: Command::QueryInfo,
            body: request,
            tree_id: Some(self.tree.tree_id),
            credit_charge: CreditCharge(1),
        });
        let responses = self
            .runtime
            .block_on(self.client.connection_mut().execute_compound(&operations))
            .map_err(|error| smb_error(error, 0))?;
        let responses = collect_compound(responses, operations.len())?;
        for response in &responses {
            require_status(response, Command::QueryInfo, 0)?;
        }
        let basic = QueryInfoResponse::unpack(&mut ReadCursor::new(&responses[0].body))
            .map_err(|error| smb_error(error, 0))?;
        let standard = QueryInfoResponse::unpack(&mut ReadCursor::new(&responses[1].body))
            .map_err(|error| smb_error(error, 0))?;
        handle_info(&basic.output_buffer, &standard.output_buffer)
    }

    pub(super) fn set_size_handle(&mut self, file_id: FileId, size: u64) -> VfResult<()> {
        let request = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: FILE_END_OF_FILE_INFORMATION,
            additional_information: 0,
            file_id,
            buffer: size.to_le_bytes().to_vec(),
        };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::SetInfo,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::SetInfo, 0)
    }

    pub(super) fn filetime_value(seconds: i64, nanoseconds: u32, index: usize) -> VfResult<u64> {
        if nanoseconds >= 1_000_000_000 {
            return Err(VfError::from_kind(index, std::io::ErrorKind::InvalidInput));
        }
        const WINDOWS_EPOCH_SECONDS: i128 = 11_644_473_600;
        let ticks = (i128::from(seconds) + WINDOWS_EPOCH_SECONDS)
            .checked_mul(10_000_000)
            .and_then(|value| value.checked_add(i128::from(nanoseconds / 100)))
            // FileBasicInformation timestamps are signed. Zero and the
            // negative values select update policies rather than a date.
            .filter(|value| (1..=i128::from(i64::MAX)).contains(value))
            .ok_or_else(|| VfError::from_kind(index, std::io::ErrorKind::InvalidInput))?;
        Ok(ticks as u64)
    }

    pub(super) fn set_times_handle(
        &mut self,
        file_id: FileId,
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
        index: usize,
    ) -> VfResult<()> {
        let access = match atime {
            Some((seconds, nanoseconds)) => Self::filetime_value(seconds, nanoseconds, index)?,
            None => 0,
        };
        let modified = match mtime {
            Some((seconds, nanoseconds)) => Self::filetime_value(seconds, nanoseconds, index)?,
            None => 0,
        };
        let mut buffer = Vec::with_capacity(40);
        buffer.extend_from_slice(&0u64.to_le_bytes()); // creation: unchanged
        buffer.extend_from_slice(&access.to_le_bytes());
        buffer.extend_from_slice(&modified.to_le_bytes());
        buffer.extend_from_slice(&0u64.to_le_bytes()); // change: unchanged
        buffer.extend_from_slice(&0u32.to_le_bytes()); // attributes: unchanged
        buffer.extend_from_slice(&0u32.to_le_bytes()); // reserved
        let request = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: FILE_BASIC_INFORMATION,
            additional_information: 0,
            file_id,
            buffer,
        };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::SetInfo,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, index))?;
        require_status(&frame, Command::SetInfo, index)
    }

    pub(super) fn set_times_path(
        &mut self,
        path: &str,
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
        index: usize,
    ) -> VfResult<()> {
        let is_directory = self.client_stat(path)?.is_directory;
        let flags = if is_directory {
            libc::O_RDONLY | libc::O_DIRECTORY
        } else {
            libc::O_RDONLY
        };
        let mut request = self.open_request(path, flags)?;
        request.desired_access = FileAccessMask::new(
            FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::FILE_WRITE_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
        );
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::Create,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, index))?;
        require_status(&frame, Command::Create, index)?;
        let response = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(|e| smb_error(e, index))?;
        let result = self.set_times_handle(response.file_id, atime, mtime, index);
        let close = self
            .close_temporary_handle(response.file_id)
            .map_err(|e| e.with_index(index));
        result.and(close)
    }

    pub(super) fn set_size_encoded_path(&mut self, path: &str, size: u64) -> VfResult<()> {
        let (file_id, _) = self.raw_open(path, libc::O_WRONLY)?;
        let result = self.set_size_handle(file_id, size);
        let close = self.close_temporary_handle(file_id);
        result.and(close)
    }

    pub(super) fn client_stat(&mut self, path: &str) -> VfResult<smb2::client::FileInfo> {
        let tree = Arc::make_mut(&mut self.tree);
        let info = self
            .runtime
            .block_on(self.client.stat(tree.sdk_tree_mut(), path));
        tree.refresh_connection(self.client.connection().clone());
        self.follow_stat_link(path, info.map_err(|error| smb_error(error, 0))?)
    }

    pub(super) fn follow_stat_link(
        &mut self,
        path: &str,
        info: smb2::client::FileInfo,
    ) -> VfResult<smb2::client::FileInfo> {
        if info.attributes.0 & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return Ok(info);
        }
        // smb2 0.34's stat describes the final link. Our followed metadata
        // contract needs its target instead. Only links pay for this fallback;
        // ordinary files retain the concurrent, single-compound fast path.
        let mut request = self.open_request(path, libc::O_RDONLY)?;
        request.desired_access =
            FileAccessMask::new(FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE);
        request.create_options = 0; // Follow links, accepting files or directories.
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::Create,
                &request,
                Some(self.tree.tree_id),
            ))
            .map_err(|error| smb_error(error, 0))?;
        require_status(&frame, Command::Create, 0)?;
        let opened = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(|error| smb_error(error, 0))?;
        let result = self.query_handle_info(opened.file_id);
        let close = self.close_temporary_handle(opened.file_id);
        result.and_then(|info| close.map(|()| info))
    }

    pub(super) fn fill_attrs(a: &mut VfAttrs, info: &smb2::client::FileInfo) {
        a.ftype = if info.is_directory {
            VfType::Directory
        } else {
            VfType::Regular
        };
        a.returned = AttrMask::empty();
        if a.masks.contains(AttrMask::MODE) {
            a.mode = if info.is_directory {
                libc::S_IFDIR | 0o777
            } else {
                libc::S_IFREG | 0o666
            };
            a.returned.insert(AttrMask::MODE);
        }
        if a.masks.contains(AttrMask::SIZE) {
            a.size = info.size;
            a.returned.insert(AttrMask::SIZE);
        }
        if a.masks.contains(AttrMask::ATIME) {
            (a.atime_sec, a.atime_nsec) = filetime_parts(info.accessed);
            a.returned.insert(AttrMask::ATIME);
        }
        if a.masks.contains(AttrMask::MTIME) {
            (a.mtime_sec, a.mtime_nsec) = filetime_parts(info.modified);
            a.returned.insert(AttrMask::MTIME);
        }
        // Only the portable fields projected above are reported here. Other
        // returned bits remain clear instead of inventing values.
    }
}
