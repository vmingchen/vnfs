use super::*;

impl VectorBackend for SmbVecFs {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_core::VecResult<FileOperand> {
        self.vopen_preflight_impl(paths, flags, modes)?;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let _ = self.drain_deferred_closes();
        use vfsi_core::OpError;
        let resolved: Vec<PathBuf> = paths.iter().map(|path| self.abs_path(path)).collect();
        let concurrent = !self.tree.is_dfs && paths_are_independent(&resolved);
        let connection = self.client.connection_mut().clone();
        #[cfg(feature = "test-faults")]
        let tree = self.tree.clone();
        let tree_id = self.tree.tree_id;
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeDispatch { chunk: 0 })?;
        let requests = resolved.iter().zip(flags).zip(modes).enumerate().map(
            |(index, ((path, flags), mode))| {
                validate_create_mode(*flags, *mode, index)
                    .and_then(|()| self.path_string(path))
                    .and_then(|path| self.open_request(&path, *flags))
                    .map_err(OpError::not_attempted)
            },
        );
        let jobs = requests.map(|request| {
            let connection = connection.clone();

            async move {
                let request = request?;
                let frame = connection
                    .execute(Command::Create, &request, Some(tree_id))
                    .await
                    .map_err(|error| protocol::wire_failure(smb_error(error, 0), None))?;
                require_status(&frame, Command::Create, 0)
                    .map_err(|error| protocol::wire_failure(error, None))?;
                let response =
                    CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(|error| {
                        OpError::unknown(VfError::invalid_reply(None, error.to_string()), None)
                    })?;
                Ok::<_, OpError>((response.file_id, response.end_of_file))
            }
        });
        let results = self.runtime.block_on(async {
            if concurrent {
                return join_all(jobs).await;
            }
            // Keep the existing alias/DFS scheduling policy. Each input still
            // has its own outcome, and ordinary failures do not stop siblings.
            let mut results = Vec::with_capacity(paths.len());
            for job in jobs {
                let result = job.await;
                let fatal = result
                    .as_ref()
                    .err()
                    .filter(|error| error.state() == vfsi_core::FailureState::Unknown)
                    .map(|error| error.cause().clone());
                results.push(result);
                if let Some(cause) = fatal {
                    // Do not even construct requests for an untouched suffix.
                    results.extend(
                        (results.len()..paths.len())
                            .map(|_| Err(OpError::not_attempted(cause.clone()))),
                    );
                    break;
                }
            }
            results
        });
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterReply { chunk: 0 }) {
            let closes = results.iter().filter_map(|result| {
                result.as_ref().ok().map(|(file_id, _)| {
                    let tree = tree.clone();
                    let file_id = *file_id;
                    async move {
                        let result = tree.close_file(file_id).await;
                        (file_id, result)
                    }
                })
            });
            for (file_id, result) in self.runtime.block_on(join_all(closes)) {
                if result.is_err() {
                    self.deferred_closes.push(DeferredClose::new(
                        self.tree.clone(),
                        CloseState::NoReplay(file_id),
                    ));
                }
            }
            return Ok(results
                .into_iter()
                .map(|result| match result {
                    Err(error) if error.state() == vfsi_core::FailureState::NotAttempted => {
                        Err(error)
                    }
                    _ => Err(OpError::unknown(error.clone(), None)),
                })
                .collect());
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
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterOpenReply { index }) {
                let _ = self.close_temporary_handle(file_id);
                output.push(Err(OpError::unknown(error, None)));
                continue;
            }
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeRegister { index }) {
                let _ = self.close_temporary_handle(file_id);
                output.push(Err(OpError::failed(error, None)));
                continue;
            }
            let access_mode = *flags & libc::O_ACCMODE;
            let fd = match self.insert_open_file(SmbOpen {
                file_id,
                tree: self.tree.clone(),
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
                    output.push(Err(OpError::failed(error, None)));
                    continue;
                }
            };
            let file = FileOperand::from_fd(fd);
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterRegister { index }) {
                let _ = self.close_impl(&file);
                output.push(Err(OpError::failed(error, None)));
                continue;
            }
            output.push(Ok(file));
        }
        Ok(output)
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &FileOperand) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCleanup { index: _index })?;
        Ok(())
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> vfsi_core::VecResult<ReadResult> {
        self.read_results(reads)
    }

    fn vwrite_impl(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_core::VecResult<WriteResult> {
        self.write_results(writes)
    }

    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let classify = |error: VfError| {
            if error.is_transport() {
                OpError::unknown(error, None)
            } else {
                OpError::failed(error, None)
            }
        };
        if attrs.len() > 1 && !self.tree.is_dfs && attrs.iter().all(|attr| !attr.file.is_fd()) {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let mut outcomes: Vec<Option<vfsi_core::OpResult<()>>> =
                (0..attrs.len()).map(|_| None).collect();
            let mut indices = Vec::with_capacity(attrs.len());
            let mut jobs = Vec::with_capacity(attrs.len());
            for (index, attr) in attrs.iter().enumerate() {
                match self
                    .file_path(&attr.file)
                    .and_then(|path| self.path_string(&path))
                {
                    Ok(path) => {
                        indices.push(index);
                        let tree = tree.clone();
                        let mut connection = connection.clone();
                        jobs.push(async move { tree.stat(&mut connection, &path).await });
                    }
                    Err(error) => outcomes[index] = Some(Err(OpError::not_attempted(error))),
                }
            }
            let results = self.runtime.block_on(join_all(jobs));
            let disconnected = results.iter().any(Result::is_err) && self.client.is_disconnected();
            for (index, result) in indices.into_iter().zip(results) {
                let result = (|| {
                    let info = match result {
                        Ok(info) => info,
                        // Rebuild a path only for failed/reparse inputs; do not
                        // clone every successful request's path for bookkeeping.
                        Err(_) if disconnected => {
                            let path = self
                                .file_path(&attrs[index].file)
                                .and_then(|path| self.path_string(&path))?;
                            self.client_stat(&path)?
                        }
                        Err(error) => return Err(smb_error(error, index)),
                    };
                    if info.attributes.0 & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        let path = self
                            .file_path(&attrs[index].file)
                            .and_then(|path| self.path_string(&path))?;
                        self.follow_stat_link(&path, info)
                    } else {
                        Ok(info)
                    }
                })();
                outcomes[index] = Some(
                    result
                        .map(|info| Self::fill_attrs(&mut attrs[index], &info))
                        .map_err(classify),
                );
            }
            return Ok(outcomes
                .into_iter()
                .map(|slot| slot.expect("every SMB metadata input assigned"))
                .collect());
        }
        let mut outcomes = Vec::with_capacity(attrs.len());
        for attr in attrs.iter_mut() {
            let retained = if let FileOperand::Fd(fd) = attr.file {
                match self.live_file(fd) {
                    Ok(open) => Some(open.file_id),
                    Err(cause) => {
                        outcomes.push(Err(OpError::not_attempted(cause)));
                        continue;
                    }
                }
            } else {
                None
            };
            let path = if retained.is_none() {
                match self
                    .file_path(&attr.file)
                    .and_then(|path| self.path_string(&path))
                {
                    Ok(path) => Some(path),
                    Err(cause) => {
                        outcomes.push(Err(OpError::not_attempted(cause)));
                        continue;
                    }
                }
            } else {
                None
            };
            let result = (|| {
                if let Some(file_id) = retained {
                    let info = self.query_handle_info(file_id)?;
                    Self::fill_attrs(attr, &info);
                } else {
                    let info = self.client_stat(path.as_deref().expect("path preflighted"))?;
                    Self::fill_attrs(attr, &info);
                }
                Ok(())
            })();
            outcomes.push(result.map_err(classify));
        }
        Ok(outcomes)
    }

    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        if attrs.is_empty() {
            Ok(Vec::new())
        } else {
            // Preserve the existing unsupported guarantee for Samba servers
            // that ignore FILE_OPEN_REPARSE_POINT; do not fabricate lstat data.
            Err(VfError::unsupported(None))
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

    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let mut unavailable: Option<VfError> = None;
        Ok(attrs
            .iter()
            .enumerate()
            .map(|(index, attrs)| {
                let settable = AttrMask::SIZE | AttrMask::ATIME | AttrMask::MTIME;
                if !attrs.masks.difference(settable).is_empty() {
                    return Err(OpError::not_attempted(VfError::unsupported(None)));
                }
                if attrs.masks.is_empty() {
                    return Ok(());
                }
                let retained = if let FileOperand::Fd(fd) = attrs.file {
                    Some(self.live_file(fd).map_err(OpError::not_attempted)?.file_id)
                } else {
                    None
                };
                let path = if retained.is_none() {
                    Some(
                        self.file_path(&attrs.file)
                            .and_then(|path| self.path_string(&path))
                            .map_err(OpError::not_attempted)?,
                    )
                } else {
                    None
                };
                // Validate both timestamp fields before SIZE can mutate the file.
                for (mask, seconds, nanos) in [
                    (AttrMask::ATIME, attrs.atime_sec, attrs.atime_nsec),
                    (AttrMask::MTIME, attrs.mtime_sec, attrs.mtime_nsec),
                ] {
                    if attrs.masks.contains(mask) {
                        Self::filetime_value(seconds, nanos, index)
                            .map_err(OpError::not_attempted)?;
                    }
                }
                if let Some(cause) = &unavailable {
                    return Err(OpError::not_attempted(cause.clone()));
                }
                (|| {
                    if attrs.masks.contains(AttrMask::SIZE) {
                        match attrs.file {
                            FileOperand::Fd(_) => {
                                let id = retained.expect("descriptor validated before dispatch");
                                self.set_size_handle(id, attrs.size)
                                    .map_err(|e| e.with_index(index))?;
                            }
                            _ => {
                                self.set_size_encoded_path(
                                    path.as_deref().expect("path preflighted"),
                                    attrs.size,
                                )
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
                            FileOperand::Fd(_) => {
                                let id = retained.expect("descriptor validated before dispatch");
                                self.set_times_handle(id, atime, mtime, index)?;
                            }
                            _ => {
                                self.set_times_path(
                                    path.as_deref().expect("path preflighted"),
                                    atime,
                                    mtime,
                                    index,
                                )?;
                            }
                        }
                    }
                    Ok(())
                })()
                .map_err(|cause: VfError| {
                    if cause.is_transport() {
                        unavailable = Some(cause.clone());
                        OpError::unknown(cause, None)
                    } else {
                        OpError::failed(cause, None)
                    }
                })
            })
            .collect())
    }

    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        Ok(attrs
            .iter()
            .map(|attrs| {
                if attrs.masks.is_empty() {
                    Ok(())
                } else {
                    Err(vfsi_core::OpError::not_attempted(VfError::unsupported(
                        None,
                    )))
                }
            })
            .collect())
    }

    fn listdir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        self.listdir_collect(dir, masks, max_count, recursive)
    }

    fn listdir_page_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        _max_entries: usize,
        follow_symlinks: bool,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        if !follow_symlinks && cursor.is_none() {
            // Ordinary SMB path opens do not guarantee no-follow for every component.
            return Err(VfError::unsupported(0));
        }
        self.read_directory_page(dir, masks, cursor, page_size)
    }

    fn vrename_impl(&mut self, pairs: &[(FileOperand, FileOperand)]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        self.drain_deferred_closes()?;
        let mut slots = (0..pairs.len()).map(|_| None).collect::<Vec<_>>();
        let mut indices = Vec::with_capacity(pairs.len());
        let mut prepared = Vec::with_capacity(pairs.len());
        let mut dependency_paths = Vec::with_capacity(pairs.len() * 2);
        for (index, (source, destination)) in pairs.iter().enumerate() {
            let request = (|| {
                let source_path = self.file_path(source)?;
                let destination_path = self.file_path(destination)?;
                let source = self.path_string(&source_path)?;
                let destination = self.path_string(&destination_path)?;
                let mut create =
                    protocol::mutation_create_request(self.open_request(&source, libc::O_RDONLY)?);
                // Match the SDK's rename access requirements; no data-read or
                // SYNCHRONIZE grant is required for this compound.
                create.desired_access = FileAccessMask::new(
                    FileAccessMask::DELETE | FileAccessMask::FILE_READ_ATTRIBUTES,
                );
                dependency_paths.extend([source_path, destination_path]);
                Ok((destination, create))
            })();
            match request {
                Ok(request) => {
                    indices.push(index);
                    prepared.push(request);
                }
                Err(cause) => slots[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        let classify = |cause: VfError| {
            if cause.is_transport() {
                OpError::unknown(cause, None)
            } else {
                OpError::failed(cause, None)
            }
        };
        let connection = self.client.connection_mut().clone();
        let tree = self.tree.clone();
        if prepared.len() > 1 && paths_are_independent(&dependency_paths) {
            let jobs =
                indices
                    .iter()
                    .copied()
                    .zip(prepared)
                    .map(|(index, (destination, create))| {
                        let connection = connection.clone();
                        let tree = tree.clone();
                        #[cfg(feature = "test-faults")]
                        let injector = self.fault_injector.clone();
                        async move {
                            #[cfg(not(feature = "test-faults"))]
                            let _ = index;
                            #[cfg(feature = "test-faults")]
                            if let Some(injector) = &injector
                                && let Err(cause) = injector
                                    .check(&OpenFaultPoint::BeforeRenameCompound { first: index })
                            {
                                return protocol::CompoundOutcome {
                                    result: Err(OpError::not_attempted(cause)),
                                    deferred_close: None,
                                };
                            }
                            let outcome = protocol::rename_entry_on_connection(
                                connection,
                                tree.clone(),
                                create,
                                &destination,
                            )
                            .await;
                            let result = outcome.result.map_err(classify);
                            #[cfg(feature = "test-faults")]
                            let result = result.and_then(|()| {
                                if let Some(injector) = &injector {
                                    injector
                                        .check(&OpenFaultPoint::AfterRenameCompoundReply {
                                            first: index,
                                        })
                                        .map_err(|cause| OpError::unknown(cause, None))?;
                                }
                                Ok(())
                            });
                            protocol::CompoundOutcome {
                                result,
                                deferred_close: outcome.deferred_close,
                            }
                        }
                    });
            let outcomes = self.runtime.block_on(join_all(jobs));
            protocol::queue_deferred_closes(&outcomes, &mut self.deferred_closes, |state| {
                DeferredClose::new(self.tree.clone(), state)
            });
            for (index, outcome) in indices.into_iter().zip(outcomes) {
                slots[index] = Some(outcome.result);
            }
        } else {
            let mut unavailable: Option<VfError> = None;
            for (index, (destination, create)) in indices.into_iter().zip(prepared) {
                #[cfg(feature = "test-faults")]
                if unavailable.is_none()
                    && let Err(cause) = self
                        .inject_open_fault(OpenFaultPoint::BeforeRenameCompound { first: index })
                {
                    if cause.is_transport() {
                        unavailable = Some(cause.clone());
                    }
                    slots[index] = Some(Err(OpError::not_attempted(cause)));
                    continue;
                }
                let result = if let Some(cause) = &unavailable {
                    slots[index] = Some(Err(OpError::not_attempted(cause.clone())));
                    continue;
                } else {
                    let outcome = self.runtime.block_on(protocol::rename_entry_on_connection(
                        connection.clone(),
                        tree.clone(),
                        create,
                        &destination,
                    ));
                    if let Some(file_id) = outcome.deferred_close {
                        self.deferred_closes
                            .push(DeferredClose::new(self.tree.clone(), file_id));
                    }
                    outcome.result
                };
                let result = result.map_err(classify);
                #[cfg(feature = "test-faults")]
                let result = result.and_then(|()| {
                    self.inject_open_fault(OpenFaultPoint::AfterRenameCompoundReply {
                        first: index,
                    })
                    .map_err(|cause| OpError::unknown(cause, None))
                });
                if let Err(error) = &result
                    && error.cause().is_transport()
                {
                    unavailable = Some(error.cause().clone());
                }
                slots[index] = Some(result);
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all SMB rename inputs assigned"))
            .collect())
    }

    fn vremove_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let mut slots = (0..files.len()).map(|_| None).collect::<Vec<_>>();
        let mut prepared = Vec::new();
        for (index, file) in files.iter().enumerate() {
            let request = (|| {
                let path = self.file_path(file)?;
                let wire = self.path_string(&path)?;
                let create = mutation_create_request(self.open_request(&wire, libc::O_RDONLY)?);
                Ok((path, create))
            })();
            match request {
                Ok((path, create)) => prepared.push((index, path, create)),
                Err(cause) => slots[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        let paths: Vec<_> = prepared.iter().map(|(_, path, _)| path.clone()).collect();
        let connection = self.client.connection_mut().clone();
        let tree = self.tree.clone();
        let classify = |cause: VfError| {
            if cause.is_transport() {
                OpError::unknown(cause, None)
            } else {
                OpError::failed(cause, None)
            }
        };
        if prepared.len() > 1 && paths_are_independent(&paths) {
            let jobs = prepared.into_iter().map(|(index, _, create)| {
                let connection = connection.clone();
                let tree = tree.clone();
                #[cfg(feature = "test-faults")]
                let injector = self.fault_injector.clone();
                async move {
                    #[cfg(feature = "test-faults")]
                    if let Some(injector) = &injector
                        && let Err(cause) =
                            injector.check(&OpenFaultPoint::BeforeRemoveCompound { first: index })
                    {
                        return (
                            index,
                            protocol::CompoundOutcome {
                                result: Err(OpError::not_attempted(cause)),
                                deferred_close: None,
                            },
                        );
                    }
                    let outcome =
                        delete_entry_on_connection(connection, tree.clone(), create).await;
                    let result = outcome.result.map_err(classify);
                    #[cfg(feature = "test-faults")]
                    let result = result.and_then(|()| {
                        if let Some(injector) = &injector {
                            injector
                                .check(&OpenFaultPoint::AfterRemoveCompoundReply { first: index })
                                .map_err(|cause| OpError::unknown(cause, None))?;
                        }
                        Ok(())
                    });
                    (
                        index,
                        protocol::CompoundOutcome {
                            result,
                            deferred_close: outcome.deferred_close,
                        },
                    )
                }
            });
            for (index, outcome) in self.runtime.block_on(join_all(jobs)) {
                if let Some(id) = outcome.deferred_close {
                    self.deferred_closes
                        .push(DeferredClose::new(self.tree.clone(), id));
                }
                slots[index] = Some(outcome.result);
            }
        } else {
            let mut fatal: Option<VfError> = None;
            for (index, _, create) in prepared {
                if let Some(cause) = &fatal {
                    slots[index] = Some(Err(OpError::not_attempted(cause.clone())));
                    continue;
                }
                #[cfg(feature = "test-faults")]
                if let Err(cause) =
                    self.inject_open_fault(OpenFaultPoint::BeforeRemoveCompound { first: index })
                {
                    slots[index] = Some(Err(OpError::not_attempted(cause)));
                    continue;
                }
                let outcome = self.runtime.block_on(delete_entry_on_connection(
                    connection.clone(),
                    tree.clone(),
                    create,
                ));
                if let Some(id) = outcome.deferred_close {
                    self.deferred_closes
                        .push(DeferredClose::new(self.tree.clone(), id));
                }
                let result = outcome.result.map_err(classify);
                #[cfg(feature = "test-faults")]
                let result = result.and_then(|()| {
                    self.inject_open_fault(OpenFaultPoint::AfterRemoveCompoundReply {
                        first: index,
                    })
                    .map_err(|cause| OpError::unknown(cause, None))
                });
                if let Err(error) = &result
                    && error.cause().is_transport()
                {
                    fatal = Some(error.cause().clone());
                }
                slots[index] = Some(result);
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all SMB removal inputs assigned"))
            .collect())
    }

    fn vmkdir_impl(&mut self, dirs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let mut slots = (0..dirs.len()).map(|_| None).collect::<Vec<_>>();
        let mut prepared = Vec::with_capacity(dirs.len());
        for (index, attrs) in dirs.iter().enumerate() {
            let request = validate_mkdir_mode(attrs).and_then(|()| {
                let path = self.file_path(&attrs.file)?;
                let wire = self.path_string(&path)?;
                let mut create =
                    self.open_request(&wire, libc::O_CREAT | libc::O_EXCL | libc::O_DIRECTORY)?;
                // Directory creation needs no FILE_LIST_DIRECTORY access.
                create.desired_access = FileAccessMask::new(
                    FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
                );
                Ok((path, create))
            });
            match request {
                Ok((path, create)) => prepared.push((index, path, create)),
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error))),
            }
        }
        let paths: Vec<_> = prepared.iter().map(|(_, path, _)| path.clone()).collect();
        let connection = self.client.connection_mut().clone();
        let tree = self.tree.clone();
        if prepared.len() > 1 && !self.tree.is_dfs && paths_are_independent(&paths) {
            let jobs = prepared.into_iter().map(|(index, _, create)| {
                let connection = connection.clone();
                let tree = tree.clone();
                async move {
                    (
                        index,
                        open_close_on_connection(connection, tree.clone(), create).await,
                    )
                }
            });
            for (index, outcome) in self.runtime.block_on(join_all(jobs)) {
                if let Some(file_id) = outcome.deferred_close {
                    self.deferred_closes
                        .push(DeferredClose::new(self.tree.clone(), file_id));
                }
                slots[index] = Some(outcome.result.map(|_| ()).map_err(|cause| {
                    if cause.is_transport() {
                        OpError::unknown(cause, None)
                    } else {
                        OpError::failed(cause, None)
                    }
                }));
            }
        } else {
            let mut fatal: Option<VfError> = None;
            for (index, _, create) in prepared {
                if let Some(cause) = &fatal {
                    slots[index] = Some(Err(OpError::not_attempted(cause.clone())));
                    continue;
                }
                let outcome = self.runtime.block_on(open_close_on_connection(
                    connection.clone(),
                    tree.clone(),
                    create,
                ));
                if let Some(file_id) = outcome.deferred_close {
                    self.deferred_closes
                        .push(DeferredClose::new(self.tree.clone(), file_id));
                }
                slots[index] = Some(outcome.result.map(|_| ()).map_err(|cause| {
                    if cause.is_transport() {
                        fatal = Some(cause.clone());
                        OpError::unknown(cause, None)
                    } else {
                        OpError::failed(cause, None)
                    }
                }));
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("every SMB mkdir assigned"))
            .collect())
    }

    fn vsymlink_impl(
        &mut self,
        oldpaths: &[&Path],
        newpaths: &[&Path],
    ) -> vfsi_core::VecResult<()> {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        if oldpaths.is_empty() {
            Ok(Vec::new())
        } else {
            Err(VfError::unsupported(None))
        }
    }

    fn vreadlink_impl(&mut self, paths: &[&Path]) -> vfsi_core::VecResult<Vec<u8>> {
        if paths.is_empty() {
            Ok(Vec::new())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn vhardlink_impl(
        &mut self,
        oldpaths: &[&Path],
        newpaths: &[&Path],
    ) -> vfsi_core::VecResult<()> {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        if oldpaths.is_empty() {
            Ok(Vec::new())
        } else {
            Err(VfError::unsupported(None))
        }
    }

    fn vcopy_data_impl(&mut self, pairs: &[ExtentPair]) -> vfsi_core::VecResult<()> {
        let mut fatal: Option<VfError> = None;
        Ok(pairs
            .iter()
            .map(|pair| {
                if let Some(cause) = &fatal {
                    return Err(vfsi_core::OpError::not_attempted(cause.clone()));
                }
                self.copy_client_side(pair).map_err(|cause| {
                    if cause.is_transport() {
                        fatal = Some(cause.clone());
                        vfsi_core::OpError::unknown(cause, None)
                    } else {
                        vfsi_core::OpError::failed(cause, None)
                    }
                })
            })
            .collect())
    }

    fn vcopy_impl(
        &mut self,
        pairs: &[ExtentPair],
        options: CopyOptions,
    ) -> vfsi_core::VecResult<()> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        if !options.follow_source_symlinks() {
            return Err(VfError::unsupported(None));
        }
        let mut fatal: Option<VfError> = None;
        Ok(pairs
            .iter()
            .map(|pair| {
                if let Some(cause) = &fatal {
                    return Err(vfsi_core::OpError::not_attempted(cause.clone()));
                }
                let result = self.copy_extent_with_strategy(pair, self.server_copy_enabled);
                result.map_err(|cause| {
                    if cause.is_transport() {
                        fatal = Some(cause.clone());
                        vfsi_core::OpError::unknown(cause, None)
                    } else {
                        vfsi_core::OpError::failed(cause, None)
                    }
                })
            })
            .collect())
    }

    fn copy_tree_impl(
        &mut self,
        source: &Path,
        destination: &Path,
        symlinks: bool,
        use_server_side_copy: bool,
    ) -> VfRes {
        vfsi_sync::backend::helpers::copy_tree_batches_with_pages(
            self,
            source,
            destination,
            symlinks,
            AttrMask::MODE | AttrMask::SIZE,
            |backend, path, masks, cursor| {
                let (entries, next) =
                    backend.read_directory_page_with_policy(path, masks, cursor, 256, symlinks)?;
                let children = (0..entries.len()).map(|_| None).collect();
                Ok((entries, next, children))
            },
            |backend, pairs| {
                if use_server_side_copy {
                    vfsi_core::internal::collect_vec_result(
                        backend.vcopy_impl(pairs, vfsi_core::CopyOptions::new()),
                        pairs.len(),
                    )
                    .map(|_| ())
                } else {
                    vfsi_core::internal::collect_vec_result(
                        backend.vcopy_data_impl(pairs),
                        pairs.len(),
                    )
                    .map(|_| ())
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

impl SmbVecFs {
    pub(super) fn read_results(&mut self, reads: &[ReadOp]) -> vfsi_core::VecResult<ReadResult> {
        use vfsi_core::{OpError, OpResult};
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(index) = reads
            .iter()
            .position(|read| read.length > isize::MAX as usize)
        {
            return Err(VfError::errno(index, libc::EFBIG as u32));
        }
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
        if !concurrent {
            return Ok(reads.iter().map(|read| self.read_one(read)).collect());
        }
        let connection = self.client.connection_mut().clone();
        let tree = self.tree.clone();
        let mut output: Vec<Option<OpResult<ReadResult>>> =
            (0..reads.len()).map(|_| None).collect();
        let mut jobs = Vec::with_capacity(reads.len());
        let mut indices = Vec::with_capacity(reads.len());
        for (index, read) in reads.iter().enumerate() {
            let prepared = (|| -> VfResult<_> {
                let VfOffset::At(offset) = read.offset else {
                    unreachable!()
                };
                checked_offset(offset, read.length as u64, index)?;
                let path = self.path_string(&self.file_path(&read.file)?)?;
                self.open_request(&path, libc::O_RDONLY)
            })();
            match prepared {
                Ok(create) => {
                    let VfOffset::At(offset) = read.offset else {
                        unreachable!()
                    };
                    jobs.push(concurrent_compound_read(
                        connection.clone(),
                        tree.clone(),
                        create,
                        offset,
                        read.length,
                    ));
                    indices.push(index);
                }
                Err(cause) => output[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        let results = self.runtime.block_on(join_all(jobs));
        queue_deferred_closes(&results, &mut self.deferred_closes, |id| {
            DeferredClose::new(self.tree.clone(), id)
        });
        // Recovery of one input changes the live connection state. Eligibility
        // belongs to the failed wave, so keep it for every original sibling.
        let disconnected = self.client.is_disconnected();
        for (index, outcome) in indices.into_iter().zip(results) {
            let read = &reads[index];
            // Only failed inputs with no confirmed data enter the existing
            // reconnect path. Successful sibling results are never replayed.
            let result = match outcome.result {
                Err(error) if disconnected && error.progress().is_none() => self.read_one(read),
                result => result.map(|data| {
                    let VfOffset::At(offset) = read.offset else {
                        unreachable!()
                    };
                    ReadResult {
                        file: read.file.clone(),
                        offset,
                        eof: data.len() < read.length,
                        data,
                    }
                }),
            };
            output[index] = Some(result);
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every SMB read resolved"))
            .collect())
    }
}

impl SmbVecFs {
    pub(super) fn write_results(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_core::VecResult<WriteResult> {
        use vfsi_core::{OpError, OpResult};
        #[cfg(feature = "test-faults")]
        {
            self.last_writev_was_concurrent = false;
        }
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        if self.client.is_disconnected() {
            let cause = VfError::transport_with_kind(
                None,
                vfsi_core::TransportKind::Connection,
                "SMB connection is disconnected",
            );
            return Ok(writes
                .iter()
                .map(|_| Err(OpError::not_attempted(cause.clone())))
                .collect());
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
            let mut output: Vec<Option<OpResult<WriteResult>>> = vec![None; writes.len()];
            let mut prepared = Vec::with_capacity(writes.len());
            let mut indices = Vec::with_capacity(writes.len());
            for (index, write) in writes.iter().enumerate() {
                let prepare = || {
                    let VfOffset::At(offset) = write.offset() else {
                        unreachable!("concurrent eligibility checked")
                    };
                    checked_offset(offset, write.data().len() as u64, index)?;
                    let path = self.path_string(&self.file_path(write.file())?)?;
                    let create = self.open_request(&path, libc::O_WRONLY)?;
                    let mut identity_create = self.open_request(&path, libc::O_RDONLY)?;
                    identity_create.desired_access = FileAccessMask::new(
                        FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
                    );
                    Ok::<_, VfError>((create, identity_create, offset, write.data()))
                };
                match prepare() {
                    Ok(request) => {
                        indices.push(index);
                        prepared.push(request);
                    }
                    Err(cause) => output[index] = Some(Err(OpError::not_attempted(cause))),
                }
            }
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let independent = if prepared.len() < 2 {
                true
            } else {
                let jobs = prepared.iter().map(|(_, create, _, _)| {
                    concurrent_file_identity(connection.clone(), tree.clone(), create.clone())
                });
                let outcomes = self.runtime.block_on(join_all(jobs));
                queue_deferred_closes(&outcomes, &mut self.deferred_closes, |state| {
                    DeferredClose::new(self.tree.clone(), state)
                });
                let identities: VfResult<Vec<_>> =
                    outcomes.into_iter().map(|outcome| outcome.result).collect();
                identities.as_deref().is_ok_and(identities_are_independent)
            };
            if independent {
                #[cfg(feature = "test-faults")]
                {
                    self.last_writev_was_concurrent = prepared.len() > 1;
                }
                let jobs = prepared.into_iter().map(|(create, _, offset, data)| {
                    concurrent_compound_write(
                        connection.clone(),
                        tree.clone(),
                        create,
                        offset,
                        data.to_vec(),
                    )
                });
                let outcomes = self.runtime.block_on(join_all(jobs));
                queue_deferred_closes(&outcomes, &mut self.deferred_closes, |state| {
                    DeferredClose::new(self.tree.clone(), state)
                });
                for (index, outcome) in indices.into_iter().zip(outcomes) {
                    let write = &writes[index];
                    let VfOffset::At(offset) = write.offset() else {
                        unreachable!()
                    };
                    output[index] = Some(outcome.result.map(|bytes_written| WriteResult {
                        file: write.file().clone(),
                        offset,
                        bytes_written,
                        stable: true,
                    }));
                }
                return Ok(output
                    .into_iter()
                    .map(|slot| slot.expect("every WRITE assigned"))
                    .collect());
            }
            // No data WRITE has been dispatched by identity probes. Aliases or
            // unknown identities retain syscall/wire order, and only unresolved
            // valid inputs need their actual write attempted.
            let mut stopped = None;
            for (index, write) in writes.iter().enumerate() {
                if output[index].is_none() {
                    output[index] = Some(self.write_unless_disconnected(*write, &mut stopped));
                }
            }
            return Ok(output
                .into_iter()
                .map(|slot| slot.expect("every WRITE assigned"))
                .collect());
        }
        let mut stopped = None;
        Ok(writes
            .iter()
            .map(|write| self.write_unless_disconnected(*write, &mut stopped))
            .collect())
    }

    fn write_unless_disconnected(
        &mut self,
        write: WriteOp<&FileOperand, &[u8]>,
        stopped: &mut Option<VfError>,
    ) -> vfsi_core::OpResult<WriteResult> {
        use vfsi_core::OpError;
        if stopped.is_none() && self.client.is_disconnected() {
            *stopped = Some(VfError::transport_with_kind(
                None,
                vfsi_core::TransportKind::Connection,
                "SMB connection is disconnected",
            ));
        }
        if let Some(cause) = stopped {
            return Err(OpError::not_attempted(cause.clone()));
        }
        let result = self.write_one(write);
        if self.client.is_disconnected()
            && let Err(error) = &result
        {
            *stopped = Some(error.cause().clone());
        }
        result
    }
}
