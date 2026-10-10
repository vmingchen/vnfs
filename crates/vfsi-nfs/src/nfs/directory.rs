//! Retained directory cursors and recursive removal state.

use super::*;

/// READDIR continuation retained across unlocked application callbacks.
pub(super) struct NfsDirectoryCursor {
    pub(super) fh: WireFileHandle,
    pub(super) cookie: u64,
    pub(super) buffered: VecDeque<crate::client::DirEntry>,
}

/// A child resolved relative to the observed parent, never by its full path.
pub(super) struct NfsChildDirectoryCursor {
    pub(super) parent: WireFileHandle,
    pub(super) name: Vec<u8>,
}

/// A directory held by filehandle during vectorized recursive removal.
///
/// `fh` addresses the directory itself (for READDIR and child LOOKUPs), while
/// `parent`/`name` address its entry in the parent for the final REMOVE. No
/// path is re-resolved after the operand's parent is resolved once. A `None`
/// parent keeps the directory (used by [`VectorBackend::remove_dir_contents_path_impl`]).
pub(super) struct RemoveDir {
    pub(super) fh: WireFileHandle,
    pub(super) parent: Option<WireFileHandle>,
    pub(super) name: Vec<u8>,
    /// Operand index this directory belongs to, for error attribution.
    pub(super) root: usize,
}

/// A unit of work for the recursive remover.
pub(super) enum RmTask {
    /// REMOVE `name` from `parent`. When `expand` is set and the entry turns
    /// out to be a non-empty directory, resolve it and enter it instead.
    RemoveOrEnter {
        parent: WireFileHandle,
        name: Vec<u8>,
        root: usize,
        expand: bool,
    },
    /// Scan one READDIR page. After a mutating pass reaches EOF, verify from
    /// cookie zero so entries skipped by changing directory cookies are found.
    Enter {
        dir: RemoveDir,
        cookie: u64,
        pass_changed: bool,
    },
    /// Flush child-directory removals before resuming the parent's scan.
    Continue {
        dir: RemoveDir,
        cookie: u64,
        files_removed: bool,
        pass_changed: bool,
    },
    /// The directory's children are gone: batch-remove them from here, then
    /// mark this directory ready to be removed by its own parent.
    Finish(RemoveDir),
}

fn removal_batch_take(remaining: usize, learned: usize, caller_cap: usize) -> usize {
    remaining.min(learned).min(if caller_cap == 0 {
        usize::MAX
    } else {
        caller_cap
    })
}

/// Transient NFS statuses for which a REMOVE may be retried.
fn remove_status_is_retryable(status: u32) -> bool {
    matches!(
        status,
        nfsstat4_NFS4ERR_DELAY | nfsstat4_NFS4ERR_SERVERFAULT
    )
}

/// Exponential backoff for REMOVE retries (10 ms, 20 ms, ...).
fn remove_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(10u64 << attempt.min(6))
}

/// Advance through the current bounded READDIR pass. A mutation may shift
/// later cookies, so a complete pass that changed anything is verified once
/// more from the beginning. Failed entries are not revisited on every page.
pub(super) fn next_rm_scan(cookie: u64, pass_changed: bool) -> Option<(u64, bool)> {
    if cookie != 0 {
        Some((cookie, pass_changed))
    } else if pass_changed {
        Some((0, false))
    } else {
        None
    }
}

/// Keep the first error seen while continuing best-effort removal.
fn record_error(slot: &mut Option<VfError>, error: VfError) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

/// Record a per-entry error, or abort immediately when the caller asked for
/// fail-fast removal.
pub(super) fn note_error(
    slot: &mut Option<VfError>,
    error: VfError,
    options: RemoveOptions,
) -> VfResult<()> {
    if options.continues_on_error() {
        record_error(slot, error);
        Ok(())
    } else {
        Err(error)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn append_bounded_walk_page(
    root: &Path,
    dir: &Path,
    masks: AttrMask,
    ids: &[u32],
    page: &[crate::client::DirEntry],
    options: ListDirOptions,
    entry_count: &mut usize,
    budget: &mut vfsi_core::internal::TraversalBudget,
    out: &mut Vec<VfAttrs>,
) -> VfResult<()> {
    for entry in page {
        let path = dir.join(path_from_bytes(&entry.name));
        budget.charge(&path).map_err(|_| {
            VfError::failure(*entry_count, libc::EFBIG as u32).with_context("walk", dir)
        })?;
        let mut attrs = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks,
            ..VfAttrs::default()
        };
        let values = parse_attr_list(ids, &entry.attrs)
            .map_err(|error| error.with_index(*entry_count).with_context("walk", dir))?;
        apply_attrs(&mut attrs, &values);
        if attrs.ftype == VfType::Directory {
            let depth = path
                .strip_prefix(root)
                .map(|relative| relative.components().count())
                .unwrap_or(usize::MAX);
            if depth > options.depth_limit() && !options.truncates_at_depth_limit() {
                return Err(
                    VfError::failure(*entry_count, libc::EFBIG as u32).with_context("walk", dir)
                );
            }
        }
        out.push(attrs);
        *entry_count += 1;
    }
    Ok(())
}

impl NfsVecFs {
    pub(super) fn listdir_rec(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        out: &mut Vec<VfAttrs>,
    ) -> VfRes {
        let reached_limit = |out: &Vec<VfAttrs>| max_count != 0 && out.len() >= max_count;
        let ids = request_mask_to_attr_list(&masks);
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            if reached_limit(out) {
                break;
            }
            // A directory argument may itself be a symlink to a directory.
            let dirfh = self.resolve_path(&self.server_path(&current), true)?;
            let mut children = Vec::new();
            let mut cookie = 0u64;
            loop {
                let entries = self
                    .nfs
                    .readdir(&dirfh, cookie, &ids)
                    .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
                if entries.is_empty() {
                    break;
                }
                for entry in &entries {
                    if reached_limit(out) {
                        return Ok(());
                    }
                    let path = current.join(path_from_bytes(&entry.name));
                    let mut attrs = VfAttrs {
                        file: VfFile::from_os_path(&path),
                        masks,
                        ..VfAttrs::default()
                    };
                    let values = parse_attr_list(&ids, &entry.attrs)?;
                    apply_attrs(&mut attrs, &values);
                    if recursive && attrs.ftype == VfType::Directory {
                        children.push(path);
                    }
                    out.push(attrs);
                }
                cookie = entries.last().expect("non-empty READDIR page").cookie;
                if cookie == 0 {
                    break;
                }
            }
            for child in children.into_iter().rev() {
                pending.push(child);
            }
        }
        Ok(())
    }

    /// Resolve a path operand to its parent handle and final component name.
    pub(super) fn remove_parent_handle(
        &mut self,
        path: &Path,
    ) -> VfResult<(WireFileHandle, Vec<u8>)> {
        let full = self.server_vf_path(&VfFile::from_os_path(path))?;
        let (dir, name) =
            split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let parent = self.resolve_path(&path_from_bytes(&dir), true)?;
        Ok((parent, name))
    }

    /// Drive recursive removal. Directories are processed depth-first (memory
    /// bounded by the current path) so that, at each directory, the batch of
    /// work under it is vectorized. Empty directories are collected in `ready`
    /// and removed a parent at a time, so sibling REMOVEs are batched too.
    pub(super) fn run_rm_tasks(
        &mut self,
        mut stack: Vec<RmTask>,
        first_error: &mut Option<VfError>,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let mut ready: std::collections::HashMap<WireFileHandle, Vec<(Vec<u8>, usize)>> =
            std::collections::HashMap::new();
        let mut changed_parents = std::collections::HashSet::new();
        while let Some(task) = stack.pop() {
            match task {
                RmTask::RemoveOrEnter {
                    parent,
                    name,
                    root,
                    expand,
                } => self.remove_or_enter(
                    &parent,
                    &name,
                    root,
                    expand,
                    &mut stack,
                    first_error,
                    options,
                )?,
                RmTask::Enter {
                    dir,
                    cookie,
                    pass_changed,
                } => self.enter_directory(
                    dir,
                    cookie,
                    pass_changed,
                    &mut stack,
                    &mut changed_parents,
                    first_error,
                    options,
                )?,
                RmTask::Continue {
                    dir,
                    cookie,
                    files_removed,
                    pass_changed,
                } => {
                    let children_removed = match ready.remove(&dir.fh) {
                        Some(children) => {
                            self.remove_list(&dir.fh, &children, first_error, options)?
                        }
                        None => false,
                    };
                    let child_changed = changed_parents.remove(&dir.fh);
                    let changed =
                        files_removed || children_removed || child_changed || pass_changed;
                    match next_rm_scan(cookie, changed) {
                        Some((cookie, pass_changed)) => stack.push(RmTask::Enter {
                            dir,
                            cookie,
                            pass_changed,
                        }),
                        None => stack.push(RmTask::Finish(dir)),
                    }
                }
                RmTask::Finish(dir) => {
                    if let Some(children) = ready.remove(&dir.fh) {
                        let _ = self.remove_list(&dir.fh, &children, first_error, options)?;
                    }
                    if let Some(parent) = &dir.parent {
                        ready
                            .entry(parent.clone())
                            .or_default()
                            .push((dir.name.clone(), dir.root));
                    }
                }
            }
        }
        // The operands themselves (and any directories still ready) are removed
        // from their parents, batched per parent.
        for (parent, children) in ready {
            self.remove_list(&parent, &children, first_error, options)?;
        }
        Ok(())
    }

    /// REMOVE one entry. A non-empty directory (NFS4ERR_NOTEMPTY) is expanded
    /// when `expand`; everything else is recorded and skipped. Transient
    /// statuses are retried; transport errors abort (the outcome is ambiguous).
    #[allow(clippy::too_many_arguments)]
    fn remove_or_enter(
        &mut self,
        parent: &WireFileHandle,
        name: &[u8],
        root: usize,
        expand: bool,
        stack: &mut Vec<RmTask>,
        first_error: &mut Option<VfError>,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let owned = name.to_vec();
        let mut attempts = 0u32;
        loop {
            match self.nfs.remove_many(parent, std::slice::from_ref(&owned)) {
                Ok(()) => return Ok(()),
                Err(error) if error.is_transport() => {
                    return Err(vfsi_core::error_from_rpc(error, root));
                }
                Err(error) if error.status == nfsstat4_NFS4ERR_NOENT => return Ok(()),
                Err(error) if error.status == nfsstat4_NFS4ERR_NOTEMPTY => {
                    if expand {
                        match self.nfs.lookup(parent, name) {
                            Ok(fh) => stack.push(RmTask::Enter {
                                dir: RemoveDir {
                                    fh,
                                    parent: Some(parent.clone()),
                                    name: owned,
                                    root,
                                },
                                cookie: 0,
                                pass_changed: false,
                            }),
                            Err(lookup) if lookup.is_transport() => {
                                return Err(vfsi_core::error_from_rpc(lookup, root));
                            }
                            Err(lookup) => {
                                note_error(
                                    first_error,
                                    vfsi_core::error_from_rpc(lookup, root),
                                    options,
                                )?;
                            }
                        }
                    } else {
                        note_error(first_error, VfError::failure(root, ERR_ISDIR), options)?;
                    }
                    return Ok(());
                }
                Err(error)
                    if remove_status_is_retryable(error.status)
                        && attempts < options.retry_limit() =>
                {
                    attempts += 1;
                    std::thread::sleep(remove_backoff(attempts));
                }
                Err(error) => {
                    note_error(first_error, vfsi_core::error_from_rpc(error, root), options)?;
                    return Ok(());
                }
            }
        }
    }

    /// Enumerate a directory with `FATTR4_TYPE`, so directories can be
    /// descended into without an extra lookup while files are removed directly.
    fn readdir_typed(
        &mut self,
        fh: &WireFileHandle,
        cookie: u64,
    ) -> Result<Vec<crate::client::DirEntry>, RpcError> {
        const REMOVE_READDIR_MAX_BYTES: usize = 32 * 1024;
        #[cfg(feature = "test-faults")]
        if let Some(injector) = &self.fault_injector {
            injector
                .check(&OpenFaultPoint::BeforeRemovePage { cookie })
                .map_err(|error| {
                    if error.is_transport() {
                        RpcError::transport(error.to_string())
                    } else {
                        RpcError::op(0, error.err_no())
                    }
                })?;
        }
        self.nfs
            .readdir_bounded(fh, cookie, &[FATTR4_TYPE], REMOVE_READDIR_MAX_BYTES)
    }

    /// List a directory, optimistically remove its non-directory children, and
    /// schedule its own removal after its subdirectories.
    #[allow(clippy::too_many_arguments)]
    fn enter_directory(
        &mut self,
        dir: RemoveDir,
        cookie: u64,
        pass_changed: bool,
        stack: &mut Vec<RmTask>,
        changed_parents: &mut std::collections::HashSet<WireFileHandle>,
        first_error: &mut Option<VfError>,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let entries = match self.readdir_typed(&dir.fh, cookie) {
            Ok(entries) => entries,
            Err(error)
                if cookie != 0
                    && pass_changed
                    && error.status == nfsv41_sys::nfsstat4_NFS4ERR_BAD_COOKIE =>
            {
                // A server may invalidate a READDIR continuation when this
                // pass removed entries. Start a fresh pass rather than
                // silently dropping the remaining directory contents.
                stack.push(RmTask::Enter {
                    dir,
                    cookie: 0,
                    pass_changed: false,
                });
                return Ok(());
            }
            Err(error) if error.status == nfsstat4_NFS4ERR_NOTDIR => {
                // Race: it is no longer a directory. Remove the entry instead.
                if let Some(parent) = &dir.parent {
                    let removed = self.remove_list(
                        parent,
                        &[(dir.name.clone(), dir.root)],
                        first_error,
                        options,
                    )?;
                    if removed {
                        changed_parents.insert(parent.clone());
                    }
                }
                return Ok(());
            }
            Err(error) => return Err(vfsi_core::error_from_rpc(error, dir.root)),
        };

        if entries.is_empty() {
            match next_rm_scan(0, pass_changed) {
                Some((cookie, pass_changed)) => stack.push(RmTask::Enter {
                    dir,
                    cookie,
                    pass_changed,
                }),
                None => stack.push(RmTask::Finish(dir)),
            }
            return Ok(());
        }
        let next_cookie = entries.last().map(|entry| entry.cookie).unwrap_or(0);
        if next_cookie != 0 && next_cookie == cookie {
            return Err(VfError::transport(None, "READDIR cookie made no progress"));
        }

        let mut file_names = Vec::new();
        let mut dir_names = Vec::new();
        for entry in entries {
            let values = parse_attr_list(&[FATTR4_TYPE], &entry.attrs)
                .map_err(|error| error.with_index(dir.root))?;
            if values.ftype == Some(nfs_ftype4_NF4DIR) {
                dir_names.push(entry.name);
            } else {
                file_names.push(entry.name);
            }
        }
        // Files (and symlinks, and empty directories) go in one batched pass.
        // A name that turned out to be a non-empty directory is promoted.
        let (promoted, files_removed) =
            self.remove_names(&dir.fh, &file_names, dir.root, first_error, options)?;
        dir_names.extend(promoted);

        let children = self.lookup_children(&dir.fh, &dir_names, dir.root, first_error, options)?;
        let parent = dir.fh.clone();
        let root = dir.root;
        stack.push(RmTask::Continue {
            dir,
            cookie: next_cookie,
            files_removed,
            pass_changed,
        });
        for (fh, name) in children {
            stack.push(RmTask::Enter {
                dir: RemoveDir {
                    fh,
                    parent: Some(parent.clone()),
                    name,
                    root,
                },
                cookie: 0,
                pass_changed: false,
            });
        }
        Ok(())
    }

    /// Optimistically REMOVE every name in `names`, returning the names that
    /// turned out to be non-empty directories (NFS4ERR_NOTEMPTY). Other
    /// per-entry failures are recorded and skipped; transport errors abort.
    fn remove_names(
        &mut self,
        dir: &WireFileHandle,
        names: &[Vec<u8>],
        root: usize,
        first_error: &mut Option<VfError>,
        options: RemoveOptions,
    ) -> VfResult<(Vec<Vec<u8>>, bool)> {
        let mut subdirs = Vec::new();
        let mut removed = false;
        let mut start = 0;
        while start < names.len() {
            let take = removal_batch_take(
                names.len() - start,
                self.nfs.remove_batch_capacity(),
                options.batch_size(),
            );
            match self.nfs.remove_many(dir, &names[start..start + take]) {
                Ok(()) => {
                    removed = true;
                    start += take;
                }
                Err(error) if error.is_transport() => {
                    return Err(vfsi_core::error_from_rpc(error, root));
                }
                Err(error) if resource_status(error.status) && take > 1 => {
                    // REMOVE is ordered: only retry the suffix after the
                    // server-confirmed successful prefix.
                    let prefix = error.op_index.min(take.saturating_sub(1));
                    removed |= prefix > 0;
                    start += prefix;
                }
                Err(error) => {
                    let status = error.status;
                    let index = error.op_index.min(take.saturating_sub(1));
                    removed |= index > 0;
                    let name = names[start + index].clone();
                    if status == nfsstat4_NFS4ERR_NOTEMPTY {
                        subdirs.push(name);
                    } else if status == nfsstat4_NFS4ERR_NOENT {
                        // Already gone.
                        removed = true;
                    } else if remove_status_is_retryable(status) {
                        removed |=
                            self.retry_remove_name(dir, &name, root, first_error, options, error)?;
                    } else {
                        note_error(first_error, vfsi_core::error_from_rpc(error, root), options)?;
                    }
                    start += index + 1;
                }
            }
        }
        Ok((subdirs, removed))
    }

    /// REMOVE a batch of `(name, root)` entries from one parent, used once the
    /// directories are known to be empty. Errors are recorded and skipped.
    fn remove_list(
        &mut self,
        parent: &WireFileHandle,
        entries: &[(Vec<u8>, usize)],
        first_error: &mut Option<VfError>,
        options: RemoveOptions,
    ) -> VfResult<bool> {
        let mut removed = false;
        let mut start = 0;
        while start < entries.len() {
            let take = removal_batch_take(
                entries.len() - start,
                self.nfs.remove_batch_capacity(),
                options.batch_size(),
            );
            let chunk = &entries[start..start + take];
            let names: Vec<Vec<u8>> = chunk.iter().map(|(name, _)| name.clone()).collect();
            match self.nfs.remove_many(parent, &names) {
                Ok(()) => {
                    removed = true;
                    start += take;
                }
                Err(error) if error.is_transport() => {
                    return Err(vfsi_core::error_from_rpc(error, chunk[0].1));
                }
                Err(error) if resource_status(error.status) && take > 1 => {
                    let prefix = error.op_index.min(take.saturating_sub(1));
                    removed |= prefix > 0;
                    start += prefix;
                }
                Err(error) => {
                    let status = error.status;
                    let index = error.op_index.min(take.saturating_sub(1));
                    removed |= index > 0;
                    let (name, root) = chunk[index].clone();
                    if status == nfsstat4_NFS4ERR_NOENT {
                        // Already gone.
                        removed = true;
                    } else if remove_status_is_retryable(status) {
                        removed |= self.retry_remove_name(
                            parent,
                            &name,
                            root,
                            first_error,
                            options,
                            error,
                        )?;
                    } else {
                        // A non-empty directory here means a concurrent actor
                        // added entries; record it and move on.
                        note_error(first_error, vfsi_core::error_from_rpc(error, root), options)?;
                    }
                    start += index + 1;
                }
            }
        }
        Ok(removed)
    }

    /// Resolve child directory names to handles, batched. Missing children
    /// (removed concurrently) are skipped; transport errors abort.
    fn lookup_children(
        &mut self,
        dir: &WireFileHandle,
        names: &[Vec<u8>],
        root: usize,
        first_error: &mut Option<VfError>,
        options: RemoveOptions,
    ) -> VfResult<Vec<(WireFileHandle, Vec<u8>)>> {
        let mut children = Vec::new();
        let mut start = 0;
        while start < names.len() {
            let take = removal_batch_take(
                names.len() - start,
                self.nfs.lookup_batch_capacity(),
                options.batch_size(),
            );
            let chunk = &names[start..start + take];
            let ops: Vec<(WireFileHandle, Vec<u8>)> = chunk
                .iter()
                .map(|name| (dir.clone(), name.clone()))
                .collect();
            match self.nfs.lookup_many(&ops) {
                Ok(results) => {
                    if take > 1
                        && results
                            .iter()
                            .any(|result| matches!(result, Err(status) if resource_status(*status)))
                    {
                        // LOOKUP is read-only, so a rejected compound can be
                        // retried in smaller pieces without replay concerns.
                        continue;
                    }
                    for (name, result) in chunk.iter().cloned().zip(results) {
                        match result {
                            Ok(fh) => children.push((fh, name)),
                            Err(nfsstat4_NFS4ERR_NOENT) => {}
                            Err(status) => {
                                note_error(first_error, VfError::nfs(root, status), options)?;
                            }
                        }
                    }
                    start += take;
                }
                Err(error) if error.is_transport() => {
                    return Err(vfsi_core::error_from_rpc(error, root));
                }
                Err(error) if resource_status(error.status) && take > 1 => {}
                Err(error) => {
                    note_error(first_error, vfsi_core::error_from_rpc(error, root), options)?;
                    start += take;
                }
            }
        }
        Ok(children)
    }

    /// Retry one REMOVE with bounded backoff; record the failure if it
    /// persists. Transport errors abort because the outcome is ambiguous.
    fn retry_remove_name(
        &mut self,
        dir: &WireFileHandle,
        name: &[u8],
        root: usize,
        first_error: &mut Option<VfError>,
        options: RemoveOptions,
        initial_error: RpcError,
    ) -> VfResult<bool> {
        let owned = name.to_vec();
        // The caller already made the initial attempt. `retries` counts
        // additional attempts, not an unconditional extra call plus retries.
        let mut last_error = initial_error;
        for attempt in 0..options.retry_limit() {
            std::thread::sleep(remove_backoff(attempt.saturating_add(1)));
            match self.nfs.remove_many(dir, std::slice::from_ref(&owned)) {
                Ok(()) => return Ok(true),
                Err(error) if error.is_transport() => {
                    return Err(vfsi_core::error_from_rpc(error, root));
                }
                Err(error) if error.status == nfsstat4_NFS4ERR_NOENT => return Ok(true),
                Err(error) if remove_status_is_retryable(error.status) => last_error = error,
                Err(error) => {
                    note_error(first_error, vfsi_core::error_from_rpc(error, root), options)?;
                    return Ok(false);
                }
            }
        }
        note_error(
            first_error,
            vfsi_core::error_from_rpc(last_error, root),
            options,
        )?;
        Ok(false)
    }
}
