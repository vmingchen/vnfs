//! Retained directory cursors and recursive removal state.

use super::*;
use std::collections::HashSet;

/// Reject an NFS server response that repeats any continuation cookie, which
/// would otherwise let a multi-page cycle make READDIR loop forever. Cookie
/// zero remains the normal end-of-directory marker.
pub(super) fn validate_readdir_progress(
    requested_cookie: u64,
    next_cookie: u64,
    seen_cookies: &mut HashSet<u64>,
    index: Option<usize>,
) -> VfRes {
    if next_cookie != 0 && (next_cookie == requested_cookie || !seen_cookies.insert(next_cookie)) {
        Err(VfError::transport(
            index,
            "READDIR continuation cookie repeated",
        ))
    } else {
        Ok(())
    }
}

/// READDIR continuation retained across unlocked application callbacks.
pub(super) struct NfsDirectoryCursor {
    pub(super) fh: NfsFh,
    pub(super) cookie: DirCookie,
    pub(super) seen_cookies: HashSet<u64>,
    pub(super) buffered: VecDeque<crate::client::DirEntry>,
}

/// A child resolved relative to the observed parent, never by its full path.
pub(super) struct NfsChildDirectoryCursor {
    pub(super) parent: NfsFh,
    pub(super) name: Box<[u8]>,
}

/// A directory held by filehandle during vectorized recursive removal.
///
/// `fh` addresses the directory itself (for READDIR and child LOOKUPs), while
/// `parent`/`name` address its entry in the parent for the final REMOVE. No
/// path is re-resolved after the operand's parent is resolved once. A `None`
/// parent keeps the directory (used by [`VectorBackend::remove_dir_contents_path_impl`]).
pub(super) struct RemoveDir {
    pub(super) fh: NfsFh,
    pub(super) parent: Option<NfsFh>,
    pub(super) name: Box<[u8]>,
    /// Operand index this directory belongs to, for error attribution.
    pub(super) root: usize,
    /// This entry completes the caller's root rather than an internal child.
    pub(super) is_root: bool,
}

/// A unit of work for the recursive remover.
pub(super) enum RmTask {
    /// REMOVE `name` from `parent`. When `expand` is set and the entry turns
    /// out to be a non-empty directory, resolve it and enter it instead.
    RemoveOrEnter {
        parent: NfsFh,
        name: Box<[u8]>,
        root: usize,
        expand: bool,
    },
    /// Scan one READDIR page. After a mutating pass reaches EOF, verify from
    /// cookie zero so entries skipped by changing directory cookies are found.
    Enter {
        dir: RemoveDir,
        cookie: DirCookie,
        seen_cookies: HashSet<u64>,
        pass_changed: bool,
    },
    /// Flush child-directory removals before resuming the parent's scan.
    Continue {
        dir: RemoveDir,
        cookie: DirCookie,
        seen_cookies: HashSet<u64>,
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

fn one_remove_outcome(results: vfsi_core::VecResult<()>) -> vfsi_core::OpResult<()> {
    use vfsi_core::OpError;
    match results {
        Ok(mut results) if results.len() == 1 => results.pop().unwrap(),
        Ok(_) => Err(OpError::unknown(
            VfError::invalid_reply(None, "invalid REMOVE result count"),
            None,
        )),
        Err(cause) => Err(OpError::not_attempted(cause)),
    }
}

/// Advance through the current bounded READDIR pass. A mutation may shift
/// later cookies, so a complete pass that changed anything is verified once
/// more from the beginning. Failed entries are not revisited on every page.
pub(super) fn next_rm_scan(cookie: DirCookie, pass_changed: bool) -> Option<(DirCookie, bool)> {
    if cookie.value != 0 {
        Some((cookie, pass_changed))
    } else if pass_changed {
        Some((DirCookie::default(), false))
    } else {
        None
    }
}

/// Private traversal state; public outcomes remain exactly OpResult<()>.
pub(super) enum RemoveRoot {
    Pending,
    Active,
    Complete,
    Failed(vfsi_core::OpError),
}

impl RemoveRoot {
    fn touch(&mut self) {
        match self {
            Self::Pending => *self = Self::Active,
            Self::Failed(error) if error.state() == vfsi_core::FailureState::NotAttempted => {
                *error = vfsi_core::OpError::failed(error.cause().clone(), None);
            }
            _ => {}
        }
    }

    fn complete(&mut self) {
        self.touch();
        if !matches!(self, Self::Failed(_)) {
            *self = Self::Complete;
        }
    }

    fn stopped(&self, options: RemoveOptions) -> bool {
        !options.continue_on_error() && matches!(self, Self::Failed(_))
    }

    fn fatal_cause(&self) -> Option<&VfError> {
        match self {
            Self::Failed(error)
                if error.state() == vfsi_core::FailureState::Unknown
                    || error.cause().is_transport() =>
            {
                Some(error.cause())
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoveStop {
    Root,
    Fatal,
}

type RemoveRun<T> = std::result::Result<T, RemoveStop>;

fn note_error(
    roots: &mut [RemoveRoot],
    root: usize,
    mut error: vfsi_core::OpError,
    options: RemoveOptions,
) -> RemoveRun<()> {
    use vfsi_core::{FailureState, OpError};
    let fatal = error.state() == FailureState::Unknown || error.cause().is_transport();
    if error.state() == FailureState::NotAttempted && !matches!(roots[root], RemoveRoot::Pending) {
        error = OpError::failed(error.into_cause(), None);
    }
    let replace = match &roots[root] {
        RemoveRoot::Failed(previous) => {
            previous.state() != FailureState::Unknown
                && (error.state() == FailureState::Unknown
                    || (fatal && !previous.cause().is_transport()))
        }
        _ => true,
    };
    if replace {
        roots[root] = RemoveRoot::Failed(error);
    }
    if fatal {
        Err(RemoveStop::Fatal)
    } else if options.continue_on_error() {
        Ok(())
    } else {
        Err(RemoveStop::Root)
    }
}

fn note_read_error(
    roots: &mut [RemoveRoot],
    root: usize,
    cause: VfError,
    options: RemoveOptions,
) -> RemoveRun<()> {
    note_error(
        roots,
        root,
        vfsi_core::OpError::failed(cause, None),
        options,
    )
}

impl RmTask {
    fn root(&self) -> usize {
        match self {
            Self::RemoveOrEnter { root, .. } => *root,
            Self::Enter { dir, .. } | Self::Continue { dir, .. } | Self::Finish(dir) => dir.root,
        }
    }
}

fn finish_remove_roots(roots: Vec<RemoveRoot>, fatal: bool) -> Vec<vfsi_core::OpResult<()>> {
    use vfsi_core::OpError;
    let cause = roots.iter().find_map(RemoveRoot::fatal_cause).cloned();
    roots
        .into_iter()
        .map(|root| match root {
            RemoveRoot::Complete => Ok(()),
            RemoveRoot::Failed(error) => Err(error),
            RemoveRoot::Pending if fatal => Err(OpError::not_attempted(
                cause.clone().expect("fatal removal has a recorded cause"),
            )),
            RemoveRoot::Active if fatal => Err(OpError::failed(
                cause.clone().expect("fatal removal has a recorded cause"),
                None,
            )),
            RemoveRoot::Pending | RemoveRoot::Active => Err(OpError::unknown(
                VfError::invalid_reply(None, "recursive removal did not finish its root"),
                None,
            )),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn append_bounded_walk_page(
    root: &Path,
    dir: &Path,
    masks: AttrMask,
    page: &[crate::client::DirEntry],
    options: ListDirOptions,
    entry_count: &mut usize,
    budget: &mut vfsi_core::internal::TraversalBudget,
    out: &mut Vec<VfAttrs>,
) -> VfResult<()> {
    for entry in page {
        let path = dir.join(path_from_bytes(&entry.name));
        budget.charge(&path).map_err(|_| {
            VfError::errno(*entry_count, libc::EFBIG as u32).with_context("walk", dir)
        })?;
        let mut attrs = VfAttrs {
            file: FileOperand::from_os_path(&path),
            masks,
            ..VfAttrs::default()
        };
        let values = parse_metadata_attrs(&entry.attrmask, &entry.attrs)
            .map_err(|error| error.with_index(*entry_count).with_context("walk", dir))?;
        apply_attrs(&mut attrs, &values);
        if attrs.ftype == VfType::Directory {
            let depth = path
                .strip_prefix(root)
                .map(|relative| relative.components().count())
                .unwrap_or(usize::MAX);
            if depth > options.max_depth_or(Default::default()).get()
                && !options.truncate_at_max_depth()
            {
                return Err(
                    VfError::errno(*entry_count, libc::EFBIG as u32).with_context("walk", dir)
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
            let mut cookie = DirCookie::default();
            let mut seen_cookies = HashSet::new();
            loop {
                let (entries, next_cookie) = self
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
                        file: FileOperand::from_os_path(&path),
                        masks,
                        ..VfAttrs::default()
                    };
                    let values = parse_metadata_attrs(&entry.attrmask, &entry.attrs)?;
                    apply_attrs(&mut attrs, &values);
                    if recursive && attrs.ftype == VfType::Directory {
                        children.push(path);
                    }
                    out.push(attrs);
                }
                validate_readdir_progress(
                    cookie.value,
                    next_cookie.value,
                    &mut seen_cookies,
                    None,
                )?;
                cookie = next_cookie;
                if cookie.value == 0 {
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
    pub(super) fn remove_parent_handle(&mut self, path: &Path) -> VfResult<(NfsFh, Vec<u8>)> {
        let full = self.server_vf_path(&FileOperand::from_os_path(path))?;
        let (dir, name) =
            split_path_bytes(path_bytes(&full)).map_err(|_| VfError::errno(0, ERR_NOENT))?;
        let (parent, kind) = self.resolve_path_and_type(&path_from_bytes(dir), true)?;
        if kind != nfs_ftype4_NF4DIR {
            return Err(VfError::nfs(None, nfsstat4_NFS4ERR_NOTDIR));
        }
        Ok((parent, name.to_vec()))
    }

    /// Drive recursive removal. Directories are processed depth-first (memory
    /// bounded by the current path) so that, at each directory, the batch of
    /// work under it is vectorized. Empty directories are collected in `ready`
    /// and removed a parent at a time, so sibling REMOVEs are batched too.
    pub(super) fn run_rm_tasks(
        &mut self,
        mut stack: Vec<RmTask>,
        mut roots: Vec<RemoveRoot>,
        options: RemoveOptions,
    ) -> Vec<vfsi_core::OpResult<()>> {
        let mut ready: std::collections::HashMap<NfsFh, Vec<_>> = std::collections::HashMap::new();
        let mut changed_parents = std::collections::HashSet::new();
        let mut fatal = roots.iter().any(|root| root.fatal_cause().is_some());
        while !fatal {
            let Some(task) = stack.pop() else {
                break;
            };
            if roots[task.root()].stopped(options) {
                continue;
            }
            let result = (|| -> RemoveRun<()> {
                match task {
                    RmTask::RemoveOrEnter {
                        parent,
                        name,
                        root,
                        expand,
                    } => {
                        self.remove_or_enter(
                            &parent, &name, root, expand, &mut stack, &mut roots, options,
                        )?;
                    }
                    RmTask::Enter {
                        dir,
                        cookie,
                        seen_cookies,
                        pass_changed,
                    } => {
                        self.enter_directory(
                            dir,
                            cookie,
                            seen_cookies,
                            pass_changed,
                            &mut stack,
                            &mut changed_parents,
                            &mut roots,
                            options,
                        )?;
                    }
                    RmTask::Continue {
                        dir,
                        cookie,
                        seen_cookies,
                        files_removed,
                        pass_changed,
                    } => {
                        let children_removed = match ready.remove(&dir.fh) {
                            Some(children) => {
                                self.remove_list(&dir.fh, &children, &mut roots, options)?
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
                                seen_cookies: if cookie.value == 0 && !pass_changed {
                                    HashSet::new()
                                } else {
                                    seen_cookies
                                },
                                pass_changed,
                            }),
                            None => stack.push(RmTask::Finish(dir)),
                        }
                    }
                    RmTask::Finish(dir) => {
                        if let Some(children) = ready.remove(&dir.fh) {
                            self.remove_list(&dir.fh, &children, &mut roots, options)?;
                        }
                        if roots[dir.root].stopped(options) {
                            return Ok(());
                        }
                        if let Some(parent) = dir.parent {
                            ready.entry(parent).or_default().push((
                                dir.name,
                                dir.root,
                                dir.is_root,
                            ));
                        } else if dir.is_root {
                            roots[dir.root].complete();
                        }
                    }
                }
                Ok(())
            })();
            fatal = matches!(result, Err(RemoveStop::Fatal));
        }
        if !fatal {
            for (parent, children) in ready {
                if matches!(
                    self.remove_list(&parent, &children, &mut roots, options),
                    Err(RemoveStop::Fatal)
                ) {
                    fatal = true;
                    break;
                }
            }
        }
        finish_remove_roots(roots, fatal)
    }

    /// REMOVE one entry. A non-empty directory (NFS4ERR_NOTEMPTY) is expanded
    /// when `expand`; everything else is recorded and skipped. Transient
    /// statuses are retried; transport errors abort (the outcome is ambiguous).
    #[allow(clippy::too_many_arguments)]
    fn remove_or_enter(
        &mut self,
        parent: &NfsFh,
        name: &[u8],
        root: usize,
        expand: bool,
        stack: &mut Vec<RmTask>,
        roots: &mut [RemoveRoot],
        options: RemoveOptions,
    ) -> RemoveRun<()> {
        let owned = name.to_vec();
        let mut attempts = 0u32;
        loop {
            let error = match one_remove_outcome(
                self.nfs.remove_many(parent, std::slice::from_ref(&owned)),
            ) {
                Ok(()) => {
                    roots[root].complete();
                    return Ok(());
                }
                Err(error) => error,
            };
            if error.state() == vfsi_core::FailureState::Unknown || error.cause().is_transport() {
                return note_error(roots, root, error, options);
            }
            let status = error.cause().status();
            match status {
                Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOTEMPTY)) if expand => {
                    roots[root].touch();
                    match self.nfs.lookup(parent, name) {
                        Ok(fh) => stack.push(RmTask::Enter {
                            dir: RemoveDir {
                                fh,
                                parent: Some(parent.clone()),
                                name: owned.into_boxed_slice(),
                                root,
                                is_root: true,
                            },
                            cookie: DirCookie::default(),
                            seen_cookies: HashSet::new(),
                            pass_changed: false,
                        }),
                        Err(error) => {
                            return note_read_error(
                                roots,
                                root,
                                vfsi_core::error_from_rpc(error, None),
                                options,
                            );
                        }
                    }
                    return Ok(());
                }
                Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOTEMPTY)) => {
                    return note_error(roots, root, error, options);
                }
                Some(vfsi_core::StatusCode::Nfs(status))
                    if remove_status_is_retryable(status) && attempts < options.retries() =>
                {
                    roots[root].touch();
                    attempts += 1;
                    std::thread::sleep(remove_backoff(attempts));
                }
                _ => return note_error(roots, root, error, options),
            }
        }
    }

    /// Enumerate a directory with `FATTR4_TYPE`, so directories can be
    /// descended into without an extra lookup while files are removed directly.
    fn readdir_typed(
        &mut self,
        fh: &NfsFh,
        cookie: DirCookie,
    ) -> VfResult<crate::client::ReaddirPage> {
        const REMOVE_READDIR_MAX_BYTES: usize = 32 * 1024;
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeRemovePage {
            cookie: cookie.value,
        })?;
        self.nfs
            .readdir_bounded(fh, cookie, &[FATTR4_TYPE], REMOVE_READDIR_MAX_BYTES)
            .map_err(|error| vfsi_core::error_from_rpc(error, None))
    }

    /// List a directory, optimistically remove its non-directory children, and
    /// schedule its own removal after its subdirectories.
    #[allow(clippy::too_many_arguments)]
    fn enter_directory(
        &mut self,
        dir: RemoveDir,
        cookie: DirCookie,
        mut seen_cookies: HashSet<u64>,
        pass_changed: bool,
        stack: &mut Vec<RmTask>,
        changed_parents: &mut std::collections::HashSet<NfsFh>,
        roots: &mut [RemoveRoot],
        options: RemoveOptions,
    ) -> RemoveRun<()> {
        roots[dir.root].touch();
        let (entries, next_cookie) = match self.readdir_typed(&dir.fh, cookie) {
            Ok(entries) => entries,
            Err(error)
                if cookie.value != 0
                    && pass_changed
                    && matches!(
                        error.status(),
                        Some(vfsi_core::StatusCode::Nfs(
                            nfsv41_sys::nfsstat4_NFS4ERR_BAD_COOKIE
                                | nfsv41_sys::nfsstat4_NFS4ERR_NOT_SAME
                        ))
                    ) =>
            {
                // A server may invalidate a READDIR continuation when this
                // pass removed entries. Start a fresh pass rather than
                // silently dropping the remaining directory contents.
                stack.push(RmTask::Enter {
                    dir,
                    cookie: DirCookie::default(),
                    seen_cookies: HashSet::new(),
                    pass_changed: false,
                });
                return Ok(());
            }
            Err(error)
                if error.status() == Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOTDIR)) =>
            {
                // Race: it is no longer a directory. Remove the entry instead.
                if let Some(parent) = &dir.parent {
                    let removed = self.remove_list(
                        parent,
                        &[(dir.name, dir.root, dir.is_root)],
                        roots,
                        options,
                    )?;
                    if removed {
                        changed_parents.insert(parent.clone());
                    }
                } else {
                    return note_read_error(roots, dir.root, error, options);
                }
                return Ok(());
            }
            Err(error) => return note_read_error(roots, dir.root, error, options),
        };

        if entries.is_empty() {
            match next_rm_scan(DirCookie::default(), pass_changed) {
                Some((cookie, pass_changed)) => stack.push(RmTask::Enter {
                    dir,
                    cookie,
                    seen_cookies: HashSet::new(),
                    pass_changed,
                }),
                None => stack.push(RmTask::Finish(dir)),
            }
            return Ok(());
        }
        validate_readdir_progress(cookie.value, next_cookie.value, &mut seen_cookies, None)
            .map_err(|cause| {
                note_read_error(roots, dir.root, cause, options)
                    .err()
                    .unwrap_or(RemoveStop::Root)
            })?;

        let mut file_names = Vec::new();
        let mut dir_names = Vec::new();
        for entry in entries {
            let values = parse_metadata_attrs(&entry.attrmask, &entry.attrs).map_err(|cause| {
                note_read_error(roots, dir.root, cause, options)
                    .err()
                    .unwrap_or(RemoveStop::Root)
            })?;
            if values.ftype == Some(nfs_ftype4_NF4DIR) {
                dir_names.push(entry.name);
            } else {
                file_names.push(entry.name);
            }
        }
        // Files (and symlinks, and empty directories) go in one batched pass.
        // A name that turned out to be a non-empty directory is promoted.
        let (promoted, files_removed) =
            self.remove_names(&dir.fh, &file_names, dir.root, roots, options)?;
        dir_names.extend(promoted);

        let children = self.lookup_children(&dir.fh, &dir_names, dir.root, roots, options)?;
        let parent = dir.fh.clone();
        let root = dir.root;
        stack.push(RmTask::Continue {
            dir,
            cookie: next_cookie,
            seen_cookies,
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
                    is_root: false,
                },
                cookie: DirCookie::default(),
                seen_cookies: HashSet::new(),
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
        dir: &NfsFh,
        names: &[Vec<u8>],
        root: usize,
        roots: &mut [RemoveRoot],
        options: RemoveOptions,
    ) -> RemoveRun<(Vec<Vec<u8>>, bool)> {
        let mut subdirs = Vec::new();
        let mut removed = false;
        let mut start = 0;
        while start < names.len() {
            let take = removal_batch_take(
                names.len() - start,
                self.nfs.remove_batch_capacity(),
                options.batch_size().map_or(0, std::num::NonZeroUsize::get),
            );
            let chunk = &names[start..start + take];
            let outcomes = self.nfs.remove_many(dir, chunk).unwrap_or_else(|cause| {
                chunk
                    .iter()
                    .map(|_| Err(vfsi_core::OpError::not_attempted(cause.clone())))
                    .collect()
            });
            if outcomes.len() != chunk.len() {
                note_read_error(
                    roots,
                    root,
                    VfError::invalid_reply(None, "invalid recursive REMOVE count"),
                    options,
                )?;
                return Err(RemoveStop::Root);
            }
            let mut stop = None;
            for (name, result) in chunk.iter().zip(outcomes) {
                let error = match result {
                    Ok(()) => {
                        roots[root].touch();
                        removed = true;
                        continue;
                    }
                    Err(error) => error,
                };
                let next = if error.state() == vfsi_core::FailureState::Unknown
                    || error.cause().is_transport()
                {
                    note_error(roots, root, error, options)
                } else {
                    match error.cause().status() {
                        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOTEMPTY)) => {
                            roots[root].touch();
                            subdirs.push(name.clone());
                            Ok(())
                        }
                        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOENT)) => {
                            roots[root].touch();
                            removed = true;
                            Ok(())
                        }
                        Some(vfsi_core::StatusCode::Nfs(status))
                            if remove_status_is_retryable(status) =>
                        {
                            self.retry_remove_name(
                                dir,
                                name,
                                root,
                                roots,
                                options,
                                error.into_cause(),
                            )
                            .map(|changed| {
                                removed |= changed;
                            })
                        }
                        _ => note_error(roots, root, error, options),
                    }
                };
                if let Err(next) = next
                    && (next == RemoveStop::Fatal || stop.is_none())
                {
                    stop = Some(next);
                }
            }
            if let Some(stop) = stop {
                return Err(stop);
            }
            start += take;
        }
        Ok((subdirs, removed))
    }

    /// REMOVE a batch of `(name, root)` entries from one parent, used once the
    /// directories are known to be empty. Errors are recorded and skipped.
    fn remove_list(
        &mut self,
        parent: &NfsFh,
        entries: &[(Box<[u8]>, usize, bool)],
        roots: &mut [RemoveRoot],
        options: RemoveOptions,
    ) -> RemoveRun<bool> {
        let mut removed = false;
        let mut start = 0;
        while start < entries.len() {
            let take = removal_batch_take(
                entries.len() - start,
                self.nfs.remove_batch_capacity(),
                options.batch_size().map_or(0, std::num::NonZeroUsize::get),
            );
            let chunk = &entries[start..start + take];
            // No cancellation allocation on a healthy batch. A snapshot only
            // on the error path lets later outcomes update roots independently.
            let stopped = chunk
                .iter()
                .any(|(_, root, _)| roots[*root].stopped(options))
                .then(|| {
                    chunk
                        .iter()
                        .map(|(_, root, _)| roots[*root].stopped(options))
                        .collect::<Vec<_>>()
                });
            let active = chunk
                .iter()
                .enumerate()
                .filter(|(index, _)| stopped.as_ref().is_none_or(|stopped| !stopped[*index]));
            let names: Vec<_> = active
                .clone()
                .map(|(_, (name, _, _))| name.to_vec())
                .collect();
            if names.is_empty() {
                start += take;
                continue;
            }
            let outcomes = self
                .nfs
                .remove_many(parent, &names)
                .unwrap_or_else(|cause| {
                    names
                        .iter()
                        .map(|_| Err(vfsi_core::OpError::not_attempted(cause.clone())))
                        .collect()
                });
            if outcomes.len() != names.len() {
                for (_, (_, root, _)) in active {
                    let _ = note_read_error(
                        roots,
                        *root,
                        VfError::invalid_reply(None, "invalid recursive REMOVE count"),
                        options,
                    );
                }
                return Err(RemoveStop::Fatal);
            }
            let mut fatal = false;
            for ((_, (name, root, is_root)), result) in active.zip(outcomes) {
                let error = match result {
                    Ok(()) => {
                        if *is_root {
                            roots[*root].complete();
                        } else {
                            roots[*root].touch();
                        }
                        removed = true;
                        continue;
                    }
                    Err(error) => error,
                };
                let next = if error.state() == vfsi_core::FailureState::Unknown
                    || error.cause().is_transport()
                {
                    note_error(roots, *root, error, options)
                } else {
                    match error.cause().status() {
                        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOENT)) => {
                            if *is_root {
                                roots[*root].complete();
                            } else {
                                roots[*root].touch();
                            }
                            removed = true;
                            Ok(())
                        }
                        Some(vfsi_core::StatusCode::Nfs(status))
                            if remove_status_is_retryable(status) =>
                        {
                            self.retry_remove_name(
                                parent,
                                name,
                                *root,
                                roots,
                                options,
                                error.into_cause(),
                            )
                            .map(|changed| {
                                if changed && *is_root {
                                    roots[*root].complete();
                                }
                                removed |= changed;
                            })
                        }
                        _ => note_error(roots, *root, error, options),
                    }
                };
                fatal |= matches!(next, Err(RemoveStop::Fatal));
            }
            if fatal {
                return Err(RemoveStop::Fatal);
            }
            start += take;
        }
        Ok(removed)
    }

    /// Resolve child directory names to handles, batched. Missing children
    /// (removed concurrently) are skipped; transport errors abort.
    fn lookup_children(
        &mut self,
        dir: &NfsFh,
        names: &[Vec<u8>],
        root: usize,
        roots: &mut [RemoveRoot],
        options: RemoveOptions,
    ) -> RemoveRun<Vec<(NfsFh, Box<[u8]>)>> {
        let mut children = Vec::new();
        let mut start = 0;
        while start < names.len() {
            let take = removal_batch_take(
                names.len() - start,
                self.nfs.lookup_batch_capacity(),
                options.batch_size().map_or(0, std::num::NonZeroUsize::get),
            );
            let chunk = &names[start..start + take];
            let ops: Vec<_> = chunk
                .iter()
                .map(|name| (dir.clone(), name.clone()))
                .collect();
            match self.nfs.lookup_many(&ops) {
                Ok(results) => {
                    if results.len() != chunk.len() {
                        note_read_error(
                            roots,
                            root,
                            VfError::invalid_reply(None, "invalid recursive LOOKUP count"),
                            options,
                        )?;
                        return Err(RemoveStop::Root);
                    }
                    if take > 1
                        && results
                            .iter()
                            .any(|result| matches!(result,Err(status) if resource_status(*status)))
                    {
                        continue;
                    }
                    for (name, result) in chunk.iter().zip(results) {
                        match result {
                            Ok(fh) => children.push((fh, Box::from(name.as_slice()))),
                            Err(nfsstat4_NFS4ERR_NOENT) => {}
                            Err(status) => {
                                note_read_error(roots, root, VfError::nfs(None, status), options)?
                            }
                        }
                    }
                    start += take;
                }
                Err(error)
                    if !error.is_transport() && resource_status(error.status) && take > 1 => {}
                Err(error) => {
                    note_read_error(roots, root, vfsi_core::error_from_rpc(error, None), options)?;
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
        dir: &NfsFh,
        name: &[u8],
        root: usize,
        roots: &mut [RemoveRoot],
        options: RemoveOptions,
        initial_error: VfError,
    ) -> RemoveRun<bool> {
        let owned = name.to_vec();
        let mut last_error = initial_error;
        for attempt in 0..options.retries() {
            std::thread::sleep(remove_backoff(attempt.saturating_add(1)));
            match one_remove_outcome(self.nfs.remove_many(dir, std::slice::from_ref(&owned))) {
                Ok(()) => {
                    roots[root].touch();
                    return Ok(true);
                }
                Err(error) => {
                    if error.state() == vfsi_core::FailureState::Unknown
                        || error.cause().is_transport()
                    {
                        note_error(roots, root, error, options)?;
                        return Ok(false);
                    }
                    match error.cause().status() {
                        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOENT)) => {
                            roots[root].touch();
                            return Ok(true);
                        }
                        Some(vfsi_core::StatusCode::Nfs(status))
                            if remove_status_is_retryable(status) =>
                        {
                            roots[root].touch();
                            last_error = error.into_cause();
                        }
                        _ => {
                            note_error(roots, root, error, options)?;
                            return Ok(false);
                        }
                    }
                }
            }
        }
        note_error(
            roots,
            root,
            vfsi_core::OpError::failed(last_error, None),
            options,
        )?;
        Ok(false)
    }
}

impl NfsVecFs {
    pub(super) fn vlistdir_pages_nfs(
        &mut self,
        dirs: &[&Path],
        masks: AttrMask,
        cursors: Vec<Option<DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
        follow_symlinks: bool,
    ) -> vfsi_core::VecResult<vfsi_sync::BackendDirectoryPage> {
        use vfsi_core::OpError;
        if dirs.len() != cursors.len() || page_size == 0 {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        if dirs.is_empty() {
            return Ok(Vec::new());
        }
        if !self.recovery_in_progress && cursors.iter().all(Option::is_none) {
            return self.vector_read_with_recovery(
                |_, _| true,
                |client, indices| match indices {
                    None => client.vlistdir_pages_impl(
                        dirs,
                        masks,
                        (0..dirs.len()).map(|_| None).collect(),
                        page_size,
                        max_entries,
                        follow_symlinks,
                    ),
                    Some(indices) => {
                        let pending: Vec<_> = indices.iter().map(|&index| dirs[index]).collect();
                        client.vlistdir_pages_impl(
                            &pending,
                            masks,
                            (0..indices.len()).map(|_| None).collect(),
                            page_size,
                            max_entries,
                            follow_symlinks,
                        )
                    }
                },
            );
        }
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeDirectoryPage {
            continuation: cursors.iter().any(Option::is_some),
        })?;
        let fresh: Vec<_> = cursors
            .iter()
            .enumerate()
            .filter_map(|(index, cursor)| cursor.is_none().then_some(index))
            .collect();
        let mut states: Vec<Option<vfsi_core::OpResult<NfsDirectoryCursor>>> =
            (0..dirs.len()).map(|_| None).collect();
        let from_cause = |cause: VfError| {
            if cause.is_transport() {
                OpError::unknown(cause, None)
            } else {
                OpError::failed(cause, None)
            }
        };
        let state = |fh| NfsDirectoryCursor {
            fh,
            cookie: DirCookie::default(),
            seen_cookies: std::collections::HashSet::new(),
            buffered: VecDeque::new(),
        };
        if !follow_symlinks || fresh.len() == 1 {
            // Keep atomic no-follow component resolution and the scalar deep
            // path fast path; failures remain local to their logical input.
            let mut fatal: Option<VfError> = None;
            for &index in &fresh {
                let result = if let Some(cause) = &fatal {
                    Err(OpError::not_attempted(cause.clone()))
                } else {
                    self.resolve_directory_path(dirs[index], follow_symlinks)
                        .map_err(|cause| {
                            if cause.is_transport() {
                                fatal = Some(cause.clone());
                            }
                            from_cause(cause)
                        })
                };
                states[index] = Some(result.map(state));
            }
        } else if !fresh.is_empty() {
            let files: Vec<_> = fresh
                .iter()
                .map(|&index| FileOperand::from_os_path(dirs[index]))
                .collect();
            let refs: Vec<_> = files.iter().collect();
            let resolved = self.resolve_files_nfs(&refs, true).unwrap_or_else(|cause| {
                fresh
                    .iter()
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect()
            });
            for (&index, result) in fresh.iter().zip(resolved) {
                states[index] = Some(result.and_then(|(fh, kind)| {
                    if kind != nfs_ftype4_NF4DIR {
                        Err(OpError::failed(
                            VfError::nfs(None, nfsstat4_NFS4ERR_NOTDIR),
                            None,
                        ))
                    } else {
                        Ok(state(fh))
                    }
                }));
            }
        }
        let ids = request_mask_to_attr_list(&masks);
        let mut children = Vec::new();
        let mut child_indices = Vec::new();
        let mut is_child = vec![false; dirs.len()];
        for (index, cursor) in cursors.into_iter().enumerate() {
            if let Some(cursor) = cursor {
                if cursor.is::<NfsChildDirectoryCursor>() {
                    let child = cursor
                        .into_state::<NfsChildDirectoryCursor>()
                        .expect("checked native child type");
                    is_child[index] = true;
                    child_indices.push(index);
                    children.push((child.parent, child.name.into_vec()));
                } else {
                    states[index] = Some(
                        cursor
                            .into_state::<NfsDirectoryCursor>()
                            .map_err(OpError::not_attempted),
                    );
                }
            }
        }
        let child_pages = self
            .nfs
            .readdir_children_bounded(&children, &ids, 32 * 1024)
            .unwrap_or_else(|cause| {
                child_indices
                    .iter()
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect()
            });
        let mut fatal = child_pages
            .iter()
            .filter_map(|r| r.as_ref().err())
            .find(|error| error.cause().is_transport())
            .map(|error| error.cause().clone());
        for (&index, result) in child_indices.iter().zip(child_pages) {
            states[index] = Some(result.and_then(|page| {
                let mut seen_cookies = std::collections::HashSet::new();
                validate_readdir_progress(0, page.cookie.value, &mut seen_cookies, None)
                    .map_err(|cause| OpError::unknown(cause, None))?;
                Ok(NfsDirectoryCursor {
                    fh: page.fh,
                    cookie: page.cookie,
                    seen_cookies,
                    buffered: page.entries.into(),
                })
            }));
        }
        let active: Vec<_> = states
            .iter()
            .enumerate()
            .filter_map(|(index, result)| {
                result
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .filter(|state| state.buffered.is_empty() && !is_child[index])
                    .map(|_| index)
            })
            .collect();
        if !active.is_empty() {
            let pages = if let Some(cause) = fatal.take() {
                active
                    .iter()
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect()
            } else {
                let operations: Vec<_> = active
                    .iter()
                    .map(|&index| {
                        let state = states[index].as_ref().unwrap().as_ref().unwrap();
                        (state.fh.clone(), state.cookie)
                    })
                    .collect();
                self.nfs
                    .readdir_pages_bounded(&operations, &ids, 32 * 1024)
                    .unwrap_or_else(|cause| {
                        active
                            .iter()
                            .map(|_| Err(OpError::not_attempted(cause.clone())))
                            .collect()
                    })
            };
            for (&index, result) in active.iter().zip(pages) {
                let current = states[index].take().expect("active directory");
                states[index] = Some(current.and_then(|mut state| {
                    let (entries, cookie) = result.map_err(|error| {
                        // Fresh identity resolution already performed confirmed
                        // work for this input even when its READDIR stayed unsent.
                        if fresh.binary_search(&index).is_ok()
                            && error.state() == vfsi_core::FailureState::NotAttempted
                        {
                            OpError::failed(error.into_cause(), None)
                        } else {
                            error
                        }
                    })?;
                    validate_readdir_progress(
                        state.cookie.value,
                        cookie.value,
                        &mut state.seen_cookies,
                        None,
                    )
                    .map_err(|cause| OpError::unknown(cause, None))?;
                    state.cookie = cookie;
                    state.buffered = entries.into();
                    Ok(state)
                }));
            }
        }
        Ok(states
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result
                    .expect("all directory page inputs assigned")
                    .and_then(|mut state| {
                        let mut output = Vec::new();
                        let mut seeds = Vec::new();
                        for entry in state.buffered.drain(..state.buffered.len().min(page_size)) {
                            let mut attrs = VfAttrs {
                                file: FileOperand::from_os_path(
                                    &dirs[index].join(path_from_bytes(&entry.name)),
                                ),
                                masks,
                                ..Default::default()
                            };
                            let values = parse_metadata_attrs(&entry.attrmask, &entry.attrs)
                                .map_err(|cause| OpError::unknown(cause, None))?;
                            apply_attrs(&mut attrs, &values);
                            seeds.push((attrs.ftype == VfType::Directory).then(|| {
                                DirPageCursor::new(NfsChildDirectoryCursor {
                                    parent: state.fh.clone(),
                                    name: entry.name.into_boxed_slice(),
                                })
                            }));
                            output.push(attrs);
                        }
                        let next = (!state.buffered.is_empty() || state.cookie.value != 0)
                            .then(|| DirPageCursor::new(state));
                        Ok((output, next, seeds))
                    })
            })
            .collect())
    }
}

#[cfg(test)]
mod root_state_tests {
    use super::*;
    use vfsi_core::{FailureState, OpError};

    #[test]
    fn fatal_root_cause_replaces_an_earlier_ordinary_error_without_losing_completed_roots() {
        let mut roots = vec![
            RemoveRoot::Pending,
            RemoveRoot::Pending,
            RemoveRoot::Complete,
        ];
        let options = RemoveOptions::new().with_continue_on_error(true);
        assert!(
            note_error(
                &mut roots,
                0,
                OpError::failed(VfError::errno(None, libc::EACCES as u32), None),
                options
            )
            .is_ok()
        );
        assert!(matches!(
            note_error(
                &mut roots,
                0,
                OpError::failed(VfError::transport(None, "later read failed"), None),
                options
            ),
            Err(RemoveStop::Fatal)
        ));
        let results = finish_remove_roots(roots, true);
        assert!(results[0].as_ref().unwrap_err().cause().is_transport());
        assert_eq!(
            results[0].as_ref().unwrap_err().state(),
            FailureState::Failed
        );
        assert_eq!(
            results[1].as_ref().unwrap_err().state(),
            FailureState::NotAttempted
        );
        assert!(results[2].is_ok());
    }

    #[test]
    fn root_completion_never_erases_an_error_or_lost_mutation_reply() {
        let mut roots = vec![RemoveRoot::Pending];
        let options = RemoveOptions::new().with_continue_on_error(true);
        assert!(
            note_error(
                &mut roots,
                0,
                OpError::not_attempted(VfError::errno(None, libc::EACCES as u32)),
                options
            )
            .is_ok()
        );
        roots[0].complete();
        assert!(
            matches!(&roots[0],RemoveRoot::Failed(error) if error.state()==FailureState::Failed)
        );
        assert!(matches!(
            note_error(
                &mut roots,
                0,
                OpError::unknown(VfError::nfs(None, nfsstat4_NFS4ERR_ACCESS), None),
                options
            ),
            Err(RemoveStop::Fatal)
        ));
        roots[0].complete();
        let results = finish_remove_roots(roots, true);
        assert_eq!(
            results[0].as_ref().unwrap_err().state(),
            FailureState::Unknown
        );
    }
}

#[cfg(all(test, feature = "test-faults"))]
mod verifier_tests {
    use super::*;
    use crate::rpc::{RpcClient, test_support::readdir_verifier_peer};
    use std::{net::TcpListener, time::Duration};

    fn fetch(
        fs: &mut NfsVecFs,
        mode: &str,
        cursor: DirPageCursor,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        let masks = AttrMask::MODE | AttrMask::MTIME;
        if mode == "scalar" {
            fs.listdir_page_impl(Path::new("/"), masks, Some(cursor), 1, 10, true)
        } else {
            let mut pages = vfsi_core::internal::collect_vec_result(
                fs.vlistdir_pages_nfs(&[Path::new("/")], masks, vec![Some(cursor)], 1, 10, true),
                1,
            )?;
            let (entries, next, _) = pages.remove(0);
            Ok((entries, next))
        }
    }

    #[test]
    fn live_directory_continuations_keep_verifiers_and_returned_attribute_masks() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["scalar", "vector", "child"] {
            for sparse in [false, true] {
                let mut fs = NfsVecFs::builder(&server)
                    .minor_version(Some(minor))
                    .auto_reconnect(false)
                    .connect()
                    .unwrap();
                let root = fs.nfs.root().clone();
                let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                let address = listener.local_addr().unwrap();
                let peer = readdir_verifier_peer(listener, sparse);
                let rpc = RpcClient::connect_with_timeouts(
                    &address.to_string(),
                    Duration::from_secs(2),
                    Duration::from_secs(2),
                )
                .unwrap();
                let original = fs.nfs.replace_test_rpc(rpc);
                let cursor = if mode == "child" {
                    DirPageCursor::new(NfsChildDirectoryCursor {
                        parent: root,
                        name: Box::from(b"child".as_slice()),
                    })
                } else {
                    DirPageCursor::new(NfsDirectoryCursor {
                        fh: root,
                        cookie: Default::default(),
                        seen_cookies: HashSet::new(),
                        buffered: VecDeque::new(),
                    })
                };
                let result = fetch(&mut fs, mode, cursor).and_then(|(first, next)| {
                    let (second, next) = fetch(&mut fs, mode, next.expect("first page continues"))?;
                    assert!(next.is_none(), "EOF must not require a third RPC");
                    Ok((first, second))
                });
                fs.nfs.replace_test_rpc(original);
                let observed = peer.join().unwrap();
                fs.shutdown().unwrap();
                let (first, second) =
                    result.unwrap_or_else(|error| panic!("{mode}/sparse={sparse}: {error:?}"));
                assert_eq!(observed, [(0, [0; 8]), (42, [0x7e; 8])]);
                for (entries, name) in [(first, "one"), (second, "two")] {
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].file.path().unwrap(), Path::new("/").join(name));
                    assert_eq!(entries[0].returned.contains(AttrMask::MODE), !sparse);
                    assert!(entries[0].returned.contains(AttrMask::MTIME));
                    assert_eq!(
                        (entries[0].mtime_sec, entries[0].mtime_nsec),
                        (1_700_000_001, 123)
                    );
                }
            }
        }
    }
    #[test]
    fn live_mutating_scan_restarts_invalid_cookies_with_zero_verifier() {
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
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        for status in [nfsstat4_NFS4ERR_BAD_COOKIE, nfsstat4_NFS4ERR_NOT_SAME] {
            for (value, changed, restart) in
                [(42, true, true), (42, false, false), (0, true, false)]
            {
                let fault = Arc::new(FaultScript::one(
                    OpenFaultPoint::BeforeRemovePage { cookie: value },
                    VfError::nfs(None, status),
                ));
                fs.set_fault_injector(fault.clone());
                let dir = RemoveDir {
                    fh: fs.nfs.root().clone(),
                    parent: None,
                    name: Box::from([]),
                    root: 0,
                    is_root: true,
                };
                let mut stack = Vec::new();
                let mut roots = vec![RemoveRoot::Active];
                let result = fs.enter_directory(
                    dir,
                    DirCookie {
                        value,
                        verifier: [7; 8],
                    },
                    HashSet::from([42]),
                    changed,
                    &mut stack,
                    &mut HashSet::new(),
                    &mut roots,
                    RemoveOptions::new(),
                );
                assert!(fault.is_consumed());
                assert_eq!(result.is_ok(), restart);
                if restart {
                    match stack.pop().expect("restart queued") {
                        RmTask::Enter {
                            cookie,
                            seen_cookies,
                            pass_changed,
                            ..
                        } => {
                            assert_eq!(cookie, DirCookie::default());
                            assert!(seen_cookies.is_empty() && !pass_changed);
                        }
                        _ => panic!("expected a fresh directory scan"),
                    }
                } else {
                    assert!(stack.is_empty());
                    assert!(
                        matches!(&roots[0], RemoveRoot::Failed(error) if error.cause().status() == Some(vfsi_core::StatusCode::Nfs(status)))
                    );
                }
            }
        }
        fs.shutdown().unwrap();
    }
}
