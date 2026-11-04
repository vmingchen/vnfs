use super::*;

impl HandleBackend for SmbVecFs {
    fn close_deferred(&mut self, file: &FileOperand) -> VfResult<()> {
        let fd = file
            .fd()
            .ok_or_else(|| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
        if self.open_files.contains_key(&fd) {
            self.close_impl(file)
        } else {
            // Failed scalar closes retain their remote FileId in this queue;
            // successful vector closes have already retired their descriptor.
            self.drain_deferred_closes()
        }
    }

    fn capabilities(&self) -> Capabilities {
        if self.server_copy_enabled {
            Capabilities::SERVER_COPY
        } else {
            Capabilities::empty()
        }
    }

    fn abs_path(&self, path: &Path) -> PathBuf {
        let root_relative = if path.is_absolute() {
            path.strip_prefix("/").unwrap_or(path).to_path_buf()
        } else {
            self.cwd.join(path)
        };
        path_from_bytes(&normalize_bytes(path_bytes(&root_relative)))
    }

    fn open_path_impl(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<FileOperand> {
        validate_create_mode(flags, mode, 0)?;
        let path = match base {
            VfPathBase::Abs => path_from_bytes(&normalize_bytes(path_bytes(pathname))),
            VfPathBase::Cwd => self.abs_path(pathname),
        };
        let path_string = self.path_string(&path)?;
        let (file_id, size) = self.raw_open(&path_string, flags)?;
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeRegister { index: 0 }) {
            let _ = self.close_temporary_handle(file_id);
            return Err(error);
        }
        let access_mode = flags & libc::O_ACCMODE;
        let open = SmbOpen {
            file_id,
            path,
            cur_offset: if flags & libc::O_APPEND != 0 { size } else { 0 },
            append: flags & libc::O_APPEND != 0,
            readable: access_mode != libc::O_WRONLY,
            writable: access_mode != libc::O_RDONLY,
        };
        match self.insert_open_file(open) {
            Ok(fd) => Ok(FileOperand::from_fd(fd)),
            Err(error) => {
                let _ = self.close_temporary_handle(file_id);
                Err(error)
            }
        }
    }

    fn close_impl(&mut self, file: &FileOperand) -> VfResult<()> {
        let fd = file
            .fd()
            .ok_or_else(|| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
        let open = self
            .open_files
            .remove(&fd)
            .ok_or_else(|| VfError::errno(0, ERR_EBADF))?;
        let file_id = open.file_id;
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeCloseDispatch { index: 0 })
        {
            self.deferred_closes.push(file_id);
            return Err(error);
        }
        match self.raw_close(file_id) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.deferred_closes.push(file_id);
                Err(error)
            }
        }
    }

    fn sync_data(&mut self, file: &FileOperand) -> VfResult<()> {
        let fd = file
            .fd()
            .ok_or_else(|| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
        let file_id = self
            .open_files
            .get(&fd)
            .ok_or_else(|| VfError::errno(0, ERR_EBADF))?
            .file_id;
        self.raw_flush(file_id)
    }

    fn chdir(&mut self, path: &Path) -> VfResult<()> {
        let attrs = self.stat_impl(path)?;
        if attrs.ftype != VfType::Directory {
            return Err(VfError::errno(0, ERR_NOTDIR));
        }
        self.cwd = self.abs_path(path);
        Ok(())
    }

    fn getcwd(&self) -> PathBuf {
        Path::new("/").join(&self.cwd)
    }

    fn seek_raw_impl(
        &mut self,
        file: &FileOperand,
        offset: i64,
        whence: SeekFrom,
    ) -> VfResult<i64> {
        let fd = file
            .fd()
            .ok_or_else(|| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
        let open = self
            .open_files
            .get(&fd)
            .ok_or_else(|| VfError::errno(0, ERR_EBADF))?;
        let base = match whence {
            SeekFrom::Set => 0i128,
            SeekFrom::Cur => open.cur_offset as i128,
            SeekFrom::End => self.query_size(open.file_id)? as i128,
            _ => return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput)),
        };
        let new = base + offset as i128;
        if !(0..=i64::MAX as i128).contains(&new) {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        self.open_files
            .get_mut(&fd)
            .expect("descriptor validated")
            .cur_offset = new as u64;
        Ok(new as i64)
    }
    fn read_file_impl(&mut self, file: &FileOperand, max_bytes: usize) -> VfResult<Vec<u8>> {
        vfsi_sync::backend::helpers::native_read_file_impl_default(self, file, max_bytes)
    }
    fn open_impl(&mut self, request: &OpenOp) -> VfResult<FileOperand> {
        vfsi_sync::backend::helpers::native_open_impl_default(self, request)
    }
    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        vfsi_sync::backend::helpers::native_read_impl_default(self, request)
    }
    fn read_into_impl(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        vfsi_sync::backend::helpers::native_read_into_impl_default(self, request, buffer)
    }
    fn write_impl(&mut self, request: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteResult> {
        vfsi_sync::backend::helpers::native_write_impl_default(self, request)
    }
    fn seek_impl(&mut self, file: &FileOperand, position: std::io::SeekFrom) -> VfResult<u64> {
        vfsi_sync::backend::helpers::native_seek_impl_default(self, file, position)
    }
    fn metadata_impl(
        &mut self,
        target: Target<'_, FileOperand>,
        options: vfsi_core::api::AttrsOptions,
    ) -> VfResult<VfAttrs> {
        vfsi_sync::backend::helpers::native_metadata_impl_default(self, target, options)
    }
    fn set_attributes_impl(
        &mut self,
        update: &SetAttrsOp<Target<'_, FileOperand>>,
    ) -> VfResult<()> {
        vfsi_sync::backend::helpers::native_set_attributes_impl_default(self, update)
    }
    fn vsetattrs_impl(&mut self, updates: &[SetAttrsOp<Target<'_, FileOperand>>]) -> VfResult<()> {
        vfsi_sync::backend::helpers::vsetattrs_typed_default(self, updates)
    }
}
