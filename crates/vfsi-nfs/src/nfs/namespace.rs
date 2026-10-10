//! Namespace resolution, symlink policy, and path mutations.

use super::*;

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
    pub(super) fn server_vf_path(&self, file: &VfFile) -> VfResult<PathBuf> {
        match file {
            VfFile::Path {
                base: VfPathBase::Abs,
                path,
            } => Ok(self.server_path(&Path::new("/").join(path))),
            VfFile::Path {
                base: VfPathBase::Cwd,
                path,
            }
            | VfFile::CwdPath(path) => Ok(self.server_path(path)),
            VfFile::Cwd => Ok(self.server_path(Path::new(""))),
            VfFile::Descriptor(_) | VfFile::Saved => Err(VfError::failure(0, ERR_INVAL)),
            _ => Err(VfError::failure(0, ERR_INVAL)),
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
        files: &[&VfFile],
        follow: bool,
    ) -> VfResult<Vec<Result<(WireFileHandle, u32), u32>>> {
        use std::collections::{BTreeMap, HashMap};
        // Group by parent directory.
        let mut groups: BTreeMap<Vec<u8>, Vec<(usize, Vec<u8>)>> = BTreeMap::new();
        let mut out: Vec<Result<(WireFileHandle, u32), u32>> =
            vec![Err(nfsstat4_NFS4ERR_NOENT); files.len()];
        for (i, f) in files.iter().enumerate() {
            if f.is_descriptor() {
                let fd = f.fd().unwrap();
                out[i] = match self.open_files.get(&fd) {
                    Some(o) => Ok((o.fh.clone(), 0)),
                    None => Err(ERR_EBADF),
                };
                continue;
            }
            let path = self.server_vf_path(f).map_err(|e| e.with_index(i))?;
            if path.as_os_str().is_empty() {
                // The export root itself.
                out[i] = Ok((self.nfs.root().clone(), nfs_ftype4_NF4DIR));
                continue;
            }
            let path_bytes = path_bytes(&path);
            let (dir, name) =
                split_path_bytes(path_bytes).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            groups.entry(dir).or_default().push((i, name));
        }
        let mut dir_cache: HashMap<Vec<u8>, WireFileHandle> = HashMap::new();
        for (dir, entries) in groups {
            let dirfh = match dir_cache.get(&dir) {
                Some(fh) => fh.clone(),
                None => match self.resolve_path(&path_from_bytes(&dir), true) {
                    Ok(fh) => {
                        dir_cache.insert(dir, fh.clone());
                        fh
                    }
                    Err(e) => {
                        if e.is_transport() {
                            return Err(e);
                        }
                        let err = e.err_no();
                        for (i, _) in &entries {
                            out[*i] = Err(err);
                        }
                        continue;
                    }
                },
            };
            let ops: Vec<(WireFileHandle, Vec<u8>)> = entries
                .iter()
                .map(|(_, name)| (dirfh.clone(), name.clone()))
                .collect();
            let results = self
                .nfs
                .lookup_getattr_many(&ops)
                .map_err(|e| vfsi_core::error_from_rpc(e, None))?;
            for ((i, _), r) in entries.iter().zip(results) {
                match r {
                    Ok((fh, ftype)) => {
                        if follow && ftype == nfs_ftype4_NF4LNK {
                            let full = self.server_vf_path(files[*i])?;
                            out[*i] = match self.resolve_follow(&full) {
                                Ok(fh) => Ok((fh, ftype)),
                                Err(e) => Err(e.err_no()),
                            };
                        } else {
                            out[*i] = Ok((fh, ftype));
                        }
                    }
                    Err(status) => out[*i] = Err(status),
                }
            }
        }
        Ok(out)
    }

    /// Fresh buffered-walk paths must not be redirected by a replaced ancestor
    /// or child. Raw NFS LOOKUP resolution never follows symlinks; READDIR also
    /// rejects a final symlink handle. Keep ordinary shallow POSIX resolution.
    pub(super) fn resolve_directory_path(
        &mut self,
        dir: &Path,
        follow_symlinks: bool,
    ) -> VfResult<WireFileHandle> {
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
    pub(super) fn resolve_path(
        &mut self,
        root_rel: &Path,
        follow_final: bool,
    ) -> VfResult<WireFileHandle> {
        let mut path = normalize_bytes(path_bytes(root_rel));
        let mut hops = 0usize;
        loop {
            if hops > 40 {
                return Err(VfError::failure(0, nfsstat4_NFS4ERR_IO)); // symlink loop
            }
            match self.nfs.resolve(&path) {
                Ok(fh) => {
                    if !follow_final {
                        return Ok(fh);
                    }
                    let t = self
                        .nfs
                        .getattr(&fh, &[FATTR4_TYPE])
                        .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
                    if !type_is_symlink(&t)? {
                        return Ok(fh);
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
                            return Ok(cur_fh);
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

    pub(super) fn resolve_tcfile(&mut self, f: &VfFile, follow: bool) -> VfResult<WireFileHandle> {
        match f {
            VfFile::Descriptor(fd) => self
                .open_files
                .get(fd)
                .map(|o| o.fh.clone())
                .ok_or_else(|| VfError::failure(0, ERR_EBADF)),
            VfFile::Path { .. } | VfFile::Cwd | VfFile::CwdPath(_) => {
                let path = self.server_vf_path(f)?;
                if follow {
                    self.resolve_follow(&path)
                } else {
                    self.resolve_path(&path, false)
                }
            }
            VfFile::Saved => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
            _ => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
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
                Err(e) if e.err_no() == ERR_NOENT => return Ok(path_from_bytes(&current)),
                Err(e) => return Err(e),
            };
            if st.ftype != VfType::Symlink {
                return Ok(path_from_bytes(&current));
            }
            if hops >= 40 {
                return Err(VfError::failure(0, nfsstat4_NFS4ERR_IO)); // symlink loop
            }
            let target = self.readlink_raw_impl(&abs_path)?;
            current = self.resolve_target(&current, &target);
            hops += 1;
        }
    }

    pub(super) fn resolve_follow(&mut self, path: &Path) -> VfResult<WireFileHandle> {
        self.resolve_path(path, true)
    }

    /// The legacy phased renamev (cached parent resolution + rename_many).
    pub(super) fn renamev_phased(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        let mut src_cache: std::collections::HashMap<Vec<u8>, WireFileHandle> =
            std::collections::HashMap::new();
        let mut dst_cache: std::collections::HashMap<Vec<u8>, WireFileHandle> =
            std::collections::HashMap::new();
        let mut ops = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let s = self.server_vf_path(src).map_err(|e| e.with_index(i))?;
            let d = self.server_vf_path(dst).map_err(|e| e.with_index(i))?;
            let (sdir, sname) =
                split_path_bytes(path_bytes(&s)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let (ddir, dname) =
                split_path_bytes(path_bytes(&d)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let sdirfh = match src_cache.get(&sdir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&sdir), true)
                        .map_err(|e| e.with_index(i))?;
                    src_cache.insert(sdir.clone(), fh.clone());
                    fh
                }
            };
            let ddirfh = match dst_cache.get(&ddir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&ddir), true)
                        .map_err(|e| e.with_index(i))?;
                    dst_cache.insert(ddir.clone(), fh.clone());
                    fh
                }
            };
            ops.push(crate::client::RenameOp {
                srcdir: sdirfh,
                oldname: sname,
                dstdir: ddirfh,
                newname: dname,
            });
        }
        self.nfs
            .rename_many(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)
    }

    /// The legacy phased removev (grouped per parent + remove_many).
    pub(super) fn removev_phased(&mut self, files: &[VfFile]) -> VfRes {
        use std::collections::BTreeMap;
        // Group by parent directory to batch REMOVEs.
        let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            let path = self.server_vf_path(f).map_err(|e| e.with_index(i))?;
            let (dir, name) =
                split_path_bytes(path_bytes(&path)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            groups.entry(dir).or_default().push(name);
        }
        for (dir, names) in &groups {
            let dirfh = self
                .resolve_path(&path_from_bytes(dir), true)
                .map_err(|e| e.with_index(0))?;
            self.nfs
                .remove_many(&dirfh, names)
                .map_err(vfsi_core::error_from_rpc_indexed)?;
        }
        Ok(())
    }

    /// Apply the requested modes of `dirs` in one batched resolve + SETATTR.
    /// Used by vmkdir_impl after creation (NFSv4 CREATE cannot carry mode attrs).
    pub(super) fn apply_dir_modes(&mut self, dirs: &[VfAttrs]) -> VfRes {
        let mut paths: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        let mut indices = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            if a.masks.contains(AttrMask::MODE) {
                let path = match self.server_vf_path(&a.file) {
                    Ok(p) => p,
                    Err(e) => return Err(e.with_index(i)),
                };
                paths.push(self.visible_path(&path));
                indices.push(i);
            }
        }
        if paths.is_empty() {
            return Ok(());
        }
        let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = match self.resolve_files_nfs(&refs, true) {
            Ok(r) => r,
            Err(e) => {
                return Err(e.map_index(|rel| indices.get(rel).copied().unwrap_or(rel)));
            }
        };
        let mut setattrs = Vec::with_capacity(dirs.len());
        for (k, r) in resolved.iter().enumerate() {
            match r {
                Ok((fh, _)) => setattrs.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: Some(dirs[indices[k]].mode & 0o7777),
                    uid: None,
                    gid: None,
                    size: None,
                    atime: None,
                    mtime: None,
                }),
                Err(status) => return Err(VfError::nfs(indices[k], *status)),
            }
        }
        if !setattrs.is_empty() {
            self.nfs.setattr_many(&setattrs).map_err(|e| {
                let rel = e.op_index;
                let orig = indices.get(rel).copied().unwrap_or(0);
                vfsi_core::error_from_rpc_indexed(e.with_op_index(orig))
            })?;
        }
        Ok(())
    }

    /// Whether an NFS status indicates the special stateid was rejected
    /// (rather than a per-file failure), so the merged path must be disabled.
    pub(super) fn is_stateid_error(status: u32) -> bool {
        matches!(
            status,
            nfsstat4_NFS4ERR_BAD_STATEID
                | nfsstat4_NFS4ERR_OLD_STATEID
                | nfsstat4_NFS4ERR_STALE_STATEID
                | nfsstat4_NFS4ERR_BAD_SEQID
                | nfsstat4_NFS4ERR_NOTSUPP
        )
    }
}
