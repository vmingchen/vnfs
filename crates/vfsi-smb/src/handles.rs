use super::*;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

impl HandleBackend for SmbVecFs {
    fn vclose_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCloseDispatch { index: 0 })?;
        let jobs = files.iter().enumerate().map(|(index, file)| {
            let prepared = file
                .fd()
                .ok_or_else(|| VfError::from_kind(None, std::io::ErrorKind::InvalidInput))
                .and_then(|fd| {
                    self.open_files
                        .get(&fd)
                        .map(|open| (fd, open.file_id, open.tree.clone()))
                        .ok_or_else(|| VfError::errno(None, ERR_EBADF))
                });
            #[cfg(not(feature = "test-faults"))]
            let _ = index;
            #[cfg(feature = "test-faults")]
            let injector = self.fault_injector.clone();
            async move {
                let (fd, file_id, tree) = prepared.map_err(OpError::not_attempted)?;
                #[cfg(feature = "test-faults")]
                if let Some(injector) = &injector {
                    injector
                        .check(&OpenFaultPoint::BeforeCloseItem { index })
                        .map_err(OpError::not_attempted)?;
                }
                tree.close_file(file_id).await?;
                #[cfg(feature = "test-faults")]
                if let Some(injector) = &injector {
                    injector
                        .check(&OpenFaultPoint::AfterCloseReply { index })
                        .map_err(|error| OpError::unknown(error, None))?;
                }
                Ok::<_, vfsi_core::OpError>(fd)
            }
        });
        let outcomes = self.runtime.block_on(join_all(jobs));
        Ok(outcomes
            .into_iter()
            .zip(files)
            .map(|(outcome, file)| {
                match &outcome {
                    Ok(fd) => {
                        self.open_files.remove(fd);
                    }
                    Err(error) if error.state() != vfsi_core::FailureState::NotAttempted => {
                        // No further I/O may use an ID whose CLOSE was attempted.
                        // The backend retains ownership until original-tree teardown.
                        if let Some(open) = file.fd().and_then(|fd| self.open_files.remove(&fd)) {
                            self.deferred_closes.push(DeferredClose::new(
                                open.tree,
                                CloseState::NoReplay(open.file_id),
                            ));
                        }
                    }
                    Err(_) => {}
                }
                outcome.map(|_| ())
            })
            .collect())
    }

    fn close_deferred(&mut self, file: &FileOperand) -> VfResult<()> {
        let fd = file
            .fd()
            .ok_or_else(|| VfError::from_kind(0, std::io::ErrorKind::InvalidInput))?;
        if self.open_files.contains_key(&fd) {
            self.close_impl(file)
        } else {
            // Submitted closes transfer uncertain ownership to the backend;
            // draining never resubmits an already-attempted FileId.
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
        OsString::from_vec(normalize_bytes(path_bytes(&root_relative))).into()
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
            VfPathBase::Abs => OsString::from_vec(normalize_bytes(path_bytes(pathname))).into(),
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
            tree: self.tree.clone(),
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
        let result = vfsi_core::internal::collect_vec_result(
            self.vclose_impl(std::slice::from_ref(file)),
            1,
        )
        .map(|_| ());
        if result.is_err()
            && let Some(open) = file.fd().and_then(|fd| self.open_files.remove(&fd))
        {
            // A scalar backend close consumes cleanup ownership even if
            // submission was rejected. Borrowed vclose keeps unsent handles.
            self.deferred_closes.push(DeferredClose::new(
                open.tree,
                CloseState::Ready(open.file_id),
            ));
        }
        result
    }

    fn vfsync_impl(
        &mut self,
        files: &[FileOperand],
        _mode: vfsi_core::api::SyncMode,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.client.connection_mut().clone();
        let tree_id = self.tree.tree_id;
        let mut output: Vec<_> = (0..files.len()).map(|_| None).collect();
        let mut indices = Vec::with_capacity(files.len());
        let mut jobs = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let prepared = file
                .fd()
                .ok_or_else(|| VfError::from_kind(None, std::io::ErrorKind::InvalidInput))
                .and_then(|fd| self.live_file(fd).map(|open| open.file_id));
            match prepared {
                Err(error) => output[index] = Some(Err(OpError::not_attempted(error))),
                Ok(file_id) => {
                    indices.push(index);
                    let connection = connection.clone();
                    #[cfg(feature = "test-faults")]
                    let injector = self.fault_injector.clone();
                    jobs.push(async move {
                        #[cfg(feature = "test-faults")]
                        if let Some(injector) = &injector {
                            injector
                                .check(&OpenFaultPoint::BeforeSyncDispatch { index })
                                .map_err(OpError::not_attempted)?;
                        }
                        let result = flush_file(connection, tree_id, file_id).await;
                        #[cfg(feature = "test-faults")]
                        if result.is_ok()
                            && let Some(injector) = &injector
                        {
                            injector
                                .check(&OpenFaultPoint::AfterSyncReply { index })
                                .map_err(|cause| OpError::unknown(cause, None))?;
                        }
                        result
                    });
                }
            }
        }
        for (index, result) in indices
            .into_iter()
            .zip(self.runtime.block_on(join_all(jobs)))
        {
            output[index] = Some(result);
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every flush input assigned"))
            .collect())
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
        let open = self.live_file(fd)?;
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
    fn vsetattrs_impl(
        &mut self,
        updates: &[SetAttrsOp<Target<'_, FileOperand>>],
    ) -> vfsi_core::VecResult<()> {
        vfsi_sync::backend::helpers::vsetattrs_typed_default(self, updates)
    }
}
