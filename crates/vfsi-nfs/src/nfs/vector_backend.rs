//! VectorBackend dispatch and native vector execution.

use super::*;

// Strict listing collection selects a cause after independent execution.
// Its optional index is relative to this wave, including transport failures.
pub(super) fn remap_listing_error(
    error: vfsi_core::IndexedError,
    owners: &[usize],
) -> vfsi_core::IndexedError {
    match error.index() {
        Some(index) => match owners.get(index) {
            Some(&owner) => error.map_index(|_| owner),
            None => {
                vfsi_core::IndexedError::unattributed(vfsi_core::internal::errors::invalid_reply(
                    "READDIR error index is outside the submitted cohort",
                ))
            }
        },
        None => error,
    }
}

fn open_adb_path(
    mut open: impl FnMut(
        crate::client::OpenCreate,
    ) -> vfsi_core::RpcResult<(NfsFh, nfsv41_sys::stateid4)>,
) -> VfResult<(NfsFh, nfsv41_sys::stateid4)> {
    match open(crate::client::OpenCreate::NoCreate) {
        Ok(opened) => Ok(opened),
        Err(error) if error.status == nfsstat4_NFS4ERR_NOENT => {
            match open(crate::client::OpenCreate::Guarded) {
                Err(error)
                    if error.status == nfsstat4_NFS4ERR_EXIST && error.transport_kind.is_none() =>
                {
                    open(crate::client::OpenCreate::NoCreate)
                }
                result => result,
            }
            .map_err(vfsi_core::internal::errors::from_rpc)
        }
        Err(error) => Err(vfsi_core::internal::errors::from_rpc(error)),
    }
}

impl VectorBackend for NfsVecFs {
    fn vopen_raw_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> vfsi_core::VecResult<FileOperand> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        self.drain_deferred_descriptor_closes()?;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let rejected = self.read_only && flags.iter().any(|flags| open_flags_mutate(*flags));
        let mut results = if rejected {
            let indices: Vec<_> = flags
                .iter()
                .enumerate()
                .filter_map(|(index, flags)| (!open_flags_mutate(*flags)).then_some(index))
                .collect();
            let pending: Vec<_> = indices.iter().map(|&index| paths[index]).collect();
            let valid_flags: Vec<_> = indices.iter().map(|&index| flags[index]).collect();
            let valid_modes: Vec<_> = indices.iter().map(|&index| modes[index]).collect();
            let outcomes = if pending.is_empty() {
                Ok(Vec::new())
            } else {
                self.openv_merged(&pending, &valid_flags, &valid_modes)
            }
            .unwrap_or_else(|cause| {
                vfsi_core::internal::errors::fanout(cause, indices.len())
                    .map(|cause| Err(vfsi_core::OpError::not_attempted(cause)))
                    .collect()
            });
            let mut output: Vec<_> = (0..paths.len())
                .map(|_| {
                    Err(vfsi_core::OpError::not_attempted(
                        std::io::Error::from_raw_os_error(libc::EROFS),
                    ))
                })
                .collect();
            for (index, outcome) in indices.into_iter().zip(outcomes) {
                output[index] = outcome;
            }
            output
        } else {
            self.openv_merged(paths, flags, modes)?
        };
        for (index, result) in results.iter_mut().enumerate() {
            if !result.as_ref().err().is_some_and(|error| {
                error.state() == vfsi_core::FailureState::Failed
                    && vfsi_core::native_status(error.cause())
                        == Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_SYMLINK))
            }) {
                continue;
            }
            if flags[index] & (libc::O_CREAT | libc::O_EXCL) == (libc::O_CREAT | libc::O_EXCL) {
                *result = Err(vfsi_core::OpError::failed(
                    vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(
                        nfsstat4_NFS4ERR_EXIST,
                    )),
                    None,
                ));
                continue;
            }
            // This actual OPEN/lookup rejection made no mutation. Follow only
            // that input's symlink, preserving every already acquired sibling.
            *result = self
                .nfs
                .ensure_usable()
                .map_err(|error| {
                    vfsi_core::OpError::not_attempted(vfsi_core::internal::errors::from_rpc(error))
                })
                .and_then(|()| {
                    self.open_raw_impl(paths[index], flags[index], modes[index])
                        .map_err(|error| {
                            if crate::client::uncertain_completion(&error) {
                                vfsi_core::OpError::unknown(error, None)
                            } else {
                                vfsi_core::OpError::failed(error, None)
                            }
                        })
                });
        }
        Ok(results)
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &FileOperand) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCleanup { index: _index })?;
        Ok(())
    }

    fn vread_impl(&mut self, reads: &[ReadOp]) -> vfsi_core::VecResult<ReadOutput> {
        self.read_results(reads)
    }

    fn vread_into_impl(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> vfsi_core::VecResult<ReadIntoResult> {
        self.read_into_results(reads, buffers)
    }

    fn vwrite_impl(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> vfsi_core::VecResult<WriteOutput> {
        self.write_results(writes)
    }

    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        self.getattr_with_recovery(attrs, true)
    }

    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> vfsi_core::VecResult<()> {
        self.getattr_with_recovery(attrs, false)
    }

    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        self.vsetattrs_nfs(attrs, true)
    }

    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
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
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
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
                cookie: DirCookie::default(),
                seen_cookies: std::collections::HashSet::new(),
                buffered: VecDeque::new(),
            },
        };
        let ids = request_mask_to_attr_list(&masks);
        if state.buffered.is_empty() {
            let (page, next_cookie) = self
                .nfs
                .readdir(&state.fh, state.cookie, &ids)
                .map_err(vfsi_core::internal::errors::from_rpc)?;
            if page.is_empty() {
                return Ok((Vec::new(), None));
            }
            validate_readdir_progress(
                state.cookie.value,
                next_cookie.value,
                &mut state.seen_cookies,
                None,
            )?;
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
                file: FileOperand::from_os_path(&path),
                masks,
                ..VfAttrs::default()
            };
            let values = parse_metadata_attrs(&entry.attrmask, &entry.attrs)?;
            apply_attrs(&mut attrs, &values);
            output.push(attrs);
        }
        let next = if !state.buffered.is_empty() || state.cookie.value != 0 {
            Some(DirPageCursor::new(state))
        } else {
            None
        };
        Ok((output, next))
    }

    fn directory_page_batch_size(&self) -> usize {
        // Bound speculative siblings, not the negotiated protocol maximum.
        // Anchored descent uses four ops per child plus SEQUENCE. A 24-child
        // cohort stays below 100 ops even after adaptive growth, while larger
        // cohorts repeatedly probe that boundary on metadata-heavy trees.
        24
    }

    fn vlistdir_pages_impl(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        cursors: Vec<Option<DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
        follow_symlinks: bool,
    ) -> vfsi_core::VecResult<vfsi_sync::BackendDirectoryPage> {
        self.vlistdir_pages_nfs(
            dirs,
            masks,
            cursors,
            page_size,
            max_entries,
            follow_symlinks,
        )
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
        let mut cookie = DirCookie::default();
        let mut seen_cookies = std::collections::HashSet::new();
        let mut count = 0usize;
        loop {
            let (page, next_cookie) = self
                .nfs
                .readdir(&fh, cookie, &ids)
                .map_err(vfsi_core::internal::errors::from_rpc)?;
            for entry in &page {
                if max_entries != 0 && count >= max_entries {
                    return Ok(());
                }
                let path = dir.join(path_from_bytes(&entry.name));
                let mut attrs = VfAttrs {
                    file: FileOperand::from_os_path(&path),
                    masks,
                    ..VfAttrs::default()
                };
                let values = parse_metadata_attrs(&entry.attrmask, &entry.attrs)?;
                apply_attrs(&mut attrs, &values);
                if !cb(&attrs) {
                    return Ok(());
                }
                count += 1;
            }
            let next = next_cookie;
            if next.value == 0 {
                return Ok(());
            }
            validate_readdir_progress(cookie.value, next.value, &mut seen_cookies, None)?;
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
    ) -> Result<(), vfsi_core::IndexedError> {
        use vfsi_core::IndexedError;
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
            let files: Vec<FileOperand> = level_paths
                .iter()
                .map(|p| FileOperand::from_os_path(p))
                .collect();
            let refs: Vec<&FileOperand> = files.iter().collect();
            let resolved = self
                .resolve_files_nfs(&refs, true)
                .map_err(IndexedError::unattributed)?;
            let mut level: Vec<(NfsFh, PathBuf, usize)> = Vec::with_capacity(level_paths.len());
            for (i, r) in resolved.into_iter().enumerate() {
                let owner = level_owners[i];
                match r {
                    Ok((fh, ftype)) if ftype == nfs_ftype4_NF4DIR => {
                        level.push((fh, level_paths[i].clone(), owner));
                    }
                    Ok((_, _)) => {
                        return Err(IndexedError::at(
                            owner,
                            vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(
                                nfsstat4_NFS4ERR_NOTDIR,
                            )),
                        ));
                    }
                    Err(error) => return Err(IndexedError::at(owner, error.into_cause())),
                }
            }
            // First pages for all directories in one compound.
            let ops: Vec<(NfsFh, DirCookie)> = level
                .iter()
                .map(|(fh, _, _)| (fh.clone(), DirCookie::default()))
                .collect();
            let results = {
                let page_ops = &ops;
                vfsi_core::internal::collect_vec_result(
                    self.nfs.readdir_pages(page_ops, &ids),
                    page_ops.len(),
                )
            }
            .map_err(|error| remap_listing_error(error, &level_owners))?;
            if results.len() != level.len() {
                return Err(IndexedError::unattributed(
                    vfsi_core::internal::errors::invalid_reply(
                        "READDIR returned the wrong number of directory pages",
                    ),
                ));
            }
            // Emit each bounded READDIR page before fetching a continuation.
            // Retaining every page until EOF used memory proportional to the
            // entire listing even when the caller requested an early stop.
            let mut next_level: Vec<PathBuf> = Vec::new();
            let mut next_owners: Vec<usize> = Vec::new();
            let mut emit_page =
                |idx: usize, entries: Vec<crate::client::DirEntry>| -> Result<bool, IndexedError> {
                    let dir = &level[idx].1;
                    let owner = level[idx].2;
                    for e in entries {
                        if max_entries != 0 && counted >= max_entries {
                            return Ok(false);
                        }
                        let path = dir.join(path_from_bytes(&e.name));
                        let mut a = VfAttrs {
                            file: FileOperand::from_os_path(&path),
                            masks,
                            ..VfAttrs::default()
                        };
                        let vals = parse_metadata_attrs(&e.attrmask, &e.attrs)
                            .map_err(|cause| IndexedError::at(owner, cause))?;
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
                let mut seen_cookies = std::collections::HashSet::new();
                validate_readdir_progress(0, cookie.value, &mut seen_cookies, Some(level[idx].2))
                    .map_err(|cause| IndexedError::at(level[idx].2, cause))?;
                if cookie.value != 0 {
                    pending.push((idx, level[idx].0.clone(), cookie, seen_cookies));
                }
            }
            while !pending.is_empty() {
                let cont_ops: Vec<(NfsFh, DirCookie)> = pending
                    .iter()
                    .map(|(_, fh, cookie, _)| (fh.clone(), *cookie))
                    .collect();
                let pending_owners: Vec<usize> =
                    pending.iter().map(|(idx, _, _, _)| level[*idx].2).collect();
                let cont = {
                    let page_ops = &cont_ops;
                    vfsi_core::internal::collect_vec_result(
                        self.nfs.readdir_pages(page_ops, &ids),
                        page_ops.len(),
                    )
                }
                .map_err(|error| remap_listing_error(error, &pending_owners))?;
                if cont.len() != pending.len() {
                    return Err(IndexedError::unattributed(
                        vfsi_core::internal::errors::invalid_reply(
                            "READDIR continuation returned the wrong number of pages",
                        ),
                    ));
                }
                let mut next_pending = Vec::new();
                for ((idx, fh, previous_cookie, mut seen_cookies), (entries, cookie)) in
                    pending.into_iter().zip(cont)
                {
                    validate_readdir_progress(
                        previous_cookie.value,
                        cookie.value,
                        &mut seen_cookies,
                        Some(level[idx].2),
                    )
                    .map_err(|cause| IndexedError::at(level[idx].2, cause))?;
                    if !emit_page(idx, entries)? {
                        return Ok(());
                    }
                    if cookie.value != 0 {
                        next_pending.push((idx, fh, cookie, seen_cookies));
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
        let mut budget = vfsi_core::internal::TraversalBudget::new(
            options.max_entries_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
            options.max_path_bytes_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
        );

        // Resolve the root once, then decode each bounded READDIR response
        // directly into the caller-visible accumulator. Raw continuation
        // pages are never retained after they are decoded.
        let mut root_attrs = Vec::new();
        let mut cookie = DirCookie::default();
        let mut seen_cookies = std::collections::HashSet::new();
        loop {
            let (page, next_cookie) = self
                .nfs
                .readdir(&root_fh, cookie, &ids)
                .map_err(vfsi_core::internal::errors::from_rpc)?;
            append_bounded_walk_page(
                root,
                root,
                masks,
                &page,
                options,
                &mut entry_count,
                &mut budget,
                &mut root_attrs,
            )?;
            validate_readdir_progress(cookie.value, next_cookie.value, &mut seen_cookies, None)?;
            cookie = next_cookie;
            if cookie.value == 0 {
                break;
            }
        }
        sort(root, &mut root_attrs);

        // Preserve the parent filehandle so resolving and reading each child
        // directory can share one compound instead of re-walking full paths.
        let mut frontier: Vec<(NfsFh, PathBuf)> = root_attrs
            .iter()
            .filter(|entry| {
                entry.ftype == VfType::Directory
                    && options.max_depth_or(Default::default()).get() > 0
            })
            .filter_map(|entry| {
                entry
                    .file
                    .path()
                    .map(|path| (root_fh.clone(), path.to_path_buf()))
            })
            .collect();
        collected.insert(root.to_path_buf(), root_attrs);

        while !frontier.is_empty() {
            let operations: Vec<(NfsFh, Vec<u8>)> = frontier
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
            let results = {
                let page_ops = &operations;
                crate::client::collect_rpc_results(
                    self.nfs.readdir_children(page_ops, &ids),
                    page_ops.len(),
                )
            }
            .map_err(vfsi_core::internal::errors::from_rpc)?;
            let mut level_attrs: Vec<Vec<VfAttrs>> =
                (0..results.len()).map(|_| Vec::new()).collect();
            let mut pending = Vec::new();
            for (index, result) in results.iter().enumerate() {
                append_bounded_walk_page(
                    root,
                    &frontier[index].1,
                    masks,
                    &result.entries,
                    options,
                    &mut entry_count,
                    &mut budget,
                    &mut level_attrs[index],
                )?;
                let mut seen_cookies = std::collections::HashSet::new();
                validate_readdir_progress(0, result.cookie.value, &mut seen_cookies, None)?;
                if result.cookie.value != 0 {
                    pending.push((index, result.fh.clone(), result.cookie, seen_cookies));
                }
            }

            while !pending.is_empty() {
                let operations: Vec<(NfsFh, DirCookie)> = pending
                    .iter()
                    .map(|(_, handle, cookie, _)| (handle.clone(), *cookie))
                    .collect();
                let pages = {
                    let page_ops = &operations;
                    crate::client::collect_rpc_results(
                        self.nfs.readdir_pages(page_ops, &ids),
                        page_ops.len(),
                    )
                }
                .map_err(vfsi_core::internal::errors::from_rpc)?;
                let mut next_pending = Vec::new();
                for ((index, handle, previous_cookie, mut seen_cookies), (page, cookie)) in
                    pending.into_iter().zip(pages)
                {
                    validate_readdir_progress(
                        previous_cookie.value,
                        cookie.value,
                        &mut seen_cookies,
                        None,
                    )?;
                    append_bounded_walk_page(
                        root,
                        &frontier[index].1,
                        masks,
                        &page,
                        options,
                        &mut entry_count,
                        &mut budget,
                        &mut level_attrs[index],
                    )?;
                    if cookie.value != 0 {
                        next_pending.push((index, handle, cookie, seen_cookies));
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
                            <= options.max_depth_or(Default::default()).get()
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
                .filter(|path| collected.contains_key(path))
                .collect();
            for s in subs.into_iter().rev() {
                stack.push(s);
            }
            out.push(WalkEntry { path: dir, entries });
        }
        Ok(out)
    }

    fn vrename_impl(&mut self, pairs: &[(FileOperand, FileOperand)]) -> vfsi_core::VecResult<()> {
        self.vrename_nfs(pairs)
    }

    fn vremove_impl(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        self.vremove_nfs(files)
    }

    fn vmkdir_impl(&mut self, dirs: &[VfAttrs]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        self.ensure_writable(dirs.len())?;
        let mut slots = (0..dirs.len()).map(|_| None).collect::<Vec<_>>();
        let mut paths = Vec::with_capacity(dirs.len());
        let mut indices = Vec::with_capacity(dirs.len());
        for (index, dir) in dirs.iter().enumerate() {
            match self.server_vf_path(&dir.file) {
                Ok(path) => {
                    paths.push(self.visible_path(&path));
                    indices.push(index);
                }
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error))),
            }
        }
        let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
        let parents = self.resolve_link_parents(&refs)?;
        let mut requests = Vec::with_capacity(paths.len());
        let mut active = Vec::with_capacity(paths.len());
        for (index, parent) in indices.into_iter().zip(parents) {
            match parent {
                Ok((parent, name)) => {
                    active.push(index);
                    requests.push(crate::client::CreateOp {
                        dir: parent,
                        name,
                        ftype: nfs_ftype4_NF4DIR,
                        linkdata: None,
                        mode: dirs[index]
                            .masks
                            .contains(AttrMask::MODE)
                            .then_some(dirs[index].mode),
                    });
                }
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error.into_cause()))),
            }
        }
        let outcomes = match self.nfs.create_many(&requests) {
            Ok(outcomes) => outcomes,
            Err(cause) => vfsi_core::internal::errors::fanout(cause, requests.len())
                .map(|cause| Err(OpError::not_attempted(cause)))
                .collect(),
        };
        for (index, outcome) in active.into_iter().zip(outcomes) {
            slots[index] = Some(outcome);
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply("missing mkdir outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    fn vsymlink_impl(
        &mut self,
        oldpaths: &[&Path],
        newpaths: &[&Path],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if oldpaths.len() != newpaths.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        self.ensure_writable(newpaths.len())?;
        let parents = self.resolve_link_parents(newpaths)?;
        let mut slots = (0..newpaths.len()).map(|_| None).collect::<Vec<_>>();
        let mut indices = Vec::with_capacity(newpaths.len());
        let mut ops = Vec::with_capacity(newpaths.len());
        for (index, result) in parents.into_iter().enumerate() {
            match result {
                Ok((dir, name)) => {
                    indices.push(index);
                    ops.push(crate::client::CreateOp {
                        dir,
                        name,
                        ftype: nfs_ftype4_NF4LNK,
                        linkdata: Some(path_bytes(oldpaths[index]).to_vec()),
                        mode: None,
                    });
                }
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error.into_cause()))),
            }
        }
        let outcomes = match self.nfs.create_many(&ops) {
            Ok(outcomes) => outcomes,
            Err(cause) => vfsi_core::internal::errors::fanout(cause, ops.len())
                .map(|cause| Err(OpError::not_attempted(cause)))
                .collect(),
        };
        for (index, result) in indices.into_iter().zip(outcomes) {
            slots[index] = Some(result);
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply("missing symlink outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    fn vreadlink_impl(&mut self, paths: &[&Path]) -> vfsi_core::VecResult<Vec<u8>> {
        use vfsi_core::OpError;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        if !self.recovery_in_progress {
            return self.vector_read_with_recovery(
                |_, _| true,
                |client, indices| match indices {
                    None => client.vreadlink_impl(paths),
                    Some(indices) => {
                        let pending: Vec<_> = indices.iter().map(|&index| paths[index]).collect();
                        client.vreadlink_impl(&pending)
                    }
                },
            );
        }
        let files: Vec<_> = paths
            .iter()
            .map(|path| FileOperand::from_os_path(path))
            .collect();
        let refs: Vec<_> = files.iter().collect();
        let resolved = self.resolve_files_nfs(&refs, false)?;
        let mut slots = (0..paths.len()).map(|_| None).collect::<Vec<_>>();
        let mut ops = Vec::with_capacity(paths.len());
        let mut indices = Vec::with_capacity(paths.len());
        for (index, result) in resolved.into_iter().enumerate() {
            match result {
                Ok((fh, _)) => {
                    indices.push(index);
                    ops.push(crate::client::ReadlinkOp { fh });
                }
                Err(error) => slots[index] = Some(Err(error)),
            }
        }
        let outcomes = match self.nfs.readlink_many(&ops) {
            Ok(outcomes) => outcomes,
            Err(error) => vfsi_core::internal::errors::fanout(error, ops.len())
                .map(|error| Err(OpError::not_attempted(error)))
                .collect(),
        };
        for (index, result) in indices.into_iter().zip(outcomes) {
            slots[index] = Some(result);
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply("missing NFS readlink outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    fn vhardlink_impl(
        &mut self,
        oldpaths: &[&Path],
        newpaths: &[&Path],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if oldpaths.len() != newpaths.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        self.ensure_writable(newpaths.len())?;
        let files: Vec<_> = oldpaths
            .iter()
            .map(|path| FileOperand::from_os_path(path))
            .collect();
        let refs: Vec<_> = files.iter().collect();
        let sources = self.resolve_files_nfs(&refs, false)?;
        let mut slots = (0..newpaths.len()).map(|_| None).collect::<Vec<_>>();
        let mut candidates = Vec::with_capacity(newpaths.len());
        for (index, source) in sources.into_iter().enumerate() {
            match source {
                Ok((fh, _)) => candidates.push((index, fh)),
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error.into_cause()))),
            }
        }
        let destinations: Vec<_> = candidates
            .iter()
            .map(|(index, _)| newpaths[*index])
            .collect();
        let parents = self.resolve_link_parents(&destinations)?;
        let mut indices = Vec::with_capacity(candidates.len());
        let mut ops = Vec::with_capacity(candidates.len());
        for ((index, src), parent) in candidates.into_iter().zip(parents) {
            match parent {
                Ok((dstdir, newname)) => {
                    indices.push(index);
                    ops.push(crate::client::LinkOp {
                        src,
                        dstdir,
                        newname,
                    });
                }
                Err(error) => slots[index] = Some(Err(OpError::not_attempted(error.into_cause()))),
            }
        }
        let outcomes = match self.nfs.link_many(&ops) {
            Ok(outcomes) => outcomes,
            Err(cause) => vfsi_core::internal::errors::fanout(cause, ops.len())
                .map(|cause| Err(OpError::not_attempted(cause)))
                .collect(),
        };
        for (index, result) in indices.into_iter().zip(outcomes) {
            slots[index] = Some(result);
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply("missing hard-link outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    fn vcopy_data_impl(&mut self, pairs: &[ExtentPair]) -> vfsi_core::VecResult<()> {
        self.ensure_writable(pairs.len())?;
        let mut fatal: Option<std::sync::Arc<std::io::Error>> = None;
        Ok(pairs
            .iter()
            .map(|pair| {
                if let Some(cause) = &fatal {
                    return Err(vfsi_core::OpError::not_attempted(
                        vfsi_core::internal::errors::from_shared(cause),
                    ));
                }
                let result = (|| {
                    let source = self.follow_target_path(&self.server_path(&pair.src_path))?;
                    let destination = self.follow_target_path(&self.server_path(&pair.dst_path))?;
                    self.copy_extent(&source, &destination, pair)
                })();
                result.map_err(|mut cause| {
                    if crate::client::uncertain_completion(&cause) {
                        fatal = Some(vfsi_core::internal::errors::share_in_place(&mut cause));
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
        self.ensure_writable(pairs.len())?;
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        if options.follow_source_symlinks() {
            return self.vcopy_data_nfs(pairs);
        }
        let mut attrs: Vec<_> = pairs
            .iter()
            .map(|pair| VfAttrs {
                file: FileOperand::from_os_path(&pair.src_path),
                masks: AttrMask::empty(),
                ..VfAttrs::default()
            })
            .collect();
        let mut results =
            super::copy::copy_slots(self.vgetattrs_nofollow_impl(&mut attrs), pairs.len());
        let mut start = 0;
        while start < pairs.len() {
            if results[start].is_err() {
                start += 1;
                continue;
            }
            let symlink = attrs[start].ftype == VfType::Symlink;
            let mut end = start + 1;
            while end < pairs.len()
                && results[end].is_ok()
                && (attrs[end].ftype == VfType::Symlink) == symlink
            {
                end += 1;
            }
            if symlink {
                let paths: Vec<_> = pairs[start..end]
                    .iter()
                    .map(|pair| pair.src_path.as_path())
                    .collect();
                let targets = super::copy::copy_slots(self.vreadlink_impl(&paths), paths.len());
                let mut links = Vec::new();
                let mut indices = Vec::new();
                for (offset, target) in targets.into_iter().enumerate() {
                    match target {
                        Ok(target) => {
                            indices.push(start + offset);
                            links.push(path_from_bytes(&target));
                        }
                        Err(error) => results[start + offset] = Err(error),
                    }
                }
                let sources: Vec<_> = links.iter().map(PathBuf::as_path).collect();
                let destinations: Vec<_> = indices
                    .iter()
                    .map(|&index| pairs[index].dst_path.as_path())
                    .collect();
                for (index, result) in indices.iter().copied().zip(super::copy::copy_slots(
                    self.vsymlink_impl(&sources, &destinations),
                    sources.len(),
                )) {
                    results[index] = result;
                }
            } else {
                let batch =
                    super::copy::copy_slots(self.vcopy_data_nfs(&pairs[start..end]), end - start);
                for (slot, result) in results[start..end].iter_mut().zip(batch) {
                    *slot = result;
                }
            }
            start = end;
        }
        Ok(results)
    }

    fn copy_tree_impl(
        &mut self,
        src_dir: &Path,
        dst: &Path,
        symlinks: bool,
        use_server_side_copy: bool,
    ) -> VfRes {
        self.ensure_writable(1)?;
        vfsi_sync::backend::helpers::copy_tree_batches_with(
            self,
            src_dir,
            dst,
            symlinks,
            AttrMask::MODE | AttrMask::SIZE | AttrMask::FILEID,
            |backend, pairs| {
                if use_server_side_copy {
                    vfsi_core::internal::collect_vec_result(
                        backend.vcopy_data_nfs(pairs),
                        pairs.len(),
                    )
                    .map_err(vfsi_core::IndexedError::into_cause)
                    .map(|_| ())
                } else {
                    vfsi_core::internal::collect_vec_result(
                        backend.vcopy_data_impl(pairs),
                        pairs.len(),
                    )
                    .map_err(vfsi_core::IndexedError::into_cause)
                    .map(|_| ())
                }
            },
            |error| error,
        )
    }
    fn vread_all_with_options_impl(
        &mut self,
        files: &[FileOperand],
        options: ReadAllOptions,
    ) -> vfsi_core::VecResult<Vec<u8>> {
        let per = self.nfs.read_per_op_bytes();
        let compound_budget = if self.nfs.max_response_bytes > 0 {
            self.nfs.read_compound_bytes().saturating_sub(128)
        } else {
            per * 4
        };
        let max_window = per * (compound_budget / per).max(1);
        // Recovery belongs to each read wave, never to the accumulated file
        // vector: completed chunks and successful siblings must not be replayed.
        vfsi_sync::backend::helpers::read_all_vectors(
            self,
            files,
            options,
            |pending, remaining| {
                bounded_read_allv_batch(pending, remaining, compound_budget, max_window)
            },
            false,
        )
    }

    fn remove_paths_with_options_impl(
        &mut self,
        objs: &[&Path],
        mode: vfsi_core::api::RemoveMode,
        options: RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::{OpError, api::RemoveMode};
        self.ensure_writable(objs.len())?;
        if objs.is_empty() {
            return Ok(Vec::new());
        }
        if mode == RemoveMode::Entry && options == RemoveOptions::default() {
            let files: Vec<_> = objs
                .iter()
                .map(|path| FileOperand::from_os_path(path))
                .collect();
            return self.vremove_impl(&files);
        }
        let mut stack = Vec::new();
        let mut roots: Vec<_> = (0..objs.len()).map(|_| RemoveRoot::Pending).collect();
        if mode == RemoveMode::Contents {
            // Resolve every no-follow directory object in a native vector.
            // The walker retains these filehandles and never reopens paths.
            let files: Vec<_> = objs
                .iter()
                .map(|path| FileOperand::from_os_path(path))
                .collect();
            let refs: Vec<_> = files.iter().collect();
            for (root, result) in self
                .resolve_files_nfs(&refs, false)?
                .into_iter()
                .enumerate()
            {
                match result {
                    Ok((fh, kind)) if kind == nfs_ftype4_NF4DIR => stack.push(RmTask::Enter {
                        dir: RemoveDir {
                            fh,
                            parent: None,
                            name: Box::default(),
                            root,
                            is_root: true,
                        },
                        cookie: DirCookie::default(),
                        seen_cookies: std::collections::HashSet::new(),
                        pass_changed: false,
                    }),
                    Ok(_) => {
                        roots[root] = RemoveRoot::Failed(OpError::not_attempted(
                            std::io::Error::from_raw_os_error((ERR_NOTDIR) as i32),
                        ))
                    }
                    Err(error) => roots[root] = RemoveRoot::Failed(error),
                }
            }
        } else {
            for (root, path) in objs.iter().enumerate() {
                #[cfg(feature = "test-faults")]
                if let Err(cause) =
                    self.inject_open_fault(OpenFaultPoint::BeforeRemoveType { index: root })
                {
                    let fatal = vfsi_core::transport_kind(&(cause)).is_some();
                    roots[root] = RemoveRoot::Failed(OpError::not_attempted(cause));
                    if fatal {
                        break;
                    }
                    continue;
                }
                match self.remove_parent_handle(path) {
                    Ok((parent, name)) => stack.push(RmTask::RemoveOrEnter {
                        parent,
                        name: name.into_boxed_slice(),
                        root,
                        expand: mode == RemoveMode::Tree,
                    }),
                    Err(cause) => {
                        let fatal = vfsi_core::transport_kind(&(cause)).is_some();
                        roots[root] = RemoveRoot::Failed(OpError::not_attempted(cause));
                        if fatal {
                            break;
                        }
                    }
                }
            }
        }
        Ok(self.run_rm_tasks(stack, roots, options))
    }

    fn vopen_dirs_impl(&mut self, paths: &[&Path]) -> vfsi_core::VecResult<VfDir> {
        use vfsi_core::OpError;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let files: Vec<_> = paths
            .iter()
            .map(|path| FileOperand::from_os_path(path))
            .collect();
        let refs: Vec<_> = files.iter().collect();
        self.resolve_files_nfs(&refs, false).map(|results| {
            results
                .into_iter()
                .map(|result| {
                    result.and_then(|(fh, kind)| {
                        if kind != nfs_ftype4_NF4DIR {
                            return Err(OpError::failed(
                                std::io::Error::from_raw_os_error((ERR_NOTDIR) as i32),
                                None,
                            ));
                        }
                        // Directory filehandles have no server-side OPEN state.
                        // Registration failure drops only this acquired filehandle.
                        let fd = vfsi_core::insert_fd(&mut self.next_fd, &mut self.open_dirs, fh)
                            .map_err(|cause| OpError::failed(cause, None))?;
                        Ok(VfDir::Descriptor {
                            fd,
                            owner: self.dir_owner,
                        })
                    })
                })
                .collect()
        })
    }

    fn close_dir_impl(&mut self, dir: &VfDir) -> VfResult<()> {
        match dir {
            VfDir::Descriptor { fd, owner } if *owner == self.dir_owner => {
                if !self.open_dirs.contains_key(fd) {
                    return Err(std::io::Error::from_raw_os_error((ERR_EBADF) as i32));
                }
                self.open_dirs.remove(fd);
                Ok(())
            }
            _ => Err(std::io::Error::from_raw_os_error((ERR_EBADF) as i32)),
        }
    }

    fn remove_dir_contents_handles_with_options_impl(
        &mut self,
        dirs: &[&VfDir],
        options: RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        self.ensure_writable(dirs.len())?;
        let mut roots: Vec<_> = (0..dirs.len()).map(|_| RemoveRoot::Pending).collect();
        let mut stack = Vec::new();
        for (root, dir) in dirs.iter().enumerate() {
            let handle = match dir {
                VfDir::Descriptor { fd, owner } if *owner == self.dir_owner => {
                    self.open_dirs.get(fd).cloned()
                }
                _ => None,
            };
            match handle {
                Some(fh) => stack.push(RmTask::Enter {
                    dir: RemoveDir {
                        fh,
                        parent: None,
                        name: Box::default(),
                        root,
                        is_root: true,
                    },
                    cookie: DirCookie::default(),
                    seen_cookies: std::collections::HashSet::new(),
                    pass_changed: false,
                }),
                None => {
                    roots[root] = RemoveRoot::Failed(OpError::not_attempted(
                        std::io::Error::from_raw_os_error((ERR_EBADF) as i32),
                    ))
                }
            }
        }
        Ok(self.run_rm_tasks(stack, roots, options))
    }

    fn vwrite_adb_impl(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        self.ensure_writable(patterns.len())?;
        let mut counts = Vec::with_capacity(patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            let layout = vfsi_core::internal::adb_layout(p)?;
            let full = self.server_path(&p.path);
            let (dir, name) = match split_path_bytes(path_bytes(&full)) {
                Ok(x) => x,
                Err(_) => return Err(std::io::Error::from_raw_os_error((ERR_NOENT) as i32)),
            };
            let dirfh = self.resolve_path(&path_from_bytes(dir), true)?;
            let (fh, sid) = open_adb_path(|create| {
                self.nfs
                    .open_path(&dirfh, name, OPEN4_SHARE_ACCESS_WRITE, create)
            })?;
            let mut written = 0usize;
            let mut failed: Option<std::io::Error> = None;
            for (block_number, number_offset, pattern_offset) in layout {
                if let Some(offset) = number_offset {
                    let adbn = block_number.to_be_bytes();
                    if let Err(error) = super::io::write_all_at(
                        &mut self.nfs,
                        i,
                        offset,
                        &adbn,
                        |nfs, offset, data| {
                            nfs.write(&fh, &sid, offset, data)
                                .map(|(written, _)| written as usize)
                                .map_err(vfsi_core::internal::errors::from_rpc)
                        },
                    ) {
                        failed = Some(error);
                        break;
                    }
                }
                if let Some(offset) = pattern_offset
                    && !p.adb_pattern_data.is_empty()
                    && let Err(error) = super::io::write_all_at(
                        &mut self.nfs,
                        i,
                        offset,
                        &p.adb_pattern_data,
                        |nfs, offset, data| {
                            nfs.write(&fh, &sid, offset, data)
                                .map(|(written, _)| written as usize)
                                .map_err(vfsi_core::internal::errors::from_rpc)
                        },
                    )
                {
                    failed = Some(error);
                    break;
                }
                written += 1;
            }
            let _ = self.nfs.close_temporary_path(&fh, &sid);
            if let Some(e) = failed {
                return Err(e);
            }
            counts.push(written);
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod adb_path_tests {
    use super::*;
    use crate::client::NfsFh;
    use nfsv41_sys::stateid4;

    fn opened() -> (NfsFh, stateid4) {
        (
            NfsFh::test_from_bytes([1u8, 2]),
            stateid4 {
                seqid: 3,
                other: [4; 12],
            },
        )
    }

    #[test]
    fn adb_creation_race_reopens_once_and_preserves_terminal_errors() {
        use crate::client::OpenCreate;
        for terminal in [None, Some(nfsstat4_NFS4ERR_ACCESS)] {
            let mut calls = Vec::new();
            let result = open_adb_path(|create| {
                calls.push(create);
                let status = match calls.len() {
                    1 => Some(nfsstat4_NFS4ERR_NOENT),
                    2 => Some(nfsstat4_NFS4ERR_EXIST),
                    3 => terminal,
                    _ => panic!("ADB create retry must be bounded"),
                };
                status.map_or_else(
                    || Ok(opened()),
                    |status| Err(vfsi_core::RpcError::op(0, status)),
                )
            });
            assert_eq!(
                calls,
                [
                    OpenCreate::NoCreate,
                    OpenCreate::Guarded,
                    OpenCreate::NoCreate
                ]
            );
            match terminal {
                None => assert_eq!(result.unwrap().0, opened().0),
                Some(code) => assert_eq!(crate::test_native_code(&result.unwrap_err()), code),
            }
        }
    }

    #[test]
    fn adb_does_not_retry_terminal_or_uncertain_creates() {
        for phase in ["denied", "uncached", "lost"] {
            let mut calls = Vec::new();
            let error = open_adb_path(|create| {
                calls.push(create);
                Err(if calls.len() == 1 {
                    vfsi_core::RpcError::op(0, nfsstat4_NFS4ERR_NOENT)
                } else {
                    match phase {
                        "denied" => vfsi_core::RpcError::op(0, nfsstat4_NFS4ERR_ACCESS),
                        "uncached" => {
                            vfsi_core::RpcError::op(0, nfsstat4_NFS4ERR_RETRY_UNCACHED_REP)
                        }
                        _ => vfsi_core::RpcError::transport("lost CREATE reply"),
                    }
                })
            })
            .unwrap_err();
            assert_eq!(
                calls,
                [
                    crate::client::OpenCreate::NoCreate,
                    crate::client::OpenCreate::Guarded
                ]
            );
            if phase == "lost" {
                assert!(vfsi_core::transport_kind(&error).is_some());
            } else {
                assert_eq!(
                    crate::test_native_code(&error),
                    if phase == "denied" {
                        nfsstat4_NFS4ERR_ACCESS
                    } else {
                        nfsstat4_NFS4ERR_RETRY_UNCACHED_REP
                    }
                );
            }
        }
    }

    #[cfg(feature = "test-faults")]
    #[test]
    fn live_adb_retains_temporary_close_owners_after_success_and_write_failure() {
        use std::sync::Arc;
        use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for write_fails in [false, true] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            let path = PathBuf::from(format!(
                "/adb-close-owner-{}-{write_fails}",
                std::process::id()
            ));
            let fault = Arc::new(FaultScript::one(
                OpenFaultPoint::BeforePathClose,
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ));
            fs.set_fault_injector(fault.clone());
            if write_fails {
                fs.nfs.inject_resource_rejection_once(b"write");
            }
            let result =
                fs.vwrite_adb_impl(&[Adb::blocknum_only(path.to_str().unwrap(), 0, 8, 1, 0, 17)]);
            let pending = fs.nfs.deferred_path_close_count();
            let confirmed = fs.nfs.confirmed_path_closes();
            let size = fs.stat_impl(&path).unwrap().size;
            fs.unlink_impl(&path).unwrap();
            fs.nfs.shutdown().unwrap();
            assert!(fault.is_consumed());
            assert_eq!(
                pending, 1,
                "exact temporary owner must survive failed CLOSE"
            );
            assert_eq!(fs.nfs.deferred_path_close_count(), 0);
            assert_eq!(fs.nfs.confirmed_path_closes(), confirmed + 1);
            if write_fails {
                assert_eq!(
                    crate::test_native_code(&result.unwrap_err()),
                    nfsstat4_NFS4ERR_RESOURCE
                );
                assert_eq!(size, 0);
            } else {
                assert_eq!(result.unwrap(), [1]);
                assert_eq!(size, 8);
            }
        }
    }

    #[test]
    fn adb_reuses_existing_path_and_only_creates_after_noent() {
        let expected = opened();
        let mut calls = Vec::new();
        let actual = open_adb_path(|create| {
            calls.push(create);
            assert_eq!(create, crate::client::OpenCreate::NoCreate);
            Ok(expected.clone())
        })
        .unwrap();
        assert_eq!(actual.0, expected.0);
        assert_eq!(calls, [crate::client::OpenCreate::NoCreate]);

        let expected = opened();
        let mut calls = Vec::new();
        let actual = open_adb_path(|create| {
            calls.push(create);
            match create {
                crate::client::OpenCreate::NoCreate => {
                    Err(vfsi_core::RpcError::op(0, nfsstat4_NFS4ERR_NOENT))
                }
                crate::client::OpenCreate::Guarded => Ok(expected.clone()),
                _ => unreachable!(),
            }
        })
        .unwrap();
        assert_eq!(actual.0, expected.0);
        assert_eq!(
            calls,
            [
                crate::client::OpenCreate::NoCreate,
                crate::client::OpenCreate::Guarded,
            ]
        );
    }
}

#[cfg(test)]
mod listing_error_tests {
    use super::remap_listing_error;
    use vfsi_core::internal::{collect_vec_result, errors};
    use vfsi_core::{IndexedError, OpError, StatusCode, TransportKind};

    #[derive(Debug)]
    struct OpaqueSource(u64);
    impl std::fmt::Display for OpaqueSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "opaque {}", self.0)
        }
    }
    impl std::error::Error for OpaqueSource {}

    #[test]
    fn outer_readdir_rejection_retains_no_owner_and_native_status() {
        let error = collect_vec_result::<()>(Err(errors::from_status(StatusCode::Nfs(10067))), 2)
            .unwrap_err();
        let error = remap_listing_error(error, &[4, 9]);
        assert_eq!(error.index(), None);
        assert_eq!(
            vfsi_core::native_status(error.cause()),
            Some(StatusCode::Nfs(10067))
        );
    }

    #[test]
    fn continuation_selection_maps_the_submitted_owner_and_moves_opaque_source() {
        let source = std::io::Error::other(OpaqueSource(73));
        let source_address = source.get_ref().unwrap() as *const dyn std::error::Error;
        let error = collect_vec_result(Ok(vec![Ok(()), Err(OpError::unknown(source, None))]), 2)
            .unwrap_err();
        let error = remap_listing_error(error, &[4, 9]);
        assert_eq!(error.index(), Some(9));
        assert_eq!(
            error
                .cause()
                .get_ref()
                .unwrap()
                .downcast_ref::<OpaqueSource>()
                .unwrap()
                .0,
            73
        );
        assert!(std::ptr::eq(
            source_address,
            error.cause().get_ref().unwrap() as *const dyn std::error::Error
        ));
    }

    #[test]
    fn selected_transport_failure_keeps_known_owner() {
        let error = collect_vec_result(
            Ok(vec![
                Ok(()),
                Err(OpError::unknown(
                    errors::transport(TransportKind::Connection, "lost continuation"),
                    None,
                )),
            ]),
            2,
        )
        .unwrap_err();
        let error = remap_listing_error(error, &[4, 9]);
        assert_eq!(error.index(), Some(9));
        assert_eq!(
            vfsi_core::transport_kind(error.cause()),
            Some(TransportKind::Connection)
        );
    }

    #[test]
    fn malformed_owner_mapping_is_unattributed_invalid_reply() {
        let error = remap_listing_error(
            IndexedError::at(2, std::io::Error::from_raw_os_error(libc::ENOENT)),
            &[4, 9],
        );
        assert_eq!(error.index(), None);
        assert_eq!(
            vfsi_core::transport_kind(error.cause()),
            Some(TransportKind::InvalidReply)
        );
    }
}
