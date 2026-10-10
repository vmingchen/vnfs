//! VectorBackend dispatch and native vector execution.

use super::*;

impl VectorBackend for NfsVecFs {
    fn vopen_outcomes_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        if self.read_only
            && let Some(index) = flags.iter().position(|flags| open_flags_mutate(*flags))
        {
            return Err(VfError::client(index, libc::EROFS as u32));
        }
        self.drain_deferred_descriptor_closes()?;
        if paths.is_empty() {
            return Ok(ManyResults::all_success(Vec::new()));
        }
        self.openv_merged(paths, flags, modes)
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &VfFile) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCleanup { index: _index })?;
        Ok(())
    }

    fn vclose_impl(&mut self, files: &[VfFile]) -> VfRes {
        let mut ops = Vec::with_capacity(files.len());
        let mut fds = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            if !f.is_descriptor() {
                return Err(VfError::failure(i, nfsstat4_NFS4ERR_INVAL));
            }
            let fd = f.fd().unwrap();
            let open = self
                .open_files
                .get(&fd)
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            fds.push(fd);
            ops.push(crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            });
        }
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCloseDispatch { index: 0 })?;
        #[cfg(feature = "test-faults")]
        for index in 0..ops.len() {
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeCloseItem { index }) {
                if index > 0 {
                    self.nfs
                        .close_many(&ops[..index])
                        .map_err(vfsi_core::error_from_rpc_indexed)?;
                    for fd in fds.iter().take(index) {
                        self.open_files.remove(fd);
                    }
                }
                return Err(error.with_index(index));
            }
        }
        match self.nfs.close_many(&ops) {
            Ok(()) => {
                for fd in fds {
                    self.open_files.remove(&fd);
                }
                Ok(())
            }
            Err(error) => {
                if !error.is_transport() {
                    for fd in fds.iter().take(error.op_index) {
                        self.open_files.remove(fd);
                    }
                }
                Err(vfsi_core::error_from_rpc_indexed(error))
            }
        }
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.vread_impl(reads));
        }
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        if reads.iter().all(|r| r.file.is_descriptor()) {
            return self.vread_batch_nfs(reads);
        }
        // Path-based (and mixed descriptor/path) batches: resolve offsets and
        // try the merged compound.
        let mut path_ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        for (i, r) in reads.iter().enumerate() {
            let off = match r.offset {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let path = self.server_vf_path(&r.file).map_err(|e| e.with_index(i))?;
                    let fh = self.resolve_follow(&path).map_err(|e| e.with_index(i))?;
                    self.file_size(&fh).map_err(|e| e.with_index(i))?
                }
                VfOffset::Cur => match r.file.fd() {
                    Some(fd) => self
                        .open_files
                        .get(&fd)
                        .map(|o| o.cur_offset)
                        .ok_or_else(|| VfError::failure(i, ERR_EBADF))?,
                    None => return Err(VfError::failure(i, ERR_INVAL)),
                },
                _ => return Err(VfError::failure(i, ERR_INVAL)),
            };
            let length =
                u64::try_from(r.length).map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            offsets.push(off);
            let file = if r.file.is_descriptor() {
                let fd = r.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(&r.file).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            path_ops.push(crate::client::PathReadOp {
                file,
                offset: off,
                count: r.length,
                stateid: r
                    .file
                    .fd()
                    .and_then(|fd| self.open_files.get(&fd).map(|o| o.stateid)),
            });
        }
        let out = match self.merged_mode {
            MergedIoMode::Off => self.vread_path_fallback_nfs(reads),
            MergedIoMode::OpenWrite => self.vread_path_openwrite_nfs(reads, &path_ops, &offsets),
            MergedIoMode::Full => self.vread_path_full_nfs(reads, &path_ops, &offsets),
        }?;
        // Advance descriptor cursors for Cur-offset reads.
        for (i, r) in reads.iter().enumerate() {
            if r.offset == VfOffset::Cur && r.file.is_descriptor() {
                let new = out[i]
                    .offset
                    .checked_add(out[i].data.len() as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                self.advance_offset(&r.file, new);
            }
        }
        Ok(out)
    }

    fn vread_into_impl(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        if reads.len() != buffers.len() {
            return Err(VfError::client(0, ERR_INVAL));
        }
        for (index, (read, buffer)) in reads.iter().zip(buffers.iter()).enumerate() {
            if read.length != buffer.len() {
                return Err(VfError::client(index, ERR_INVAL));
            }
        }
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.vread_into_impl(reads, buffers));
        }
        if reads.iter().all(|read| read.file.is_descriptor()) {
            return self.vread_batch_into_nfs(reads, buffers);
        }
        // Path-based reads still use the merged path planner; its owned
        // results are copied until that decoder also supports caller storage.
        let results = self.vread_impl(reads)?;
        if results.len() != reads.len() {
            return Err(VfError::transport(
                None,
                "readv_into returned wrong result count",
            ));
        }
        let mut output = Vec::with_capacity(reads.len());
        for (index, ((read, result), buffer)) in reads
            .iter()
            .zip(results)
            .zip(buffers.iter_mut())
            .enumerate()
        {
            if result.file != read.file
                || result.data.len() > buffer.len()
                || matches!(read.offset, VfOffset::At(offset) if result.offset != offset)
            {
                return Err(VfError::transport(
                    Some(index),
                    "malformed readv_into result",
                ));
            }
            buffer[..result.data.len()].copy_from_slice(&result.data);
            output.push(ReadIntoResult {
                file: result.file,
                offset: result.offset,
                read: result.data.len(),
                eof: result.eof,
            });
        }
        Ok(output)
    }

    fn vwrite_impl(&mut self, writes: &[WriteOp<&VfFile, &[u8]>]) -> VfResult<Vec<WriteResult>> {
        self.ensure_writable(writes.len())?;
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        if writes.iter().all(|w| w.file().is_descriptor()) {
            return self.vwrite_batch_nfs(writes);
        }
        let mut path_ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        for (i, w) in writes.iter().enumerate() {
            let off = match w.offset() {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let path = self.server_vf_path(w.file()).map_err(|e| e.with_index(i))?;
                    let fh = self.resolve_follow(&path).map_err(|e| e.with_index(i))?;
                    self.file_size(&fh).map_err(|e| e.with_index(i))?
                }
                VfOffset::Cur => match w.file().fd() {
                    Some(fd) => self
                        .open_files
                        .get(&fd)
                        .map(|o| o.cur_offset)
                        .ok_or_else(|| VfError::failure(i, ERR_EBADF))?,
                    None => return Err(VfError::failure(i, ERR_INVAL)),
                },
                _ => return Err(VfError::failure(i, ERR_INVAL)),
            };
            let length = u64::try_from(w.data().len())
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            offsets.push(off);
            let file = if w.file().is_descriptor() {
                let fd = w.file().fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(
                        &self.server_vf_path(w.file()).map_err(|e| e.with_index(i))?,
                    )
                    .to_vec(),
                )
            };
            path_ops.push(crate::client::PathWriteOp {
                file,
                offset: off,
                data: w.data(),
                create: w.creates() && !w.file().is_descriptor(),
                truncate: w.truncates() && !w.file().is_descriptor(),
                stateid: w
                    .file()
                    .fd()
                    .and_then(|fd| self.open_files.get(&fd).map(|o| o.stateid)),
            });
        }
        let out = match self.merged_mode {
            MergedIoMode::Off => self.vwrite_path_fallback_nfs(writes),
            MergedIoMode::OpenWrite => self.vwrite_path_openwrite_nfs(writes, &path_ops, &offsets),
            MergedIoMode::Full => self.vwrite_path_full_nfs(writes, &path_ops, &offsets),
        }?;
        // Advance descriptor cursors for Cur-offset writes.
        for (i, w) in writes.iter().enumerate() {
            if w.offset() == VfOffset::Cur && w.file().is_descriptor() {
                let new = out[i]
                    .offset
                    .checked_add(out[i].written as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                self.advance_offset(w.file(), new);
            }
        }
        Ok(out)
    }

    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.vgetattrs_impl(attrs));
        }
        self.getattrsv_impl(attrs, true)
    }

    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.vgetattrs_nofollow_impl(attrs));
        }
        self.getattrsv_impl(attrs, false)
    }

    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.ensure_writable(attrs.len())?;
        self.vsetattrs_nfs(attrs, true)
    }

    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.ensure_writable(attrs.len())?;
        self.vsetattrs_nfs(attrs, false)
    }

    fn listdir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| {
                client.listdir_impl(dir, masks, max_count, recursive)
            });
        }
        let mut out = Vec::new();
        self.listdir_rec(dir, masks, max_count, recursive, &mut out)?;
        Ok(out)
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
        if page_size == 0 {
            return Err(VfError::client(0, ERR_INVAL));
        }
        // Only a failed first page may be retried: after a page has reached
        // the application, a reconnect could invalidate the READDIR cookie.
        if !self.recovery_in_progress && cursor.is_none() {
            return self.read_with_recovery(|client| {
                client.listdir_page_impl(dir, masks, None, page_size, _max_entries, follow_symlinks)
            });
        }
        let mut state = match cursor {
            Some(cursor) => cursor.into_state::<NfsDirectoryCursor>()?,
            None => NfsDirectoryCursor {
                fh: self.resolve_directory_path(dir, follow_symlinks)?,
                cookie: 0,
                buffered: VecDeque::new(),
            },
        };
        let ids = request_mask_to_attr_list(&masks);
        if state.buffered.is_empty() {
            let page = self
                .nfs
                .readdir(&state.fh, state.cookie, &ids)
                .map_err(|error| vfsi_core::error_from_rpc(error, 0))?;
            if page.is_empty() {
                return Ok((Vec::new(), None));
            }
            let next_cookie = page.last().map(|entry| entry.cookie).unwrap_or(0);
            if next_cookie != 0 && next_cookie == state.cookie {
                return Err(VfError::transport(None, "READDIR cookie made no progress"));
            }
            state.cookie = next_cookie;
            state.buffered = page.into();
        }
        let mut output = Vec::with_capacity(state.buffered.len().min(page_size));
        while output.len() < page_size {
            let Some(entry) = state.buffered.pop_front() else {
                break;
            };
            let path = dir.join(path_from_bytes(&entry.name));
            let mut attrs = VfAttrs {
                file: VfFile::from_os_path(&path),
                masks,
                ..VfAttrs::default()
            };
            let values = parse_attr_list(&ids, &entry.attrs)?;
            apply_attrs(&mut attrs, &values);
            output.push(attrs);
        }
        let next = if !state.buffered.is_empty() || state.cookie != 0 {
            Some(DirPageCursor::new(state))
        } else {
            None
        };
        Ok((output, next))
    }

    fn directory_page_batch_size(&self) -> usize {
        32
    }

    fn vlistdir_pages_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        cursors: Vec<Option<DirPageCursor>>,
        page_size: usize,
        _max_entries: usize,
        follow_symlinks: bool,
    ) -> VfResult<Vec<vfsi_sync::BackendDirectoryPage>> {
        if dirs.len() != cursors.len() || page_size == 0 {
            return Err(VfError::client(0, ERR_INVAL));
        }
        if dirs.is_empty() {
            return Ok(Vec::new());
        }
        if !self.recovery_in_progress && cursors.iter().all(Option::is_none) {
            return self.read_with_recovery(|client| {
                client.vlistdir_pages_impl(
                    dirs,
                    masks,
                    (0..dirs.len()).map(|_| None).collect(),
                    page_size,
                    _max_entries,
                    follow_symlinks,
                )
            });
        }
        let fresh: Vec<usize> = cursors
            .iter()
            .enumerate()
            .filter_map(|(i, cursor)| cursor.is_none().then_some(i))
            .collect();
        let mut handles = std::collections::HashMap::new();
        if !follow_symlinks {
            for &index in &fresh {
                let fh = self
                    .resolve_directory_path(dirs[index], false)
                    .map_err(|error| error.with_index(index))?;
                handles.insert(index, fh);
            }
        } else if let [index] = fresh.as_slice() {
            // Preserve the scalar deep-resolution fast path. READDIR itself
            // validates that the resolved target is a directory.
            let fh = self
                .resolve_directory_path(dirs[*index], true)
                .map_err(|error| error.with_index(*index))?;
            handles.insert(*index, fh);
        } else if !fresh.is_empty() {
            let files: Vec<_> = fresh
                .iter()
                .map(|&i| VfFile::from_os_path(dirs[i]))
                .collect();
            let refs: Vec<_> = files.iter().collect();
            let resolved = self
                .resolve_files_nfs(&refs, true)
                .map_err(|error| remap_active_error(error, &fresh))?;
            for (&index, resolved) in fresh.iter().zip(resolved) {
                let (fh, kind) = resolved.map_err(|status| VfError::nfs(index, status))?;
                if kind != nfs_ftype4_NF4DIR {
                    return Err(VfError::nfs(index, nfsstat4_NFS4ERR_NOTDIR));
                }
                handles.insert(index, fh);
            }
        }
        let ids = request_mask_to_attr_list(&masks);
        let mut states = Vec::with_capacity(dirs.len());
        let mut children = Vec::new();
        let mut child_indices = Vec::new();
        for (index, cursor) in cursors.into_iter().enumerate() {
            states.push(match cursor {
                Some(cursor) if cursor.is::<NfsChildDirectoryCursor>() => {
                    let child = cursor.into_state::<NfsChildDirectoryCursor>()?;
                    child_indices.push(index);
                    children.push((child.parent.clone(), child.name));
                    // Filled by the anchored LOOKUP + READDIR wave below.
                    NfsDirectoryCursor {
                        fh: child.parent,
                        cookie: 0,
                        buffered: VecDeque::new(),
                    }
                }
                Some(cursor) => cursor
                    .into_state::<NfsDirectoryCursor>()
                    .map_err(|error| error.with_index(index))?,
                None => NfsDirectoryCursor {
                    fh: handles.remove(&index).expect("resolved fresh directory"),
                    cookie: 0,
                    buffered: VecDeque::new(),
                },
            });
        }
        let child_pages = self
            .nfs
            .readdir_children_bounded(&children, &ids, 32 * 1024)
            .map_err(|error| {
                remap_active_error(vfsi_core::error_from_rpc_indexed(error), &child_indices)
            })?;
        if child_pages.len() != child_indices.len() {
            return Err(VfError::transport(
                None,
                "invalid child directory page count",
            ));
        }
        for (&index, page) in child_indices.iter().zip(child_pages) {
            states[index] = NfsDirectoryCursor {
                fh: page.fh,
                cookie: page.cookie,
                buffered: page.entries.into(),
            };
        }
        let active: Vec<_> = states
            .iter()
            .enumerate()
            .filter_map(|(i, state)| {
                (state.buffered.is_empty() && !child_indices.contains(&i)).then_some(i)
            })
            .collect();
        let operations: Vec<_> = active
            .iter()
            .map(|&i| (states[i].fh.clone(), states[i].cookie))
            .collect();
        let pages = self
            .nfs
            .readdir_pages_bounded(&operations, &ids, 32 * 1024)
            .map_err(|error| {
                remap_active_error(vfsi_core::error_from_rpc_indexed(error), &active)
            })?;
        if pages.len() != active.len() {
            return Err(VfError::transport(None, "invalid READDIR page count"));
        }
        for (&index, (entries, cookie)) in active.iter().zip(pages) {
            let state = &mut states[index];
            if cookie != 0 && cookie == state.cookie {
                return Err(VfError::transport(
                    Some(index),
                    "READDIR cookie made no progress",
                ));
            }
            state.cookie = cookie;
            state.buffered = entries.into();
        }
        states
            .into_iter()
            .enumerate()
            .map(|(index, mut state)| {
                let mut output = Vec::new();
                let mut seeds = Vec::new();
                for entry in state.buffered.drain(..state.buffered.len().min(page_size)) {
                    let mut attrs = VfAttrs {
                        file: VfFile::from_os_path(&dirs[index].join(path_from_bytes(&entry.name))),
                        masks,
                        ..VfAttrs::default()
                    };
                    let values = parse_attr_list(&ids, &entry.attrs)
                        .map_err(|error| error.with_index(index))?;
                    apply_attrs(&mut attrs, &values);
                    seeds.push((attrs.ftype == VfType::Directory).then(|| {
                        DirPageCursor::new(NfsChildDirectoryCursor {
                            parent: state.fh.clone(),
                            name: entry.name,
                        })
                    }));
                    output.push(attrs);
                }
                let next = (!state.buffered.is_empty() || state.cookie != 0)
                    .then(|| DirPageCursor::new(state));
                Ok((output, next, seeds))
            })
            .collect()
    }

    fn visit_dir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_entries: usize,
        cb: &mut dyn FnMut(&VfAttrs) -> bool,
    ) -> VfRes {
        if !self.recovery_in_progress {
            // Retry a failed read only before the first callback: replaying
            // after an entry was delivered would duplicate application work.
            let mut delivered = false;
            self.recovery_in_progress = true;
            let first = self.visit_dir_impl(dir, masks, max_entries, &mut |attrs| {
                delivered = true;
                cb(attrs)
            });
            self.recovery_in_progress = false;
            return match first {
                Err(error) if !delivered && self.auto_reconnect && Self::needs_recovery(&error) => {
                    self.reconnect()?;
                    self.recovery_in_progress = true;
                    let retry = self.visit_dir_impl(dir, masks, max_entries, cb);
                    self.recovery_in_progress = false;
                    retry
                }
                result => result,
            };
        }
        // Stream each READDIR page before requesting the next one.
        let fh = self.resolve_path(&self.server_path(dir), true)?;
        let ids = request_mask_to_attr_list(&masks);
        let mut cookie = 0u64;
        let mut count = 0usize;
        loop {
            let page = self
                .nfs
                .readdir(&fh, cookie, &ids)
                .map_err(|error| vfsi_core::error_from_rpc(error, 0))?;
            for entry in &page {
                if max_entries != 0 && count >= max_entries {
                    return Ok(());
                }
                let path = dir.join(path_from_bytes(&entry.name));
                let mut attrs = VfAttrs {
                    file: VfFile::from_os_path(&path),
                    masks,
                    ..VfAttrs::default()
                };
                let values = parse_attr_list(&ids, &entry.attrs)?;
                apply_attrs(&mut attrs, &values);
                if !cb(&attrs) {
                    return Ok(());
                }
                count += 1;
            }
            let next = page.last().map(|entry| entry.cookie).unwrap_or(0);
            if next == 0 {
                return Ok(());
            }
            if next == cookie {
                return Err(VfError::transport(None, "READDIR cookie made no progress"));
            }
            cookie = next;
        }
    }

    /// List many directories in a few compounds: the directories are
    /// resolved in one batched lookup, then their first READDIR pages (and
    /// any continuation pages) are drained in batched compounds within the
    /// session's negotiated operation, request, and response limits.
    fn vlistdirs_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut dyn FnMut(&VfAttrs, &Path) -> bool,
    ) -> VfRes {
        if dirs.is_empty() {
            return Ok(());
        }
        let ids = request_mask_to_attr_list(&masks);
        let mut counted = 0usize;
        let mut level_paths: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
        let mut level_owners: Vec<usize> = (0..dirs.len()).collect();
        loop {
            if level_paths.is_empty() {
                return Ok(());
            }
            // Batch-resolve this level's directories.
            let files: Vec<VfFile> = level_paths
                .iter()
                .map(|p| VfFile::from_os_path(p))
                .collect();
            let refs: Vec<&VfFile> = files.iter().collect();
            let resolved = self.resolve_files_nfs(&refs, true)?;
            let mut level: Vec<(WireFileHandle, PathBuf, usize)> =
                Vec::with_capacity(level_paths.len());
            for (i, r) in resolved.iter().enumerate() {
                let owner = level_owners[i];
                match r {
                    Ok((fh, ftype)) if *ftype == nfs_ftype4_NF4DIR => {
                        level.push((fh.clone(), level_paths[i].clone(), owner));
                    }
                    Ok((_, _)) => {
                        return Err(VfError::failure(owner, nfsstat4_NFS4ERR_NOTDIR));
                    }
                    Err(status) => return Err(VfError::nfs(owner, *status)),
                }
            }
            // First pages for all directories in one compound.
            let ops: Vec<(WireFileHandle, u64)> =
                level.iter().map(|(fh, _, _)| (fh.clone(), 0)).collect();
            let results = self.nfs.readdir_pages(&ops, &ids).map_err(|error| {
                remap_active_error(vfsi_core::error_from_rpc_indexed(error), &level_owners)
            })?;
            if results.len() != level.len() {
                return Err(VfError::transport(
                    None,
                    "READDIR returned the wrong number of directory pages",
                ));
            }
            // Emit each bounded READDIR page before fetching a continuation.
            // Retaining every page until EOF used memory proportional to the
            // entire listing even when the caller requested an early stop.
            let mut next_level: Vec<PathBuf> = Vec::new();
            let mut next_owners: Vec<usize> = Vec::new();
            let mut emit_page =
                |idx: usize, entries: Vec<crate::client::DirEntry>| -> VfResult<bool> {
                    let dir = &level[idx].1;
                    let owner = level[idx].2;
                    for e in entries {
                        if max_entries != 0 && counted >= max_entries {
                            return Ok(false);
                        }
                        let path = dir.join(path_from_bytes(&e.name));
                        let mut a = VfAttrs {
                            file: VfFile::from_os_path(&path),
                            masks,
                            ..VfAttrs::default()
                        };
                        let vals = parse_attr_list(&ids, &e.attrs)
                            .map_err(|error| error.with_index(owner))?;
                        apply_attrs(&mut a, &vals);
                        if recursive && a.ftype == VfType::Directory {
                            next_level.push(path);
                            next_owners.push(owner);
                        }
                        if !cb(&a, dir) {
                            return Ok(false);
                        }
                        counted += 1;
                    }
                    Ok(true)
                };
            // Each page is emitted before another wave is fetched. Entries
            // for different directories may interleave by page; callers
            // must discard partial listings after an error.
            let mut pending = Vec::new();
            for (idx, (entries, cookie)) in results.into_iter().enumerate() {
                if !emit_page(idx, entries)? {
                    return Ok(());
                }
                if cookie != 0 {
                    pending.push((idx, level[idx].0.clone(), cookie));
                }
            }
            while !pending.is_empty() {
                let cont_ops: Vec<(WireFileHandle, u64)> = pending
                    .iter()
                    .map(|(_, fh, cookie)| (fh.clone(), *cookie))
                    .collect();
                let pending_owners: Vec<usize> =
                    pending.iter().map(|(idx, _, _)| level[*idx].2).collect();
                let cont = self.nfs.readdir_pages(&cont_ops, &ids).map_err(|error| {
                    remap_active_error(vfsi_core::error_from_rpc_indexed(error), &pending_owners)
                })?;
                if cont.len() != pending.len() {
                    return Err(VfError::transport(
                        None,
                        "READDIR continuation returned the wrong number of pages",
                    ));
                }
                let mut next_pending = Vec::new();
                for ((idx, fh, previous_cookie), (entries, cookie)) in pending.into_iter().zip(cont)
                {
                    if !emit_page(idx, entries)? {
                        return Ok(());
                    }
                    if cookie == previous_cookie {
                        return Err(VfError::transport(
                            Some(level[idx].2),
                            "READDIR continuation cookie made no progress",
                        ));
                    }
                    if cookie != 0 {
                        next_pending.push((idx, fh, cookie));
                    }
                }
                pending = next_pending;
            }
            if !recursive {
                return Ok(());
            }
            level_paths = next_level;
            level_owners = next_owners;
        }
    }

    fn walk_with_options_impl(
        &mut self,
        root: &Path,
        masks: AttrMask,
        options: ListDirOptions,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        let root_fh = self.resolve_path(&self.server_path(root), true)?;
        let ids = request_mask_to_attr_list(&masks);
        let mut collected: std::collections::HashMap<PathBuf, Vec<VfAttrs>> =
            std::collections::HashMap::new();
        let mut entry_count = 0usize;
        let mut stored_path_bytes = 0usize;

        // Resolve the root once, then decode each bounded READDIR response
        // directly into the caller-visible accumulator. Raw continuation
        // pages are never retained after they are decoded.
        let mut root_attrs = Vec::new();
        let mut cookie = 0u64;
        loop {
            let page = self
                .nfs
                .readdir(&root_fh, cookie, &ids)
                .map_err(|error| vfsi_core::error_from_rpc(error, 0))?;
            append_bounded_walk_page(
                root,
                root,
                masks,
                &ids,
                &page,
                options,
                &mut entry_count,
                &mut stored_path_bytes,
                &mut root_attrs,
            )?;
            cookie = page.last().map(|entry| entry.cookie).unwrap_or(0);
            if cookie == 0 {
                break;
            }
        }
        sort(root, &mut root_attrs);

        // Preserve the parent filehandle so resolving and reading each child
        // directory can share one compound instead of re-walking full paths.
        let mut frontier: Vec<(WireFileHandle, PathBuf)> = root_attrs
            .iter()
            .filter(|entry| entry.ftype == VfType::Directory)
            .filter_map(|entry| {
                entry
                    .file
                    .path()
                    .map(|path| (root_fh.clone(), path.to_path_buf()))
            })
            .collect();
        collected.insert(root.to_path_buf(), root_attrs);

        while !frontier.is_empty() {
            let operations: Vec<(WireFileHandle, Vec<u8>)> = frontier
                .iter()
                .map(|(parent, path)| {
                    (
                        parent.clone(),
                        path.file_name()
                            .map(|name| path_bytes(Path::new(name)).to_vec())
                            .unwrap_or_default(),
                    )
                })
                .collect();
            let results = self
                .nfs
                .readdir_children(&operations, &ids)
                .map_err(vfsi_core::error_from_rpc_indexed)?;
            let mut level_attrs: Vec<Vec<VfAttrs>> =
                (0..results.len()).map(|_| Vec::new()).collect();
            let mut pending = Vec::new();
            for (index, result) in results.iter().enumerate() {
                append_bounded_walk_page(
                    root,
                    &frontier[index].1,
                    masks,
                    &ids,
                    &result.entries,
                    options,
                    &mut entry_count,
                    &mut stored_path_bytes,
                    &mut level_attrs[index],
                )?;
                if result.cookie != 0 {
                    pending.push((index, result.fh.clone(), result.cookie));
                }
            }

            while !pending.is_empty() {
                let operations: Vec<(WireFileHandle, u64)> = pending
                    .iter()
                    .map(|(_, handle, cookie)| (handle.clone(), *cookie))
                    .collect();
                let pages = self
                    .nfs
                    .readdir_pages(&operations, &ids)
                    .map_err(vfsi_core::error_from_rpc_indexed)?;
                let mut next_pending = Vec::new();
                for ((index, handle, _), (page, cookie)) in pending.iter().zip(pages) {
                    append_bounded_walk_page(
                        root,
                        &frontier[*index].1,
                        masks,
                        &ids,
                        &page,
                        options,
                        &mut entry_count,
                        &mut stored_path_bytes,
                        &mut level_attrs[*index],
                    )?;
                    if cookie != 0 {
                        next_pending.push((*index, handle.clone(), cookie));
                    }
                }
                pending = next_pending;
            }

            let mut next_frontier = Vec::new();
            for (index, mut entries) in level_attrs.into_iter().enumerate() {
                let directory = frontier[index].1.clone();
                sort(&directory, &mut entries);
                for entry in &entries {
                    if entry.ftype == VfType::Directory
                        && let Some(path) = entry.file.path()
                        && path
                            .strip_prefix(root)
                            .map(|relative| relative.components().count())
                            .unwrap_or(usize::MAX)
                            <= options.depth_limit()
                    {
                        next_frontier.push((results[index].fh.clone(), path.to_path_buf()));
                    }
                }
                collected.insert(directory, entries);
            }
            frontier = next_frontier;
        }

        let mut out = Vec::with_capacity(collected.len());
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = collected.remove(&dir).unwrap_or_default();
            let subs: Vec<PathBuf> = entries
                .iter()
                .filter(|e| e.ftype == VfType::Directory)
                .filter_map(|e| e.file.path().map(Path::to_path_buf))
                .collect();
            for s in subs.into_iter().rev() {
                stack.push(s);
            }
            out.push(WalkEntry { path: dir, entries });
        }
        Ok(out)
    }

    fn vrename_impl(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        self.ensure_writable(pairs.len())?;
        if pairs.is_empty() {
            return Ok(());
        }
        if pairs
            .iter()
            .any(|(s, d)| s.is_descriptor() || d.is_descriptor())
        {
            return self.renamev_phased(pairs);
        }
        let mut prs = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let s = self.server_vf_path(src).map_err(|e| e.with_index(i))?;
            let d = self.server_vf_path(dst).map_err(|e| e.with_index(i))?;
            prs.push(crate::client::PathRenamePair {
                src: path_bytes(&s).to_vec(),
                dst: path_bytes(&d).to_vec(),
            });
        }
        let outcome = self
            .nfs
            .renamev_path_compound(&prs)
            .map_err(|e| vfsi_core::error_from_rpc(e, None))?;
        match outcome.failed {
            Some((i, _st)) => {
                // Prefix [0..i) renamed; retry [i..] via the phased path,
                // re-attributing its error to the original index.
                let suffix = &pairs[i..];
                self.renamev_phased(suffix)
                    .map_err(|e| e.map_index(|rel| i + rel))
            }
            None => Ok(()),
        }
    }

    fn vremove_impl(&mut self, files: &[VfFile]) -> VfRes {
        self.ensure_writable(files.len())?;
        if files.is_empty() {
            return Ok(());
        }
        if files.iter().any(|f| f.is_descriptor()) {
            return self.removev_phased(files);
        }
        let mut paths = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            paths.push(path_bytes(&self.server_vf_path(f).map_err(|e| e.with_index(i))?).to_vec());
        }
        let outcome = self
            .nfs
            .removev_path_compound(&paths)
            .map_err(|e| vfsi_core::error_from_rpc(e, None))?;
        match outcome.failed {
            Some((i, _st)) => {
                // Prefix [0..i) removed; retry [i..] via the phased path.
                let suffix = &files[i..];
                self.removev_phased(suffix)
                    .map_err(|e| e.map_index(|rel| i + rel))
            }
            None => Ok(()),
        }
    }

    fn vmkdir_impl(&mut self, dirs: &[VfAttrs]) -> VfRes {
        self.ensure_writable(dirs.len())?;
        if dirs.is_empty() {
            return Ok(());
        }
        // Batch-resolve the parents, then CREATE in one compound.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            let path = self.server_vf_path(&a.file).map_err(|e| e.with_index(i))?;
            let (dir, name) =
                split_path_bytes(path_bytes(&path)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(self.visible_path(&path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_files_nfs(&refs, true)?;
        let mut creates = Vec::with_capacity(dirs.len());
        for (i, (r, name)) in resolved.iter().zip(&names).enumerate() {
            match r {
                Ok((fh, ftype)) if *ftype == nfs_ftype4_NF4DIR => {
                    creates.push(crate::client::CreateOp {
                        dir: fh.clone(),
                        name: name.clone(),
                        ftype: nfs_ftype4_NF4DIR,
                        linkdata: None,
                    });
                }
                Ok((_, _)) => return Err(VfError::failure(i, nfsstat4_NFS4ERR_NOTDIR)),
                Err(status) => return Err(VfError::nfs(i, *status)),
            }
        }
        if let Err(e) = self.nfs.create_many(&creates) {
            let i = e.op_index;
            // The prefix [0..i) was created; apply its modes before
            // reporting the failure (modes cannot ride in the CREATE).
            if i > 0 {
                let _ = self.apply_dir_modes(&dirs[..i]);
            }
            return Err(vfsi_core::error_from_rpc_indexed(e));
        }
        self.apply_dir_modes(dirs)
    }

    fn vsymlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        self.ensure_writable(newpaths.len())?;
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        // Batch-resolve the destination parents, then CREATE in one compound.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(newpaths.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(newpaths.len());
        for (i, new) in newpaths.iter().enumerate() {
            let full = self.server_path(new);
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(self.visible_path(&path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_files_nfs(&refs, true)?;
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, ((r, name), old)) in resolved.iter().zip(&names).zip(oldpaths).enumerate() {
            match r {
                Ok((fh, ftype)) if *ftype == nfs_ftype4_NF4DIR => {
                    ops.push(crate::client::CreateOp {
                        dir: fh.clone(),
                        name: name.clone(),
                        ftype: nfs_ftype4_NF4LNK,
                        linkdata: Some(path_bytes(old).to_vec()),
                    })
                }
                Ok((_, _)) => return Err(VfError::failure(i, nfsstat4_NFS4ERR_NOTDIR)),
                Err(status) => return Err(VfError::nfs(i, *status)),
            }
        }
        self.nfs
            .create_many(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)
    }

    fn vreadlink_impl(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.vreadlink_impl(paths));
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        // Batch-resolve the links themselves (no final-component follow),
        // then READLINK in one compound.
        let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_files_nfs(&refs, false)?;
        let mut ops = Vec::with_capacity(paths.len());
        for (i, r) in resolved.iter().enumerate() {
            match r {
                Ok((fh, _)) => ops.push(crate::client::ReadlinkOp { fh: fh.clone() }),
                Err(status) => return Err(VfError::nfs(i, *status)),
            }
        }
        self.nfs
            .readlink_many(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)
    }

    fn vhardlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        self.ensure_writable(newpaths.len())?;
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        if oldpaths.is_empty() {
            return Ok(());
        }
        // Batch-resolve the sources (no follow) and destination parents
        // (follow), then LINK in one compound.
        let src_files: Vec<VfFile> = oldpaths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let src_refs: Vec<&VfFile> = src_files.iter().collect();
        let src_resolved = self.resolve_files_nfs(&src_refs, false)?;
        let mut parents: Vec<PathBuf> = Vec::with_capacity(newpaths.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(newpaths.len());
        for (i, new) in newpaths.iter().enumerate() {
            let full = self.server_path(new);
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(self.visible_path(&path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let dst_resolved = self.resolve_files_nfs(&refs, true)?;
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, (((sr, dr), name), _old)) in src_resolved
            .iter()
            .zip(&dst_resolved)
            .zip(&names)
            .zip(oldpaths)
            .enumerate()
        {
            match (sr, dr) {
                (Ok((src, _)), Ok((dstdir, ftype))) if *ftype == nfs_ftype4_NF4DIR => {
                    ops.push(crate::client::LinkOp {
                        dstdir: dstdir.clone(),
                        src: src.clone(),
                        newname: name.clone(),
                    });
                }
                (Ok((_, _)), Ok((_, _))) => {
                    return Err(VfError::failure(i, nfsstat4_NFS4ERR_NOTDIR));
                }
                (Err(status), _) => return Err(VfError::nfs(i, *status)),
                (_, Err(status)) => return Err(VfError::nfs(i, *status)),
            }
        }
        self.nfs
            .link_many(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)
    }

    fn vcopy_data_impl(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.ensure_writable(pairs.len())?;
        for (i, p) in pairs.iter().enumerate() {
            // Follow final-component symlinks for both ends, matching the
            // `std::fs` backend (OPEN cannot target a symlink directly).
            let src = self
                .follow_target_path(&self.server_path(&p.src_path))
                .map_err(|e| e.with_index(i))?;
            let dst = self
                .follow_target_path(&self.server_path(&p.dst_path))
                .map_err(|e| e.with_index(i))?;
            self.copy_extent(&src, &dst, p)
                .map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn vcopy_impl(&mut self, pairs: &[ExtentPair], options: CopyOption) -> VfRes {
        self.ensure_writable(pairs.len())?;
        if pairs.is_empty() {
            return Ok(());
        }
        if options.follows_source_symlinks() {
            return self.vcopy_data_nfs(pairs);
        }
        let mut attrs: Vec<_> = pairs
            .iter()
            .map(|p| {
                VfAttrs {
                    file: VfFile::from_os_path(&p.src_path),
                    masks: AttrMask::empty(), // type is always requested
                    ..VfAttrs::default()
                }
            })
            .collect();
        self.vgetattrs_nofollow_impl(&mut attrs)?;
        let mut start = 0;
        while start < pairs.len() {
            let symlink = attrs[start].ftype == VfType::Symlink;
            let mut end = start + 1;
            while end < pairs.len() && (attrs[end].ftype == VfType::Symlink) == symlink {
                end += 1;
            }
            let result = if symlink {
                let paths: Vec<_> = pairs[start..end]
                    .iter()
                    .map(|p| p.src_path.as_path())
                    .collect();
                self.vreadlink_impl(&paths).and_then(|targets| {
                    if targets.len() != end - start {
                        return Err(VfError::transport(
                            None,
                            "readlink backend returned an invalid result count",
                        ));
                    }
                    let targets: Vec<_> = targets
                        .into_iter()
                        .map(|bytes| path_from_bytes(&bytes))
                        .collect();
                    let sources: Vec<_> = targets.iter().map(|p| p.as_path()).collect();
                    let destinations: Vec<_> = pairs[start..end]
                        .iter()
                        .map(|p| p.dst_path.as_path())
                        .collect();
                    self.vsymlink_impl(&sources, &destinations)
                })
            } else {
                self.vcopy_data_nfs(&pairs[start..end])
            };
            result.map_err(|e| e.map_index(|index| start + index))?;
            start = end;
        }
        Ok(())
    }

    fn copy_tree_impl(
        &mut self,
        src_dir: &Path,
        dst: &Path,
        symlinks: bool,
        _use_server_side_copy: bool,
    ) -> VfRes {
        self.ensure_writable(1)?;
        if !self.exists_impl(dst)? {
            self.ensure_dir_impl(dst, 0o755)
                .map_err(|e| e.with_index(0))?;
        }
        let masks = AttrMask::MODE | AttrMask::SIZE | AttrMask::FILEID;
        let mut pending = vec![(src_dir.to_path_buf(), dst.to_path_buf())];
        while let Some((source, destination)) = pending.pop() {
            let entries = self.listdir_impl(&source, masks, 0, false)?;
            let mut directories = Vec::new();
            for entry in entries {
                let name = entry
                    .file
                    .path()
                    .and_then(|path| path.file_name())
                    .map(|name| path_bytes(Path::new(name)).to_vec())
                    .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
                let source_child = source.join(path_from_bytes(&name));
                let destination_child = destination.join(path_from_bytes(&name));
                if entry.ftype == VfType::Directory {
                    self.ensure_dir_impl(&destination_child, 0o755)
                        .map_err(|error| error.with_index(0))?;
                    directories.push((source_child, destination_child));
                } else if entry.ftype == VfType::Symlink && symlinks {
                    let target = self
                        .readlink_raw_impl(&source_child)
                        .map_err(|error| error.with_index(0))?;
                    self.symlink_raw_impl(&path_from_bytes(&target), &destination_child)
                        .map_err(|error| error.with_index(0))?;
                } else {
                    let pair =
                        ExtentPair::from_os_paths(&source_child, 0, &destination_child, 0, None);
                    let source_target = self
                        .follow_target_path(&self.server_path(&source_child))
                        .map_err(|error| error.with_index(0))?;
                    let destination_target = self
                        .follow_target_path(&self.server_path(&destination_child))
                        .map_err(|error| error.with_index(0))?;
                    self.copy_extent(&source_target, &destination_target, &pair)
                        .map_err(|error| error.with_index(0))?;
                }
            }
            for directory in directories.into_iter().rev() {
                pending.push(directory);
            }
        }
        Ok(())
    }
    fn vread_all_with_options_impl(
        &mut self,
        files: &[VfFile],
        options: ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        if !self.recovery_in_progress {
            return self
                .read_with_recovery(|client| client.vread_all_with_options_impl(files, options));
        }
        // Read until EOF in per-op chunks, batching every active file into
        // each compound so round trips scale with file size, not file count.
        // No size stat is needed: READ's EOF flag terminates each file.
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let per = self.nfs.read_per_op_bytes();
        // The window adapts to the number of active files: small files all
        // fit the first compound (so cat(20) is one compound), while big
        // files still get near-compound-sized windows.
        let compound_budget = if self.nfs.max_response_bytes > 0 {
            self.nfs.read_compound_bytes().saturating_sub(128)
        } else {
            per * 4
        };
        let max_window = per * (compound_budget / per).max(1);
        let mut out: Vec<Vec<u8>> = files.iter().map(|_| Vec::new()).collect();
        let mut offsets = vec![0u64; files.len()];
        let mut total = 0usize;
        let mut active: Vec<usize> = (0..files.len()).collect();
        while !active.is_empty() {
            let remaining = options.total_byte_limit().saturating_sub(total);
            let (batch_active, window) =
                bounded_read_allv_batch(&active, remaining, compound_budget, max_window);
            let reads: Vec<ReadOp> = batch_active
                .iter()
                .map(|&i| ReadOp::at(files[i].clone(), offsets[i], window))
                .collect();
            let results = self
                .vread_impl(&reads)
                .map_err(|error| remap_active_error(error, &batch_active))?;
            let mut next = merge_read_allv_round(
                &batch_active,
                &results,
                &mut out,
                &mut offsets,
                &mut total,
                options.total_byte_limit(),
            )?;
            let mut untouched = active.split_off(batch_active.len());
            untouched.append(&mut next);
            active = untouched;
        }
        Ok(out)
    }

    fn remove_paths_with_options_impl(
        &mut self,
        objs: &[&Path],
        recursive: bool,
        options: RemoveOptions,
    ) -> VfRes {
        self.ensure_writable(objs.len())?;
        // Resolve each operand's parent once; the operand itself is then
        // addressed by filehandle. Removal is optimistic: trying to REMOVE an
        // entry and expanding it on NFS4ERR_NOTEMPTY avoids a separate type
        // lookup that could race with the removal itself.
        let mut stack: Vec<RmTask> = Vec::new();
        let mut first_error: Option<VfError> = None;
        for (root, path) in objs.iter().enumerate() {
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeRemoveType { index: root })?;
            match self.remove_parent_handle(path) {
                Ok((parent, name)) => stack.push(RmTask::RemoveOrEnter {
                    parent,
                    name,
                    root,
                    expand: recursive,
                }),
                Err(error) => note_error(&mut first_error, error.with_index(root), options)?,
            }
        }
        self.run_rm_tasks(stack, &mut first_error, options)?;
        first_error.map_or(Ok(()), Err)
    }

    fn remove_dir_contents_path_with_options_impl(
        &mut self,
        dir: &Path,
        options: RemoveOptions,
    ) -> VfRes {
        let handle = self.open_dir_impl(dir)?;
        let result = self.remove_dir_contents_handle_with_options_impl(&handle, options);
        self.close_dir_impl(&handle)?;
        result
    }

    fn open_dir_impl(&mut self, path: &Path) -> VfResult<VfDir> {
        let full = self.server_vf_path(&VfFile::from_os_path(path))?;
        let fh = if normalize_bytes(path_bytes(&full)).is_empty() {
            self.nfs.root().clone()
        } else {
            let (parent, name) = self.remove_parent_handle(path)?;
            let (fh, ftype) = self
                .nfs
                .lookup_getattr(&parent, &name)
                .map_err(|error| vfsi_core::error_from_rpc(error, 0))?;
            if ftype != nfs_ftype4_NF4DIR {
                return Err(VfError::failure(0, ERR_NOTDIR));
            }
            fh
        };
        let fd = crate::vecfs::insert_fd(&mut self.next_fd, &mut self.open_dirs, fh)?;
        Ok(VfDir::Descriptor {
            fd,
            owner: self.dir_owner,
        })
    }

    fn close_dir_impl(&mut self, dir: &VfDir) -> VfResult<()> {
        match dir {
            VfDir::Descriptor { fd, owner } if *owner == self.dir_owner => {
                if !self.open_dirs.contains_key(fd) {
                    return Err(VfError::failure(0, ERR_EBADF));
                }
                self.open_dirs.remove(fd);
                Ok(())
            }
            _ => Err(VfError::failure(0, ERR_EBADF)),
        }
    }

    fn remove_dir_contents_handle_with_options_impl(
        &mut self,
        dir: &VfDir,
        options: RemoveOptions,
    ) -> VfRes {
        self.ensure_writable(1)?;
        let fh = match dir {
            VfDir::Descriptor { fd, owner } if *owner == self.dir_owner => self
                .open_dirs
                .get(fd)
                .cloned()
                .ok_or_else(|| VfError::failure(0, ERR_EBADF))?,
            _ => return Err(VfError::failure(0, ERR_EBADF)),
        };
        let mut first_error: Option<VfError> = None;
        // The directory itself is kept, so there is no parent entry to remove.
        let stack = vec![RmTask::Enter {
            dir: RemoveDir {
                fh,
                parent: None,
                name: Vec::new(),
                root: 0,
            },
            cookie: 0,
            pass_changed: false,
        }];
        self.run_rm_tasks(stack, &mut first_error, options)?;
        first_error.map_or(Ok(()), Err)
    }

    fn vwrite_adb_impl(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        self.ensure_writable(patterns.len())?;
        let mut counts = Vec::with_capacity(patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            // Validate the entire layout before creating/opening a file so an
            // overflow cannot strand server-side open state.
            let mut layout = Vec::with_capacity(p.adb_block_count);
            let pattern_len = u64::try_from(p.adb_pattern_data.len())
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            for b in 0..p.adb_block_count {
                let base = adb_block_base(p, b, i)?;
                let block_number = p
                    .adb_block_num
                    .checked_add(b as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                let number_offset = p
                    .adb_reloff_blocknum
                    .map(|relative| {
                        let offset = adb_field_offset(base, relative, i)?;
                        adb_field_offset(offset, 8, i)?;
                        Ok(offset)
                    })
                    .transpose()?;
                let pattern_offset = p
                    .adb_reloff_pattern
                    .map(|relative| {
                        let offset = adb_field_offset(base, relative, i)?;
                        adb_field_offset(offset, pattern_len, i)?;
                        Ok(offset)
                    })
                    .transpose()?;
                layout.push((block_number, number_offset, pattern_offset));
            }
            let full = self.server_path(&p.path);
            let (dir, name) = match split_path_bytes(path_bytes(&full)) {
                Ok(x) => x,
                Err(_) => return Err(VfError::failure(i, ERR_NOENT)),
            };
            let dirfh = match self.resolve_path(&path_from_bytes(&dir), true) {
                Ok(fh) => fh,
                Err(e) => return Err(e.with_index(i)),
            };
            let (fh, sid) = match self
                .nfs
                .open_path(
                    &dirfh,
                    &name,
                    OPEN4_SHARE_ACCESS_WRITE,
                    crate::client::OpenCreate::Guarded,
                )
                .map_err(|e| vfsi_core::error_from_rpc(e, 0))
            {
                Ok(x) => x,
                Err(e) => return Err(e.with_index(i)),
            };
            let mut written = 0usize;
            let mut failed: Option<VfError> = None;
            for (block_number, number_offset, pattern_offset) in layout {
                if let Some(offset) = number_offset {
                    let adbn = block_number.to_be_bytes();
                    if let Err(e) = self.nfs.write(&fh, &sid, offset, &adbn) {
                        failed = Some(vfsi_core::error_from_rpc(e, i));
                        break;
                    }
                }
                if let Some(offset) = pattern_offset
                    && !p.adb_pattern_data.is_empty()
                    && let Err(e) = self.nfs.write(&fh, &sid, offset, &p.adb_pattern_data)
                {
                    failed = Some(vfsi_core::error_from_rpc(e, i));
                    break;
                }
                written += 1;
            }
            let _ = self.nfs.close_path(&fh, &sid);
            if let Some(e) = failed {
                return Err(e);
            }
            counts.push(written);
        }
        Ok(counts)
    }
}
