//! Namespace resolution, symlink policy, and path mutations.

use super::*;

fn resolve_lookup_result<T>(
    result: (T, u32),
    follow: bool,
    resolve_target: impl FnOnce() -> VfResult<(T, u32)>,
) -> VfResult<(T, u32)> {
    if follow && result.1 == nfs_ftype4_NF4LNK {
        resolve_target()
    } else {
        Ok(result)
    }
}

/// Root-relative application path for `path`, per the `HandleBackend::abs_path`
/// contract: absolute inputs are taken relative to the application root and
/// relative inputs resolve against `cwd`; neither includes the export prefix.
pub(super) fn namespace_path(cwd: &Path, path: &Path) -> PathBuf {
    let namespace_relative = if path.is_absolute() {
        path.strip_prefix("/").unwrap_or(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    path_from_bytes(&normalize_bytes(path_bytes(&namespace_relative)))
}

/// Export-root path used for NFS resolution.
pub(super) fn server_path_for(root: &Path, cwd: &Path, path: &Path) -> PathBuf {
    root.join(namespace_path(cwd, path))
}

impl NfsVecFs {
    pub(super) fn visible_path(&self, server_path: &Path) -> PathBuf {
        Path::new("/").join(
            server_path
                .strip_prefix(&self.connection.root)
                .unwrap_or(server_path),
        )
    }

    /// Server path (export root joined with the namespace-relative path) used
    /// for NFS resolution.
    pub(super) fn server_path(&self, path: &Path) -> PathBuf {
        server_path_for(&self.connection.root, &self.cwd, path)
    }

    /// Server-path equivalent of [`HandleBackend::vf_path`]. Descriptors and the
    /// saved/cwd sentinels have no path.
    pub(super) fn server_vf_path(&self, file: &FileOperand) -> VfResult<PathBuf> {
        match file {
            FileOperand::Path {
                base: VfPathBase::Abs,
                path,
            } => Ok(self.server_path(&Path::new("/").join(path))),
            FileOperand::Path {
                base: VfPathBase::Cwd,
                path,
            }
            | FileOperand::CwdPath(path) => Ok(self.server_path(path)),
            FileOperand::Cwd => Ok(self.server_path(Path::new(""))),
            FileOperand::Fd(_) | FileOperand::Saved => {
                Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput))
            }
            _ => Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput)),
        }
    }

    /// Resolve `files` to file handles in as few compounds as possible: each
    /// unique parent directory is resolved once, then all children are
    /// LOOKUPed in tolerant batches (`[PUTFH, LOOKUP, GETFH, GETATTR type]`
    /// per child). Returns per-index `(handle, own type)`; a failed LOOKUP
    /// (e.g. NOENT) is reported per path. When `follow` is set, a
    /// final-component symlink is followed through the existing per-path
    /// resolver. Descriptors resolve straight from the open-file table.
    pub(super) fn resolve_files_nfs(
        &mut self,
        files: &[&FileOperand],
        follow: bool,
    ) -> vfsi_core::VecResult<(NfsFh, u32)> {
        use std::collections::BTreeMap;
        use vfsi_core::OpError;
        let mut groups: BTreeMap<Box<[u8]>, Vec<_>> = BTreeMap::new();
        let mut out = (0..files.len()).map(|_| None).collect::<Vec<_>>();
        for (index, file) in files.iter().enumerate() {
            if let Some(fd) = file.fd() {
                out[index] = Some(
                    self.open_files
                        .get(&fd)
                        .map(|open| (open.fh.clone(), 0))
                        .ok_or_else(|| OpError::not_attempted(VfError::errno(None, ERR_EBADF))),
                );
                continue;
            }
            let path = match self.server_vf_path(file) {
                Ok(path) => path,
                Err(error) => {
                    out[index] = Some(Err(OpError::not_attempted(error)));
                    continue;
                }
            };
            if normalize_bytes(path_bytes(&path)).is_empty() {
                out[index] = Some(Ok((self.nfs.root().clone(), nfs_ftype4_NF4DIR)));
                continue;
            }
            match split_path_bytes(path_bytes(&path)) {
                Ok((dir, name)) => groups
                    .entry(Box::from(dir))
                    .or_default()
                    .push((index, name.to_vec())),
                Err(_) => {
                    out[index] = Some(Err(OpError::not_attempted(VfError::errno(None, ERR_NOENT))))
                }
            }
        }
        for (dir, mut entries) in groups {
            let dirfh = match self.resolve_path(&path_from_bytes(&dir), true) {
                Ok(fh) => fh,
                Err(error) => {
                    for (index, _) in entries {
                        out[index] = Some(Err(if error.is_transport() {
                            OpError::unknown(error.clone(), None)
                        } else {
                            OpError::failed(error.clone(), None)
                        }));
                    }
                    continue;
                }
            };
            let ops: Vec<_> = entries
                .iter_mut()
                .map(|(_, name)| (dirfh.clone(), std::mem::take(name)))
                .collect();
            let results = match self.nfs.lookup_getattr_many(&ops) {
                Ok(results) => results,
                Err(error) => entries
                    .iter()
                    .map(|_| Err(OpError::not_attempted(error.clone())))
                    .collect(),
            };
            for ((index, _), result) in entries.into_iter().zip(results) {
                out[index] = Some(result.and_then(|result| {
                    resolve_lookup_result(result, follow, || {
                        #[cfg(feature = "test-faults")]
                        self.inject_open_fault(OpenFaultPoint::BeforeFollowSymlink { index })?;
                        let path = self.server_vf_path(files[index])?;
                        self.resolve_path_and_type(&path, true)
                    })
                    .map_err(|error| {
                        if error.is_transport() {
                            OpError::unknown(error, None)
                        } else {
                            OpError::failed(error, None)
                        }
                    })
                }));
            }
        }
        Ok(out
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing path resolution result"),
                        None,
                    ))
                })
            })
            .collect())
    }

    /// Resolve destination parents in one grouped phase, preserving invalid
    /// paths and lookup failures independently. Names and filehandles move into
    /// the mutation requests without additional copies.
    pub(super) fn resolve_link_parents(
        &mut self,
        paths: &[&Path],
    ) -> vfsi_core::VecResult<(NfsFh, Vec<u8>)> {
        use vfsi_core::OpError;
        let mut slots = (0..paths.len()).map(|_| None).collect::<Vec<_>>();
        let mut files = Vec::with_capacity(paths.len());
        let mut names = Vec::with_capacity(paths.len());
        let mut indices = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            let full = self.server_path(path);
            match split_path_bytes(path_bytes(&full)) {
                Ok((parent, name)) => {
                    files.push(FileOperand::from_os_path(
                        &self.visible_path(&path_from_bytes(parent)),
                    ));
                    names.push(name.to_vec());
                    indices.push(index);
                }
                Err(_) => {
                    slots[index] =
                        Some(Err(OpError::not_attempted(VfError::errno(None, ERR_NOENT))))
                }
            }
        }
        let refs: Vec<_> = files.iter().collect();
        let results = match self.resolve_files_nfs(&refs, true) {
            Ok(results) => results,
            Err(cause) => refs
                .iter()
                .map(|_| Err(OpError::not_attempted(cause.clone())))
                .collect(),
        };
        for ((index, name), result) in indices.into_iter().zip(names).zip(results) {
            slots[index] = Some(result.and_then(|(fh, kind)| {
                if kind == nfs_ftype4_NF4DIR {
                    Ok((fh, name))
                } else {
                    Err(OpError::failed(
                        VfError::nfs(None, nfsstat4_NFS4ERR_NOTDIR),
                        None,
                    ))
                }
            }));
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing destination-parent outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    /// Fresh buffered-walk paths must not be redirected by a replaced ancestor
    /// or child. Raw NFS LOOKUP resolution never follows symlinks; READDIR also
    /// rejects a final symlink handle. Keep ordinary shallow POSIX resolution.
    pub(super) fn resolve_directory_path(
        &mut self,
        dir: &Path,
        follow_symlinks: bool,
    ) -> VfResult<NfsFh> {
        let path = self.server_path(dir);
        if follow_symlinks {
            self.resolve_path(&path, true)
        } else {
            self.nfs
                .resolve(&normalize_bytes(path_bytes(&path)))
                .map_err(|error| vfsi_core::error_from_rpc(error, 0))
        }
    }

    /// Resolve a root-relative path to a file handle, following symlinks in
    /// intermediate components (POSIX pathwalk) and, when `follow_final` is
    /// set, the final component too (for `stat`/`open` semantics).
    ///
    /// The fast path is a single deep-resolve compound; when the server
    /// reports `NFS4ERR_SYMLINK` mid-path, resolution falls back to a
    /// component-wise walk that follows each symlink with READLINK, splicing
    /// its target into the remaining path (hop-capped at 40).
    pub(super) fn resolve_path(&mut self, root_rel: &Path, follow_final: bool) -> VfResult<NfsFh> {
        self.resolve_path_and_type(root_rel, follow_final)
            .map(|(fh, _)| fh)
    }

    /// Resolve a path like resolve_path, retaining the type observed on the
    /// final handle so callers following a symlink can validate its target.
    pub(super) fn resolve_path_and_type(
        &mut self,
        root_rel: &Path,
        follow_final: bool,
    ) -> VfResult<(NfsFh, u32)> {
        let mut path = normalize_bytes(path_bytes(root_rel));
        let mut hops = 0usize;
        loop {
            if hops > 40 {
                return Err(VfError::errno(0, nfsstat4_NFS4ERR_IO)); // symlink loop
            }
            match self.nfs.resolve(&path) {
                Ok(fh) => {
                    if !follow_final {
                        return Ok((fh, 0));
                    }
                    let attrs = self
                        .nfs
                        .getattr(&fh, &[FATTR4_TYPE])
                        .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
                    let is_symlink = type_is_symlink(&attrs)?;
                    let ftype = u32::from_be_bytes(attrs[0..4].try_into().unwrap());
                    if !is_symlink {
                        return Ok((fh, ftype));
                    }
                    let target = self
                        .nfs
                        .readlink(&fh)
                        .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
                    path = self.resolve_target(&path, &target);
                    hops += 1;
                }
                Err(e) if e.status == nfsstat4_NFS4ERR_SYMLINK => {
                    // An intermediate component is a symlink: walk component
                    // by component, following each link we encounter.
                    let comps = components_bytes(&path);
                    if comps.is_empty() {
                        return Err(vfsi_core::error_from_rpc(e, 0));
                    }
                    let mut cur_fh = self.nfs.root().clone();
                    let mut consumed: Vec<u8> = Vec::new();
                    let mut followed = false;
                    for (i, comp) in comps.iter().enumerate() {
                        let is_last = i + 1 == comps.len();
                        let (child, ftype) = self
                            .nfs
                            .lookup_getattr(&cur_fh, comp)
                            .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
                        let full_comp = join_path_bytes(&consumed, comp);
                        if ftype == nfs_ftype4_NF4LNK && (follow_final || !is_last) {
                            let target = self
                                .nfs
                                .readlink(&child)
                                .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
                            let mut rest = Vec::new();
                            for (j, r) in comps.iter().enumerate().skip(i + 1) {
                                if j > i + 1 {
                                    rest.push(b'/');
                                }
                                rest.extend_from_slice(r);
                            }
                            let base = self.resolve_target(&full_comp, &target);
                            path = if rest.is_empty() {
                                base
                            } else {
                                join_path_bytes(&base, &rest)
                            };
                            followed = true;
                            break;
                        }
                        cur_fh = child;
                        if is_last {
                            return Ok((cur_fh, ftype));
                        }
                        consumed = full_comp;
                    }
                    if !followed {
                        return Err(vfsi_core::error_from_rpc(e, 0));
                    }
                    hops += 1;
                }
                Err(e) => return Err(vfsi_core::error_from_rpc(e, 0)),
            }
        }
    }

    pub(super) fn resolve_tcfile(&mut self, f: &FileOperand, follow: bool) -> VfResult<NfsFh> {
        match f {
            FileOperand::Fd(fd) => self
                .open_files
                .get(fd)
                .map(|o| o.fh.clone())
                .ok_or_else(|| VfError::errno(0, ERR_EBADF)),
            FileOperand::Path { .. } | FileOperand::Cwd | FileOperand::CwdPath(_) => {
                let path = self.server_vf_path(f)?;
                if follow {
                    self.resolve_follow(&path)
                } else {
                    self.resolve_path(&path, false)
                }
            }
            FileOperand::Saved => Err(VfError::errno(0, nfsstat4_NFS4ERR_INVAL)),
            _ => Err(VfError::errno(0, nfsstat4_NFS4ERR_INVAL)),
        }
    }

    /// Resolve a symlink target against the link's parent directory (POSIX
    /// semantics), returning a normalized root-relative path. Absolute
    /// targets resolve from the configured application namespace root.
    fn resolve_target(&self, link_path: &[u8], target: &[u8]) -> Vec<u8> {
        let namespace_root = path_bytes(&self.connection.root);
        let link_relative = link_path
            .strip_prefix(namespace_root)
            .map(|path| path.strip_prefix(b"/").unwrap_or(path))
            .unwrap_or(link_path);
        let relative = if target.first() == Some(&b'/') {
            normalize_bytes(&target[1..])
        } else {
            let mut combined = Vec::new();
            if let Some(idx) = link_relative.iter().rposition(|&b| b == b'/') {
                combined.extend_from_slice(&link_relative[..=idx]);
            };
            combined.extend_from_slice(target);
            normalize_bytes(&combined)
        };
        path_bytes(&self.connection.root.join(path_from_bytes(&relative))).to_vec()
    }

    pub(super) fn follow_target_path(&mut self, root_rel: &Path) -> VfResult<PathBuf> {
        let mut current = normalize_bytes(path_bytes(root_rel));
        let mut hops = 0usize;
        loop {
            let namespace_root = path_bytes(&self.connection.root);
            let visible = current
                .strip_prefix(namespace_root)
                .map(|path| path.strip_prefix(b"/").unwrap_or(path))
                .unwrap_or(&current);
            let mut abs = b"/".to_vec();
            abs.extend_from_slice(visible);
            let abs_path = path_from_bytes(&abs);
            let st = match self.lstat_impl(&abs_path) {
                Ok(s) => s,
                Err(e) if e.raw_code() == ERR_NOENT => return Ok(path_from_bytes(&current)),
                Err(e) => return Err(e),
            };
            if st.ftype != VfType::Symlink {
                return Ok(path_from_bytes(&current));
            }
            if hops >= 40 {
                return Err(VfError::errno(0, nfsstat4_NFS4ERR_IO)); // symlink loop
            }
            let target = self.readlink_raw_impl(&abs_path)?;
            current = self.resolve_target(&current, &target);
            hops += 1;
        }
    }

    pub(super) fn resolve_follow(&mut self, path: &Path) -> VfResult<NfsFh> {
        self.resolve_path(path, true)
    }

    /// Preserve merged rename completions and repair only a proven-unexecuted
    /// ancestor lookup before allowing later mutations to run.
    pub(super) fn vrename_nfs(
        &mut self,
        pairs: &[(FileOperand, FileOperand)],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        self.ensure_writable(pairs.len())?;
        let mut slots = (0..pairs.len()).map(|_| None).collect::<Vec<_>>();
        let mut prepared = Vec::with_capacity(pairs.len());
        let mut indices = Vec::with_capacity(pairs.len());
        for (index, (src, dst)) in pairs.iter().enumerate() {
            match self.server_vf_path(src).and_then(|src| {
                self.server_vf_path(dst)
                    .map(|dst| crate::client::PathRenamePair {
                        src: path_bytes(&src).to_vec(),
                        dst: path_bytes(&dst).to_vec(),
                    })
            }) {
                Ok(pair) => {
                    indices.push(index);
                    prepared.push(pair);
                }
                Err(cause) => slots[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        let mut start = 0;
        while start < prepared.len() {
            let (results, repair) = match self.nfs.rename_path_results(&prepared[start..], true) {
                Ok(result) => result,
                Err(cause) => {
                    for &index in &indices[start..] {
                        slots[index] = Some(Err(OpError::not_attempted(cause.clone())));
                    }
                    break;
                }
            };
            let completed = repair.map_or(prepared.len() - start, |point| point.input);
            let mut outcomes = results.into_iter();
            for (offset, result) in outcomes.by_ref().take(completed).enumerate() {
                slots[indices[start + offset]] = Some(result);
            }
            let Some(point) = repair else { break };
            let relative = point.input;
            let index = indices[start + relative];
            let failure = outcomes.next().expect("repair retains its failed input");
            // Some servers use NOTDIR for an incompatible named destination.
            // At RENAME, repair that status only after observing a symlink
            // parent. An ordinary failed rename must never be issued again.
            if point.failed_op == nfs_opnum4_NFS4_OP_RENAME
                && failure.as_ref().err().is_some_and(|error| {
                    error.cause().status()
                        == Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOTDIR))
                })
            {
                let pair = &prepared[start + relative];
                let follows_parent = [&pair.src, &pair.dst].into_iter().any(|path| {
                    split_path_bytes(path).ok().is_some_and(|(parent, _)| {
                        self.resolve_path_and_type(&path_from_bytes(parent), false)
                            .is_ok_and(|(_, kind)| kind == nfs_ftype4_NF4LNK)
                    })
                });
                if !follows_parent {
                    slots[index] = Some(failure);
                    start += relative + 1;
                    continue;
                }
            }
            slots[index] = Some(match self.renamev_phased(&pairs[index..index + 1]) {
                Ok(mut results) if results.len() == 1 => results.pop().unwrap().map_err(|error| {
                    // The merged call already submitted this logical input.
                    // A rejected repair must not turn it into NotAttempted.
                    if error.state() == vfsi_core::FailureState::NotAttempted {
                        OpError::failed(error.into_cause(), None)
                    } else {
                        error
                    }
                }),
                Ok(_) => Err(OpError::unknown(
                    VfError::invalid_reply(None, "invalid rename repair count"),
                    None,
                )),
                Err(cause) => Err(OpError::failed(cause, None)),
            });
            start += relative + 1;
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing rename outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    /// Resolve each unique parent once, retaining preparation failures per input.
    pub(super) fn renamev_phased(
        &mut self,
        pairs: &[(FileOperand, FileOperand)],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let mut cache = std::collections::HashMap::<Box<[u8]>, VfResult<NfsFh>>::new();
        fn parent(
            fs: &mut NfsVecFs,
            cache: &mut std::collections::HashMap<Box<[u8]>, VfResult<NfsFh>>,
            path: &[u8],
        ) -> VfResult<NfsFh> {
            if let Some(result) = cache.get(path) {
                return result.clone();
            }
            let result = fs
                .resolve_path_and_type(&path_from_bytes(path), true)
                .and_then(|(fh, kind)| {
                    if kind == nfs_ftype4_NF4DIR {
                        Ok(fh)
                    } else {
                        Err(VfError::nfs(None, nfsstat4_NFS4ERR_NOTDIR))
                    }
                });
            cache.insert(path.into(), result.clone());
            result
        }
        let mut slots = (0..pairs.len()).map(|_| None).collect::<Vec<_>>();
        let mut ops = Vec::with_capacity(pairs.len());
        let mut indices = Vec::with_capacity(pairs.len());
        for (index, (src, dst)) in pairs.iter().enumerate() {
            let prepared = (|| {
                let source = self.server_vf_path(src)?;
                let destination = self.server_vf_path(dst)?;
                let (sdir, oldname) = split_path_bytes(path_bytes(&source))
                    .map_err(|_| VfError::errno(None, ERR_NOENT))?;
                let (ddir, newname) = split_path_bytes(path_bytes(&destination))
                    .map_err(|_| VfError::errno(None, ERR_NOENT))?;
                Ok(crate::client::RenameOp {
                    srcdir: parent(self, &mut cache, sdir)?,
                    oldname: oldname.to_vec(),
                    dstdir: parent(self, &mut cache, ddir)?,
                    newname: newname.to_vec(),
                })
            })();
            match prepared {
                Ok(op) => {
                    indices.push(index);
                    ops.push(op);
                }
                Err(cause) => slots[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        let results = match self.nfs.rename_many(&ops) {
            Ok(results) => results,
            Err(cause) => ops
                .iter()
                .map(|_| Err(OpError::not_attempted(cause.clone())))
                .collect(),
        };
        for (index, result) in indices.into_iter().zip(results) {
            slots[index] = Some(result);
        }
        Ok(slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing phased rename outcome"),
                        None,
                    ))
                })
            })
            .collect())
    }

    pub(super) fn vremove_nfs(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        self.ensure_writable(files.len())?;
        let mut slots = (0..files.len()).map(|_| None).collect::<Vec<_>>();
        let mut paths = Vec::new();
        let mut indices = Vec::new();
        for (index, file) in files.iter().enumerate() {
            match self.server_vf_path(file) {
                Ok(path) => {
                    paths.push(path_bytes(&path).to_vec());
                    indices.push(index);
                }
                Err(cause) => slots[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        let mut start = 0;
        while start < paths.len() {
            let (results, repair) = match self.nfs.remove_path_results(&paths[start..], true) {
                Ok(results) => results,
                Err(cause) => {
                    for &index in &indices[start..] {
                        slots[index] = Some(Err(OpError::not_attempted(cause.clone())));
                    }
                    break;
                }
            };
            let completed = repair.map_or(paths.len() - start, |point| point.input);
            let mut results = results.into_iter();
            for (offset, result) in results.by_ref().take(completed).enumerate() {
                slots[indices[start + offset]] = Some(result);
            }
            let Some(point) = repair else {
                break;
            };
            let index = indices[start + point.input];
            let failure = results.next().expect("repair retains failed REMOVE");
            let path = &paths[start + point.input];
            let repairable = point.failed_op != nfs_opnum4_NFS4_OP_REMOVE
                || failure
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.cause().raw_code() == nfsstat4_NFS4ERR_SYMLINK)
                || split_path_bytes(path).ok().is_some_and(|(parent, _)| {
                    self.resolve_path_and_type(&path_from_bytes(parent), false)
                        .is_ok_and(|(_, kind)| kind == nfs_ftype4_NF4LNK)
                });
            if !repairable {
                slots[index] = Some(failure);
                start += point.input + 1;
                continue;
            }
            // The merged driver stopped at this proven parent-type rejection.
            // Every later prepared input is untouched. Resolve those parents
            // together so a symlink ancestor does not degrade into singleton RPCs.
            let remaining = &files[index..];
            let outcomes = self.removev_phased(remaining).unwrap_or_else(|cause| {
                remaining
                    .iter()
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect()
            });
            if outcomes.len() != remaining.len() {
                let cause = VfError::invalid_reply(None, "invalid REMOVE repair count");
                for slot in &mut slots[index..] {
                    *slot = Some(Err(OpError::unknown(cause.clone(), None)));
                }
            } else {
                for (offset, result) in outcomes.into_iter().enumerate() {
                    slots[index + offset] = Some(if offset == 0 {
                        result.map_err(|error| {
                            if error.state() == vfsi_core::FailureState::NotAttempted {
                                OpError::failed(error.into_cause(), None)
                            } else {
                                error
                            }
                        })
                    } else {
                        result
                    });
                }
            }
            break;
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all NFS removal inputs assigned"))
            .collect())
    }

    /// Resolve each distinct parent once and preserve every native REMOVE result.
    pub(super) fn removev_phased(&mut self, files: &[FileOperand]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let mut groups = std::collections::BTreeMap::<Box<[u8]>, Vec<(usize, Vec<u8>)>>::new();
        let mut slots = (0..files.len()).map(|_| None).collect::<Vec<_>>();
        for (index, file) in files.iter().enumerate() {
            let request = self.server_vf_path(file).and_then(|path| {
                let (parent, name) = split_path_bytes(path_bytes(&path))
                    .map_err(|_| VfError::errno(None, ERR_NOENT))?;
                Ok((Box::<[u8]>::from(parent), name.to_vec()))
            });
            match request {
                Ok((parent, name)) => groups.entry(parent).or_default().push((index, name)),
                Err(cause) => slots[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        for (parent, entries) in groups {
            let parent = match self
                .resolve_path_and_type(&path_from_bytes(&parent), true)
                .and_then(|(handle, kind)| {
                    if kind == nfs_ftype4_NF4DIR {
                        Ok(handle)
                    } else {
                        Err(VfError::nfs(None, nfsstat4_NFS4ERR_NOTDIR))
                    }
                }) {
                Ok(parent) => parent,
                Err(cause) => {
                    for (index, _) in entries {
                        slots[index] = Some(Err(OpError::not_attempted(cause.clone())));
                    }
                    continue;
                }
            };
            let names: Vec<_> = entries.iter().map(|(_, name)| name.clone()).collect();
            let results = self
                .nfs
                .remove_many(&parent, &names)
                .unwrap_or_else(|cause| {
                    names
                        .iter()
                        .map(|_| Err(OpError::not_attempted(cause.clone())))
                        .collect()
                });
            for ((index, _), result) in entries.into_iter().zip(results) {
                slots[index] = Some(result);
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all phased removal inputs assigned"))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn following_lookup_symlink_returns_the_resolved_target_type() {
        let result = resolve_lookup_result(("link", nfs_ftype4_NF4LNK), true, || {
            Ok(("target", nfs_ftype4_NF4DIR))
        })
        .unwrap();

        assert_eq!(result, ("target", nfs_ftype4_NF4DIR));
    }

    #[test]
    fn lookup_without_following_keeps_the_link_and_skips_resolution() {
        let result = resolve_lookup_result(("link", nfs_ftype4_NF4LNK), false, || {
            panic!("nofollow must not resolve the target")
        })
        .unwrap();

        assert_eq!(result, ("link", nfs_ftype4_NF4LNK));
    }

    #[test]
    fn following_non_symlink_does_not_resolve_again() {
        let result = resolve_lookup_result(("directory", nfs_ftype4_NF4DIR), true, || {
            panic!("non-symlinks are already resolved")
        })
        .unwrap();

        assert_eq!(result, ("directory", nfs_ftype4_NF4DIR));
    }
}
