use super::*;

impl VectorBackend for SmbVecFs {
    fn vopen_preflight_impl(&mut self, paths: &[&Path], flags: &[i32], modes: &[u32]) -> VfRes {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        validate_open_modes(flags, modes)
    }

    fn vopen_outcomes_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<FileOperand>> {
        self.vopen_preflight_impl(paths, flags, modes)?;
        self.drain_deferred_closes()?;
        let resolved: Vec<PathBuf> = paths.iter().map(|path| self.abs_path(path)).collect();
        if paths.len() <= 1 || self.tree.is_dfs || !paths_are_independent(&resolved) {
            let mut results = Vec::with_capacity(paths.len());
            for ((path, flags), mode) in paths.iter().zip(flags).zip(modes) {
                match self.open_raw_impl(path, *flags, *mode) {
                    Ok(file) => results.push(Ok(file)),
                    Err(error) => {
                        results.push(Err(error));
                        break;
                    }
                }
            }
            return Ok(ManyResults::new(paths.len(), results));
        }

        let mut requests = Vec::with_capacity(paths.len());
        for (index, (path, flags)) in resolved.iter().zip(flags).enumerate() {
            let path_string = self
                .path_string(path)
                .map_err(|error| error.with_index(index))?;
            let request = self
                .open_request(&path_string, *flags)
                .map_err(|error| error.with_index(index))?;
            requests.push(request);
        }
        let connection = self.client.connection_mut().clone();
        let tree_id = self.tree.tree_id;
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeDispatch { chunk: 0 })?;
        let jobs = requests.into_iter().map(|request| {
            let connection = connection.clone();
            async move {
                let frame = connection
                    .execute(Command::Create, &request, Some(tree_id))
                    .await
                    .map_err(|error| smb_error(error, 0))?;
                require_status(&frame, Command::Create, 0)?;
                let response = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(|error| smb_error(error, 0))?;
                Ok::<_, VfError>((response.file_id, response.end_of_file))
            }
        });
        let results = self.runtime.block_on(join_all(jobs));
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterReply { chunk: 0 }) {
            let closes = results.iter().filter_map(|result| {
                result.as_ref().ok().map(|(file_id, _)| {
                    let connection = connection.clone();
                    let file_id = *file_id;
                    async move {
                        let result =
                            close_on_connection_result(&connection, tree_id, file_id).await;
                        (file_id, result)
                    }
                })
            });
            for (file_id, result) in self.runtime.block_on(join_all(closes)) {
                if result.is_err() {
                    self.deferred_closes.push(file_id);
                }
            }
            return Err(error);
        }
        let mut output = Vec::with_capacity(paths.len());
        for (((path, flags), _mode), result) in
            resolved.into_iter().zip(flags).zip(modes).zip(results)
        {
            #[cfg(feature = "test-faults")]
            let index = output.len();
            let (file_id, size) = match result {
                Ok(value) => value,
                Err(error) => {
                    output.push(Err(error));
                    continue;
                }
            };
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeRegister { index }) {
                let _ = self.close_temporary_handle(file_id);
                output.push(Err(error));
                continue;
            }
            let access_mode = *flags & libc::O_ACCMODE;
            let fd = match self.insert_open_file(SmbOpen {
                file_id,
                path,
                cur_offset: if *flags & libc::O_APPEND != 0 {
                    size
                } else {
                    0
                },
                append: *flags & libc::O_APPEND != 0,
                readable: access_mode != libc::O_WRONLY,
                writable: access_mode != libc::O_RDONLY,
            }) {
                Ok(fd) => fd,
                Err(error) => {
                    let _ = self.close_temporary_handle(file_id);
                    output.push(Err(error));
                    continue;
                }
            };
            let file = FileOperand::from_fd(fd);
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterRegister { index }) {
                let _ = self.close_impl(&file);
                output.push(Err(error));
                continue;
            }
            output.push(Ok(file));
        }
        Ok(ManyResults::new(paths.len(), output))
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &FileOperand) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCleanup { index: _index })?;
        Ok(())
    }

    fn vclose_impl(&mut self, files: &[FileOperand]) -> VfRes {
        let mut file_ids = Vec::with_capacity(files.len());
        let mut fds = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let fd = file
                .fd()
                .ok_or_else(|| VfError::from_kind(index, std::io::ErrorKind::InvalidInput))?;
            let open = self
                .open_files
                .get(&fd)
                .ok_or_else(|| VfError::errno(index, ERR_EBADF))?;
            fds.push(fd);
            file_ids.push(open.file_id);
        }
        let connection = self.client.connection_mut().clone();
        let tree_id = self.tree.tree_id;
        let jobs = file_ids.into_iter().enumerate().map(|(index, file_id)| {
            let connection = connection.clone();
            #[cfg(not(feature = "test-faults"))]
            let _ = index;
            #[cfg(feature = "test-faults")]
            let injector = self.fault_injector.clone();
            async move {
                #[cfg(feature = "test-faults")]
                if let Some(injector) = injector {
                    injector.check(&OpenFaultPoint::BeforeCloseItem { index })?;
                }
                close_on_connection_result(&connection, tree_id, file_id).await
            }
        });
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCloseDispatch { index: 0 })?;
        let results = self.runtime.block_on(join_all(jobs));
        let mut first_error = None;
        for (index, result) in results.into_iter().enumerate() {
            let result = result.map_err(|error| error.with_index(index));
            match result {
                Ok(()) => {
                    self.open_files.remove(&fds[index]);
                }
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let max_read = self
            .client
            .params()
            .map(|params| params.max_read_size as usize)
            .unwrap_or(65_536);
        let concurrent = reads.len() > 1
            && reads.iter().all(|read| {
                !read.file.is_fd()
                    && !matches!(read.file, FileOperand::Saved)
                    && matches!(read.offset, VfOffset::At(_))
                    && read.length > 0
                    && read.length <= max_read
                    && read.length <= u32::MAX as usize
            });
        if concurrent {
            let connection = self.client.connection_mut().clone();
            let tree_id = self.tree.tree_id;
            let mut jobs = Vec::with_capacity(reads.len());
            for (index, read) in reads.iter().enumerate() {
                let path = self
                    .path_string(
                        &self
                            .file_path(&read.file)
                            .map_err(|e| e.with_index(index))?,
                    )
                    .map_err(|e| e.with_index(index))?;
                let create = self
                    .open_request(&path, libc::O_RDONLY)
                    .map_err(|e| e.with_index(index))?;
                let VfOffset::At(offset) = read.offset else {
                    unreachable!("concurrent read eligibility checked")
                };
                jobs.push(concurrent_compound_read(
                    connection.clone(),
                    tree_id,
                    create,
                    offset,
                    read.length,
                ));
            }
            let results = self.runtime.block_on(join_all(jobs));
            queue_deferred_closes(&results, &mut self.deferred_closes);
            if results.iter().any(|outcome| outcome.result.is_err())
                && self.client.is_disconnected()
            {
                let mut output = Vec::with_capacity(reads.len());
                for (index, read) in reads.iter().enumerate() {
                    output.push(
                        self.read_one(read)
                            .map_err(|error| error.with_index(index))?,
                    );
                }
                return Ok(output);
            }
            let mut output = Vec::with_capacity(reads.len());
            for (index, (read, result)) in reads.iter().zip(results).enumerate() {
                let data = result.result.map_err(|error| error.with_index(index))?;
                let VfOffset::At(offset) = read.offset else {
                    unreachable!("concurrent read eligibility checked")
                };
                output.push(ReadResult {
                    file: read.file.clone(),
                    offset,
                    eof: data.len() < read.length,
                    data,
                });
            }
            return Ok(output);
        }
        let mut output = Vec::with_capacity(reads.len());
        for (index, read) in reads.iter().enumerate() {
            output.push(self.read_one(read).map_err(|e| e.with_index(index))?);
        }
        Ok(output)
    }

    fn vwrite_impl(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> VfResult<Vec<WriteResult>> {
        #[cfg(feature = "test-faults")]
        {
            self.last_writev_was_concurrent = false;
        }
        let max_write = self
            .client
            .params()
            .map(|params| params.max_write_size as usize)
            .unwrap_or(65_536);
        let concurrent = writes.len() > 1
            && writes.iter().all(|write| {
                !write.file().is_fd()
                    && !matches!(write.file(), FileOperand::Saved)
                    && matches!(write.offset(), VfOffset::At(_))
                    && !write.creates()
                    && !write.truncates()
                    && !write.data().is_empty()
                    && write.data().len() <= max_write
                    && write.data().len() <= u32::MAX as usize
            });
        if concurrent {
            let mut prepared = Vec::with_capacity(writes.len());
            for (index, write) in writes.iter().enumerate() {
                let path = self
                    .path_string(
                        &self
                            .file_path(write.file())
                            .map_err(|e| e.with_index(index))?,
                    )
                    .map_err(|e| e.with_index(index))?;
                let create = self
                    .open_request(&path, libc::O_WRONLY)
                    .map_err(|e| e.with_index(index))?;
                let mut identity_create = self
                    .open_request(&path, libc::O_RDONLY)
                    .map_err(|e| e.with_index(index))?;
                identity_create.desired_access = FileAccessMask::new(
                    FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
                );
                let VfOffset::At(offset) = write.offset() else {
                    unreachable!("concurrent write eligibility checked")
                };
                prepared.push((create, identity_create, offset, write.data()));
            }
            let connection = self.client.connection_mut().clone();
            let tree_id = self.tree.tree_id;
            let identity_jobs = prepared.iter().map(|(_, create, _, _)| {
                concurrent_file_identity(connection.clone(), tree_id, create.clone())
            });
            let identity_outcomes = self.runtime.block_on(join_all(identity_jobs));
            let mut identity_values = Vec::with_capacity(identity_outcomes.len());
            let mut identity_failed = false;
            queue_deferred_closes(&identity_outcomes, &mut self.deferred_closes);
            for outcome in identity_outcomes {
                match outcome.result {
                    Ok(identity) => identity_values.push(identity),
                    Err(_) => identity_failed = true,
                }
            }
            let identities = (!identity_failed).then_some(identity_values);
            if identities
                .as_deref()
                .is_some_and(identities_are_independent)
            {
                #[cfg(feature = "test-faults")]
                {
                    self.last_writev_was_concurrent = true;
                }
                let jobs = prepared.into_iter().map(|(create, _, offset, data)| {
                    concurrent_compound_write(
                        connection.clone(),
                        tree_id,
                        create,
                        offset,
                        data.to_vec(),
                    )
                });
                let results = self.runtime.block_on(join_all(jobs));
                for outcome in &results {
                    if let Some(file_id) = &outcome.deferred_close {
                        self.deferred_closes.push(*file_id);
                    }
                }
                let mut output = Vec::with_capacity(writes.len());
                for (index, (write, result)) in writes.iter().zip(results).enumerate() {
                    let written = result.result.map_err(|error| error.with_index(index))?;
                    let VfOffset::At(offset) = write.offset() else {
                        unreachable!("concurrent write eligibility checked")
                    };
                    output.push(WriteResult {
                        file: write.file().clone(),
                        offset,
                        bytes_written: written,
                        stable: true,
                    });
                }
                return Ok(output);
            }
        }
        let mut output = Vec::with_capacity(writes.len());
        for (index, write) in writes.iter().enumerate() {
            output.push(self.write_one(*write).map_err(|e| e.with_index(index))?);
        }
        Ok(output)
    }

    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if attrs.len() > 1 && !self.tree.is_dfs && attrs.iter().all(|attrs| !attrs.file.is_fd()) {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let mut jobs = Vec::with_capacity(attrs.len());
            for (index, attrs) in attrs.iter().enumerate() {
                let path = self
                    .path_string(
                        &self
                            .file_path(&attrs.file)
                            .map_err(|e| e.with_index(index))?,
                    )
                    .map_err(|e| e.with_index(index))?;
                let tree = tree.clone();
                let mut connection = connection.clone();
                jobs.push(async move { tree.stat(&mut connection, &path).await });
            }
            let results = self.runtime.block_on(join_all(jobs));
            if results.iter().any(Result::is_err) && self.client.is_disconnected() {
                for (index, attrs) in attrs.iter_mut().enumerate() {
                    let path = self
                        .path_string(
                            &self
                                .file_path(&attrs.file)
                                .map_err(|error| error.with_index(index))?,
                        )
                        .map_err(|error| error.with_index(index))?;
                    let info = self
                        .client_stat(&path)
                        .map_err(|error| error.with_index(index))?;
                    Self::fill_attrs(attrs, &info);
                }
                return Ok(());
            }
            for (index, (attrs, result)) in attrs.iter_mut().zip(results).enumerate() {
                let info = result.map_err(|error| smb_error(error, index))?;
                let info = if info.attributes.0 & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    let path = self
                        .file_path(&attrs.file)
                        .and_then(|path| self.path_string(&path))
                        .map_err(|error| error.with_index(index))?;
                    self.follow_stat_link(&path, info)
                        .map_err(|error| error.with_index(index))?
                } else {
                    info
                };
                Self::fill_attrs(attrs, &info);
            }
            return Ok(());
        }
        for (index, attrs) in attrs.iter_mut().enumerate() {
            if let FileOperand::Fd(fd) = attrs.file {
                let file_id = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::errno(index, ERR_EBADF))?
                    .file_id;
                // No pathname reopen/retry: a lost handle must surface an error.
                let info = self
                    .query_handle_info(file_id)
                    .map_err(|error| error.with_index(index))?;
                Self::fill_attrs(attrs, &info);
                continue;
            }
            let path = self
                .path_string(
                    &self
                        .file_path(&attrs.file)
                        .map_err(|e| e.with_index(index))?,
                )
                .map_err(|e| e.with_index(index))?;
            let info = self.client_stat(&path).map_err(|e| e.with_index(index))?;
            Self::fill_attrs(attrs, &info);
        }
        Ok(())
    }

    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if attrs.is_empty() {
            Ok(())
        } else {
            // Some Samba versions ignore FILE_OPEN_REPARSE_POINT. Until
            // reliable no-follow support is established, never present
            // followed metadata as lstat data.
            Err(VfError::unsupported(0))
        }
    }

    fn exists_impl(&mut self, path: &Path) -> VfResult<bool> {
        let path = self.local_path_string(path)?;
        match self.client_stat(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn file_type_impl(&mut self, path: &Path) -> VfResult<VfType> {
        let path = self.local_path_string(path)?;
        let info = self.client_stat(&path)?;
        Ok(if info.is_directory {
            VfType::Directory
        } else {
            VfType::Regular
        })
    }

    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        for (index, attrs) in attrs.iter().enumerate() {
            let settable = AttrMask::SIZE | AttrMask::ATIME | AttrMask::MTIME;
            if !attrs.masks.difference(settable).is_empty() {
                return Err(VfError::unsupported(index));
            }
            if attrs.masks.contains(AttrMask::SIZE) {
                match attrs.file {
                    FileOperand::Fd(fd) => {
                        let id = self
                            .open_files
                            .get(&fd)
                            .ok_or_else(|| VfError::errno(index, ERR_EBADF))?
                            .file_id;
                        self.set_size_handle(id, attrs.size)
                            .map_err(|e| e.with_index(index))?;
                    }
                    _ => {
                        let path = self
                            .file_path(&attrs.file)
                            .map_err(|e| e.with_index(index))?;
                        self.set_size_path(&path, attrs.size)
                            .map_err(|e| e.with_index(index))?;
                    }
                }
            }
            if attrs.masks.intersects(AttrMask::ATIME | AttrMask::MTIME) {
                let atime = attrs
                    .masks
                    .contains(AttrMask::ATIME)
                    .then_some((attrs.atime_sec, attrs.atime_nsec));
                let mtime = attrs
                    .masks
                    .contains(AttrMask::MTIME)
                    .then_some((attrs.mtime_sec, attrs.mtime_nsec));
                match attrs.file {
                    FileOperand::Fd(fd) => {
                        let id = self
                            .open_files
                            .get(&fd)
                            .ok_or_else(|| VfError::errno(index, ERR_EBADF))?
                            .file_id;
                        self.set_times_handle(id, atime, mtime, index)?;
                    }
                    _ => {
                        let path = self
                            .file_path(&attrs.file)
                            .map_err(|e| e.with_index(index))?;
                        self.set_times_path(&path, atime, mtime, index)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        match attrs.iter().position(|attrs| !attrs.masks.is_empty()) {
            Some(index) => Err(VfError::unsupported(index)),
            None => Ok(()),
        }
    }

    fn listdir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        self.listdir_with_reparse_policy(dir, masks, max_count, recursive, false)
    }

    fn vrename_impl(&mut self, pairs: &[(FileOperand, FileOperand)]) -> VfRes {
        let mut prepared = Vec::with_capacity(pairs.len());
        let mut dependency_paths = Vec::with_capacity(pairs.len() * 2);
        for (index, (source, destination)) in pairs.iter().enumerate() {
            let source_path = self.file_path(source).map_err(|e| e.with_index(index))?;
            let destination_path = self
                .file_path(destination)
                .map_err(|e| e.with_index(index))?;
            let source = self
                .path_string(&source_path)
                .map_err(|e| e.with_index(index))?;
            let destination = self
                .path_string(&destination_path)
                .map_err(|e| e.with_index(index))?;
            dependency_paths.push(source_path);
            dependency_paths.push(destination_path);
            prepared.push((source, destination));
        }
        if prepared.len() > 1 && !self.tree.is_dfs && paths_are_independent(&dependency_paths) {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let jobs = prepared.into_iter().map(|(source, destination)| {
                let tree = tree.clone();
                let mut connection = connection.clone();
                async move { tree.rename(&mut connection, &source, &destination).await }
            });
            for (index, result) in self
                .runtime
                .block_on(join_all(jobs))
                .into_iter()
                .enumerate()
            {
                result.map_err(|error| smb_error(error, index))?;
            }
        } else {
            for (index, (source, destination)) in prepared.into_iter().enumerate() {
                self.runtime
                    .block_on(self.client.rename(&self.tree, &source, &destination))
                    .map_err(|e| smb_error(e, index))?;
            }
        }
        Ok(())
    }

    fn vremove_impl(&mut self, files: &[FileOperand]) -> VfRes {
        let mut prepared = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let path = self.file_path(file).map_err(|e| e.with_index(index))?;
            let path_string = self.path_string(&path).map_err(|e| e.with_index(index))?;
            let create = self
                .open_request(&path_string, libc::O_RDONLY)
                .map_err(|e| e.with_index(index))?;
            prepared.push((path, delete_entry_create_request(create)));
        }
        let path_list: Vec<PathBuf> = prepared.iter().map(|(path, _)| path.clone()).collect();
        let connection = self.client.connection_mut().clone();
        let tree_id = self.tree.tree_id;
        if prepared.len() > 1 && !self.tree.is_dfs && paths_are_independent(&path_list) {
            let jobs = prepared
                .into_iter()
                .map(|(_, create)| delete_entry_on_connection(connection.clone(), tree_id, create));
            let outcomes = self.runtime.block_on(join_all(jobs));
            queue_deferred_closes(&outcomes, &mut self.deferred_closes);
            for (index, outcome) in outcomes.into_iter().enumerate() {
                outcome.result.map_err(|error| error.with_index(index))?;
            }
        } else {
            for (index, (_, create)) in prepared.into_iter().enumerate() {
                let outcome = self.runtime.block_on(delete_entry_on_connection(
                    connection.clone(),
                    tree_id,
                    create,
                ));
                if let Some(file_id) = outcome.deferred_close {
                    self.deferred_closes.push(file_id);
                }
                outcome.result.map_err(|error| error.with_index(index))?;
            }
        }
        Ok(())
    }

    fn vmkdir_impl(&mut self, dirs: &[VfAttrs]) -> VfRes {
        validate_mkdir_modes(dirs)?;
        let mut prepared = Vec::with_capacity(dirs.len());
        for (index, attrs) in dirs.iter().enumerate() {
            let path = self
                .file_path(&attrs.file)
                .map_err(|e| e.with_index(index))?;
            let wire = self.path_string(&path).map_err(|e| e.with_index(index))?;
            prepared.push((path, wire));
        }
        let path_list: Vec<PathBuf> = prepared.iter().map(|(path, _)| path.clone()).collect();
        if prepared.len() > 1 && !self.tree.is_dfs && paths_are_independent(&path_list) {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let jobs = prepared.into_iter().map(|(_, path)| {
                let tree = tree.clone();
                let mut connection = connection.clone();
                async move { tree.create_directory(&mut connection, &path).await }
            });
            for (index, result) in self
                .runtime
                .block_on(join_all(jobs))
                .into_iter()
                .enumerate()
            {
                result.map_err(|error| smb_error(error, index))?;
            }
        } else {
            for (index, (_, path)) in prepared.into_iter().enumerate() {
                self.runtime
                    .block_on(self.client.create_directory(&self.tree, &path))
                    .map_err(|e| smb_error(e, index))?;
            }
        }
        Ok(())
    }

    fn vsymlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        if oldpaths.is_empty() {
            Ok(())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn vreadlink_impl(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
        if paths.is_empty() {
            Ok(Vec::new())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn vhardlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        if oldpaths.is_empty() {
            Ok(())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn vcopy_data_impl(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (index, pair) in pairs.iter().enumerate() {
            self.copy_client_side(pair)
                .map_err(|e| e.with_index(index))?;
        }
        Ok(())
    }

    fn vcopy_impl(&mut self, pairs: &[ExtentPair], options: CopyOptions) -> VfRes {
        if pairs.is_empty() {
            return Ok(());
        }
        if !options.follow_source_symlinks() {
            return Err(VfError::unsupported(0));
        }
        if !self.server_copy_enabled {
            return self.vcopy_data_impl(pairs);
        }
        for (index, pair) in pairs.iter().enumerate() {
            let source = self
                .local_path_string(&pair.src_path)
                .map_err(|e| e.with_index(index))?;
            let destination = self
                .local_path_string(&pair.dst_path)
                .map_err(|e| e.with_index(index))?;
            let source_size = self
                .client_stat(&source)
                .map_err(|e| e.with_index(index))?
                .size;
            let length = pair
                .length
                .unwrap_or_else(|| source_size.saturating_sub(pair.src_offset))
                .min(source_size.saturating_sub(pair.src_offset));
            let result = self
                .runtime
                .block_on(self.client.server_side_copy_file_range(
                    &self.tree,
                    &source,
                    pair.src_offset,
                    &destination,
                    pair.dst_offset,
                    length,
                ));
            match result {
                Ok(copied) => self
                    .set_size_path(
                        &pair.dst_path,
                        checked_offset(pair.dst_offset, copied, index)?,
                    )
                    .map_err(|e| e.with_index(index))?,
                Err(error) if error.kind() == SmbErrorKind::Unsupported => {
                    self.server_copy_enabled = false;
                    return self
                        .vcopy_data_impl(&pairs[index..])
                        .map_err(|e| e.map_index(|relative| index + relative));
                }
                Err(error) => return Err(smb_error(error, index)),
            }
        }
        Ok(())
    }

    fn copy_tree_impl(
        &mut self,
        source: &Path,
        destination: &Path,
        symlinks: bool,
        use_server_side_copy: bool,
    ) -> VfRes {
        vfsi_sync::backend::helpers::copy_tree_with_listdir(
            self,
            source,
            destination,
            symlinks,
            AttrMask::MODE | AttrMask::SIZE,
            |backend, path, masks| {
                backend.listdir_with_reparse_policy(path, masks, 0, false, symlinks)
            },
            |backend, pair| {
                let pairs = std::slice::from_ref(pair);
                if use_server_side_copy {
                    backend.vcopy_impl(pairs, vfsi_core::CopyOptions::new())
                } else {
                    backend.vcopy_data_impl(pairs)
                }
            },
            std::convert::identity,
        )
    }
    fn vwrite_adb_impl(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let mut counts = Vec::with_capacity(patterns.len());
        for (index, pattern) in patterns.iter().enumerate() {
            let layout = vfsi_core::internal::adb_layout(pattern, index)?;
            let file = self
                .open_raw_impl(&pattern.path, libc::O_WRONLY | libc::O_CREAT, 0o666)
                .map_err(|e| e.with_index(index))?;
            let result = (|| {
                for (block_number, number_offset, pattern_offset) in layout {
                    if let Some(offset) = number_offset {
                        self.write_raw_impl(&file, offset, &block_number.to_be_bytes())?;
                    }
                    if let Some(offset) = pattern_offset
                        && !pattern.adb_pattern_data.is_empty()
                    {
                        self.write_raw_impl(&file, offset, &pattern.adb_pattern_data)?;
                    }
                }
                Ok::<_, VfError>(pattern.adb_block_count)
            })();
            let close = self.close_impl(&file);
            counts.push(result.map_err(|e| e.with_index(index))?);
            close.map_err(|e| e.with_index(index))?;
        }
        Ok(counts)
    }
}
