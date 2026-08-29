//! NFSv4.1 implementation of the vectorized [`VecFs`] API.
//!
//! [`NfsVecFs`] is the analog of the C `tc_init()` module handle: it connects
//! to an NFSv4.1 server, coalesces vector operations into as few compounds as
//! the server supports, and destroys its session/clientid on drop.

// bindgen emits lowercase constants (e.g. nfs_ftype4_NF4DIR) matched here.
#![allow(non_upper_case_globals)]

use std::path::PathBuf;

use nfsv41_sys::*;

use crate::client::{FileHandle, NfsClient, OpenCreate};
use crate::vecfs::*;

// Re-export the shared types/trait so `use vnfs::nfs::*` works.
pub use crate::vecfs::*;

/// An open file on the NFS server: resolved handle, open stateid, the current
/// read/write offset (for `tc_fseek`), and whether it was opened with
/// `O_APPEND` (writes then always go to the end of the file).
#[derive(Debug, Clone)]
struct OpenFile {
    fh: FileHandle,
    stateid: stateid4,
    cur_offset: u64,
    append: bool,
}

/// An NFSv4.1 client exposing the vectorized [`VecFs`] API.
pub struct NfsVecFs {
    nfs: NfsClient,
    cwd: PathBuf,
    next_fd: i32,
    /// Canonical open-file state, keyed by the client-assigned descriptor.
    open_files: std::collections::HashMap<i32, OpenFile>,
    /// How path-based bulk I/O is issued: one compound per batch including
    /// CLOSE (Ganesha's special-stateid behavior), one open+I/O compound
    /// plus a separate CLOSE compound (portable), or the old phased path.
    merged_mode: MergedIoMode,
}

/// Which merged-compound strategy the NFS backend uses for path-based bulk
/// I/O, downgraded automatically when the server rejects the current form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergedIoMode {
    /// `[PUTROOTFH, LOOKUP, SAVEFH, OPEN, WRITE, RESTOREFH, ..., CLOSE]` in a
    /// single compound (special-stateid CLOSE; Ganesha).
    Full,
    /// One open+I/O compound, then a separate CLOSE compound with the real
    /// stateids (portable).
    OpenWrite,
    /// Legacy phased path: resolve + probe + open + I/O + close compounds.
    Off,
}

impl NfsVecFs {
    /// Force a particular merged-compound mode (diagnostics/tests): "full"
    /// (default, one compound incl. CLOSE), "openwrite" (open+I/O compound +
    /// separate close), or "off" (legacy phased path).
    #[doc(hidden)]
    pub fn set_merged_mode(&mut self, mode: &str) {
        self.merged_mode = match mode {
            "openwrite" => MergedIoMode::OpenWrite,
            "off" => MergedIoMode::Off,
            _ => MergedIoMode::Full,
        };
    }

    /// Resolve `files` to file handles in as few compounds as possible: each
    /// unique parent directory is resolved once, then all children are
    /// LOOKUPed in tolerant batches (`[PUTFH, LOOKUP, GETFH, GETATTR type]`
    /// per child). Returns per-index `(handle, own type)`; a failed LOOKUP
    /// (e.g. NOENT) is reported per path. When `follow` is set, a
    /// final-component symlink is followed through the existing per-path
    /// resolver. Descriptors resolve straight from the open-file table.
    fn resolve_many_tcfile(
        &mut self,
        files: &[&VfFile],
        follow: bool,
    ) -> VfResult<Vec<Result<(FileHandle, u32), u32>>> {
        use std::collections::{BTreeMap, HashMap};
        // Group by parent directory.
        let mut groups: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
        let mut out: Vec<Result<(FileHandle, u32), u32>> =
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
            let path = self.vf_path(f).map_err(|e| e.with_index(i))?;
            if path.is_empty() {
                // The export root itself.
                out[i] = Ok((self.nfs.root().clone(), nfs_ftype4_NF4DIR));
                continue;
            }
            let (dir, name) = split_path(&path).map_err(|e| VfError::failure(i, e))?;
            groups
                .entry(dir.to_string())
                .or_default()
                .push((i, name.to_string()));
        }
        let mut dir_cache: HashMap<String, FileHandle> = HashMap::new();
        for (dir, entries) in groups {
            let dirfh = match dir_cache.get(&dir) {
                Some(fh) => fh.clone(),
                None => match self.resolve_path(&dir, true) {
                    Ok(fh) => {
                        dir_cache.insert(dir, fh.clone());
                        fh
                    }
                    Err(e) => {
                        let err = e.err_no();
                        for (i, _) in &entries {
                            out[*i] = Err(err);
                        }
                        continue;
                    }
                },
            };
            let ops: Vec<(FileHandle, String)> = entries
                .iter()
                .map(|(_, name)| (dirfh.clone(), name.clone()))
                .collect();
            let results = self
                .nfs
                .lookup_getattr_many(&ops)
                .map_err(|e| VfError::from_rpc(e, 0))?;
            for ((i, _), r) in entries.iter().zip(results) {
                match r {
                    Ok((fh, ftype)) => {
                        if follow && ftype == nfs_ftype4_NF4LNK {
                            let full = self.vf_path(files[*i])?;
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

    /// The merged (single-compound) setattrsv: resolve each parent once,
    /// then LOOKUP + GETATTR type + SETATTR per file. Symlinks (either to
    /// refuse for lsetattrsv or to follow for setattrsv) are handled
    /// per-file via the phased path.
    fn setattrsv_impl(&mut self, attrs: &[VfAttrs], follow: bool) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE.union(AttrMask::SIZE);
        if attrs.is_empty() {
            return Ok(());
        }
        for (i, a) in attrs.iter().enumerate() {
            let unsupported = a.masks.difference(SETTABLE);
            if !unsupported.is_empty() {
                return Err(VfError::unsupported(i));
            }
        }
        if attrs.iter().any(|a| a.file.is_descriptor()) {
            return self.setattrsv_phased(attrs, follow);
        }
        let mut ops = Vec::with_capacity(attrs.len());
        for a in attrs {
            let path = self.vf_path(&a.file)?;
            let mode = if a.masks.contains(AttrMask::MODE) {
                Some(a.mode & 0o7777)
            } else {
                None
            };
            let size = if a.masks.contains(AttrMask::SIZE) {
                Some(a.size)
            } else {
                None
            };
            ops.push(crate::client::PathSetattrOp {
                path,
                mode,
                size,
                check_type: true,
            });
        }
        match self.nfs.setattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((_i, _st)) = outcome.failed {
                    return self.setattrsv_phased(attrs, follow);
                }
                for (i, a) in attrs.iter().enumerate() {
                    let own_type = outcome.types[i];
                    if own_type == Some(nfs_ftype4_NF4LNK) {
                        if !follow {
                            // No non-following mode/size setter for symlinks.
                            return Err(VfError::unsupported(i));
                        }
                        self.setattr_one_following(i, a)?;
                    }
                }
                Ok(())
            }
            Err(_) => self.setattrsv_phased(attrs, follow),
        }
    }

    /// The legacy phased setattrsv (resolve_many_tcfile + setattr_many).
    fn setattrsv_phased(&mut self, attrs: &[VfAttrs], follow: bool) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE.union(AttrMask::SIZE);
        for (i, a) in attrs.iter().enumerate() {
            let unsupported = a.masks.difference(SETTABLE);
            if !unsupported.is_empty() {
                return Err(VfError::unsupported(i));
            }
        }
        let files: Vec<&VfFile> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_many_tcfile(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let (fh, ftype) = match &resolved[i] {
                Ok(x) => x.clone(),
                Err(status) => return Err(VfError::failure(i, *status)),
            };
            if !follow && ftype == nfs_ftype4_NF4LNK {
                // NFSv4 has no non-following mode/size setter for symlinks;
                // refuse like the `std::fs` backend instead of pretending the
                // SETATTR applied to the link.
                return Err(VfError::unsupported(i));
            }
            let mode = if a.masks.contains(AttrMask::MODE) {
                Some(a.mode & 0o7777)
            } else {
                None
            };
            let size = if a.masks.contains(AttrMask::SIZE) {
                Some(a.size)
            } else {
                None
            };
            ops.push(crate::client::SetattrOp { fh, mode, size });
        }
        self.nfs
            .setattr_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    /// The merged (single-compound) getattrsv: resolve each parent once,
    /// then LOOKUP + GETATTR per file. Final-component symlinks (follow
    /// semantics) are resolved individually afterwards.
    fn getattrsv_impl(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        if attrs.is_empty() {
            return Ok(());
        }
        if attrs.iter().any(|a| a.file.is_descriptor()) {
            return self.getattrsv_phased(attrs, follow);
        }
        let mut ops = Vec::with_capacity(attrs.len());
        for a in attrs.iter() {
            let path = self.vf_path(&a.file)?;
            ops.push(crate::client::PathGetattrOp {
                path,
                attrs: request_mask_to_attr_list(&a.masks),
            });
        }
        match self.nfs.getattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((_i, _st)) = outcome.failed {
                    return self.getattrsv_phased(attrs, follow);
                }
                let mut symlinks = Vec::new();
                for (i, (a, op)) in attrs.iter_mut().zip(&ops).enumerate() {
                    let list = outcome.lists[i].as_deref().unwrap_or_default();
                    let v = parse_attr_list(&op.attrs, list)?;
                    apply_attrs(a, &v);
                    if follow && a.ftype == VfType::Symlink {
                        symlinks.push(i);
                    }
                }
                for i in symlinks {
                    self.stat_one_following(i, &mut attrs[i])?;
                }
                Ok(())
            }
            Err(_) => self.getattrsv_phased(attrs, follow),
        }
    }

    /// The legacy phased getattrsv (resolve_many_tcfile + getattr_many).
    fn getattrsv_phased(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        let files: Vec<&VfFile> = attrs.iter().map(|a| &a.file).collect();
        let resolved = self.resolve_many_tcfile(&files, follow)?;
        let mut ops = Vec::with_capacity(attrs.len());
        let mut ids_list = Vec::with_capacity(attrs.len());
        let mut first_failure: Option<VfError> = None;
        for (i, a) in attrs.iter().enumerate() {
            let fh = match &resolved[i] {
                Ok((fh, _)) => fh.clone(),
                Err(status) => {
                    if first_failure.is_none() {
                        first_failure = Some(VfError::failure(i, *status));
                    }
                    ids_list.push(Vec::new());
                    continue;
                }
            };
            let ids = request_mask_to_attr_list(&a.masks);
            ids_list.push(ids.clone());
            ops.push(crate::client::GetattrOp { fh, attrs: ids });
        }
        if let Some(e) = first_failure {
            return Err(e);
        }
        let results = self
            .nfs
            .getattr_many(&ops)
            .map_err(VfError::from_rpc_indexed)?;
        for ((a, ids), list) in attrs.iter_mut().zip(ids_list).zip(results) {
            let v = parse_attr_list(&ids, &list)?;
            apply_attrs(a, &v);
        }
        Ok(())
    }

    /// stat one path following final-component symlinks (phased).
    fn stat_one_following(&mut self, index: usize, a: &mut VfAttrs) -> VfResult<()> {
        let path = self.vf_path(&a.file).map_err(|e| e.with_index(index))?;
        let fh = self
            .resolve_follow(&path)
            .map_err(|e| e.with_index(index))?;
        let ids = request_mask_to_attr_list(&a.masks);
        let list = self
            .nfs
            .getattr(&fh, &ids)
            .map_err(|e| VfError::from_rpc(e, index))?;
        let v = parse_attr_list(&ids, &list)?;
        apply_attrs(a, &v);
        Ok(())
    }

    /// setattr one path following final-component symlinks (phased).
    fn setattr_one_following(&mut self, index: usize, a: &VfAttrs) -> VfResult<()> {
        let path = self.vf_path(&a.file).map_err(|e| e.with_index(index))?;
        let fh = self
            .resolve_follow(&path)
            .map_err(|e| e.with_index(index))?;
        let mode = if a.masks.contains(AttrMask::MODE) {
            Some(a.mode & 0o7777)
        } else {
            None
        };
        let size = if a.masks.contains(AttrMask::SIZE) {
            Some(a.size)
        } else {
            None
        };
        self.nfs
            .setattr(&fh, mode, size)
            .map_err(|e| VfError::from_rpc(e, index))
    }

    /// Connect to the NFS server at `host` and resolve the export root.
    pub fn connect(host: &str) -> VfResult<NfsVecFs> {
        let nfs = NfsClient::connect(host).map_err(|e| VfError::from_rpc(e, 0))?;
        Ok(NfsVecFs {
            nfs,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: std::collections::HashMap::new(),
            merged_mode: MergedIoMode::Full,
        })
    }

    // -- private helpers ----------------------------------------------------

    /// Resolve `path` (absolute, or relative to the client cwd) to a handle,
    /// following intermediate symlinks but not a final symlink.
    fn resolve(&mut self, path: &str) -> VfResult<FileHandle> {
        self.resolve_path(&self.abs_path(path), false)
    }

    /// Resolve a root-relative path to a file handle, following symlinks in
    /// intermediate components (POSIX pathwalk) and, when `follow_final` is
    /// set, the final component too (for `stat`/`open` semantics).
    ///
    /// The fast path is a single deep-resolve compound; when the server
    /// reports `NFS4ERR_SYMLINK` mid-path, resolution falls back to a
    /// component-wise walk that follows each symlink with READLINK, splicing
    /// its target into the remaining path (hop-capped at 40).
    fn resolve_path(&mut self, root_rel: &str, follow_final: bool) -> VfResult<FileHandle> {
        let mut path = root_rel.to_string();
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
                        .map_err(|e| VfError::from_rpc(e, 0))?;
                    let mut off = 0;
                    if read_u32(&t, &mut off).unwrap_or(0) != nfs_ftype4_NF4LNK {
                        return Ok(fh);
                    }
                    let target = self
                        .nfs
                        .readlink(&fh)
                        .map_err(|e| VfError::from_rpc(e, 0))?;
                    path = Self::resolve_target(&path, &String::from_utf8_lossy(&target));
                    hops += 1;
                }
                Err(e) if e.status == nfsstat4_NFS4ERR_SYMLINK => {
                    // An intermediate component is a symlink: walk component
                    // by component, following each link we encounter.
                    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
                    if comps.is_empty() {
                        return Err(VfError::from_rpc(e, 0));
                    }
                    let mut cur_fh = self.nfs.root().clone();
                    let mut consumed = String::new();
                    let mut followed = false;
                    for (i, comp) in comps.iter().enumerate() {
                        let is_last = i + 1 == comps.len();
                        let (child, ftype) = self
                            .nfs
                            .lookup_getattr(&cur_fh, comp)
                            .map_err(|e| VfError::from_rpc(e, 0))?;
                        let full_comp = if consumed.is_empty() {
                            (*comp).to_string()
                        } else {
                            format!("{}/{}", consumed, comp)
                        };
                        if ftype == nfs_ftype4_NF4LNK && (follow_final || !is_last) {
                            let target = self
                                .nfs
                                .readlink(&child)
                                .map_err(|e| VfError::from_rpc(e, 0))?;
                            let rest = comps[i + 1..].join("/");
                            let base =
                                Self::resolve_target(&full_comp, &String::from_utf8_lossy(&target));
                            path = if rest.is_empty() {
                                base
                            } else {
                                format!("{}/{}", base, rest)
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
                        return Err(VfError::from_rpc(e, 0));
                    }
                    hops += 1;
                }
                Err(e) => return Err(VfError::from_rpc(e, 0)),
            }
        }
    }

    /// Map fcntl-style flags to an NFSv4 share access mode.
    fn flags_to_access(flags: i32) -> u32 {
        use libc::{O_RDWR, O_WRONLY};
        if flags & O_RDWR != 0 {
            OPEN4_SHARE_ACCESS_BOTH
        } else if flags & O_WRONLY != 0 {
            OPEN4_SHARE_ACCESS_WRITE
        } else {
            OPEN4_SHARE_ACCESS_READ
        }
    }

    fn open_impl(
        &mut self,
        dir: &str,
        name: &str,
        access: u32,
        create: bool,
        excl: bool,
    ) -> VfResult<(FileHandle, stateid4)> {
        let dirfh = self.resolve_path(dir, true).map_err(|e| e.with_index(0))?;
        let mode = match (create, excl) {
            (false, _) => OpenCreate::NoCreate,
            (true, true) => OpenCreate::Exclusive,
            (true, false) => OpenCreate::Guarded,
        };
        self.nfs
            .open(&dirfh, name, access, mode)
            .map_err(|e| VfError::from_rpc(e, 0))
    }

    /// The merged (single-compound) openv: resolve each parent once, then
    /// OPEN + GETFH (+ SETATTR for truncate / exclusive-create mode) per
    /// file. UNCHECKED creates carry the mode in their createattrs, so no
    /// existence probe is needed.
    fn openv_merged(
        &mut self,
        paths: &[&str],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<Vec<VfFile>> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        let mut ops = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let create = if flags[i] & O_CREAT != 0 {
                if flags[i] & O_EXCL != 0 {
                    crate::client::OpenCreate::Exclusive
                } else {
                    crate::client::OpenCreate::Unchecked
                }
            } else {
                crate::client::OpenCreate::NoCreate
            };
            ops.push(crate::client::PathOpenOp {
                path: self.abs_path(p),
                access: Self::flags_to_access(flags[i]),
                create,
                mode: Some(modes[i] & 0o7777),
                truncate: flags[i] & O_TRUNC != 0,
            });
        }
        let outcome = self
            .nfs
            .openv_path_compound(&ops)
            .map_err(|e| VfError::from_rpc(e, 0))?;
        if let Some((_i, _st)) = outcome.failed {
            return self.openv_phased(paths, flags, modes);
        }
        let mut out = Vec::with_capacity(paths.len());
        for (i, o) in outcome.opened.iter().enumerate() {
            let (fh, stateid) = o.clone().expect("completed open");
            self.next_fd += 1;
            self.open_files.insert(
                self.next_fd,
                OpenFile {
                    fh,
                    stateid,
                    cur_offset: 0,
                    append: flags[i] & O_APPEND != 0,
                },
            );
            out.push(VfFile::from_fd(self.next_fd));
        }
        Ok(out)
    }

    /// The legacy phased openv (batched existence probe + OPENs + SETATTRs).
    fn openv_phased(
        &mut self,
        paths: &[&str],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<Vec<VfFile>> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        // Batched existence probe for O_CREAT-without-O_EXCL entries, so the
        // mode is only applied to files this call actually creates.
        let mut dir_cache: std::collections::HashMap<String, FileHandle> =
            std::collections::HashMap::new();
        let mut probe: Vec<(usize, FileHandle, String)> = Vec::new();
        let mut entries: Vec<(usize, FileHandle, String, u32, bool, bool)> = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let full = self.abs_path(p);
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e))?;
            let dirfh = match dir_cache.get(dir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self.resolve_path(dir, true).map_err(|e| e.with_index(i))?;
                    dir_cache.insert(dir.to_string(), fh.clone());
                    fh
                }
            };
            let access = Self::flags_to_access(flags[i]);
            let create = flags[i] & O_CREAT != 0;
            let excl = flags[i] & O_EXCL != 0;
            if create && !excl {
                probe.push((i, dirfh.clone(), name.to_string()));
            }
            entries.push((
                i,
                dirfh,
                name.to_string(),
                access,
                excl,
                flags[i] & O_TRUNC != 0,
            ));
        }

        let mut created = vec![false; paths.len()];
        if !probe.is_empty() {
            let lookup: Vec<(FileHandle, String)> = probe
                .iter()
                .map(|(_, dir, name)| (dir.clone(), name.clone()))
                .collect();
            let results = self
                .nfs
                .lookup_many(&lookup)
                .map_err(|e| VfError::from_rpc(e, 0))?;
            for ((orig, _, _), r) in probe.iter().zip(results) {
                match r {
                    Ok(_) => {}
                    Err(status) if status == nfsstat4_NFS4ERR_NOENT => created[*orig] = true,
                    Err(status) => return Err(VfError::failure(*orig, status)),
                }
            }
        }
        for (i, _, _, _, excl, _) in &entries {
            if *excl {
                created[*i] = true;
            }
        }

        let opens: Vec<crate::client::OpenOp> = entries
            .iter()
            .map(|(i, dir, name, access, excl, _)| crate::client::OpenOp {
                dir: dir.clone(),
                name: name.clone(),
                access: *access,
                // Create only when the file is (believed) absent: kernel nfsd
                // rejects CREATE_GUARDED on existing files with EXIST.
                create: if created[*i] {
                    if *excl {
                        OpenCreate::Exclusive
                    } else {
                        OpenCreate::Guarded
                    }
                } else {
                    OpenCreate::NoCreate
                },
            })
            .collect();

        let results = self
            .nfs
            .open_many(&opens)
            .map_err(VfError::from_rpc_indexed)?;

        // Apply per-file mode / O_TRUNC with a single SETATTR compound.
        let mut setattr_ops = Vec::new();
        for (i, (fh, _)) in results.iter().enumerate() {
            if created[i] {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: Some(modes[i] & 0o7777),
                    size: None,
                });
            }
            if entries[i].5 {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: None,
                    size: Some(0),
                });
            }
        }
        if !setattr_ops.is_empty() {
            self.nfs
                .setattr_many(&setattr_ops)
                .map_err(VfError::from_rpc_indexed)?;
        }

        let mut out = Vec::with_capacity(results.len());
        for (i, (fh, stateid)) in results.into_iter().enumerate() {
            self.next_fd += 1;
            let open = OpenFile {
                fh,
                stateid,
                cur_offset: 0,
                append: flags[i] & O_APPEND != 0,
            };
            self.open_files.insert(self.next_fd, open);
            out.push(VfFile::from_fd(self.next_fd));
        }
        Ok(out)
    }

    /// Open every path-based file in `files` in one batched OPEN compound,
    /// returning, per original index, the temporary descriptor (or `None` for
    /// inputs that already were descriptors). The caller must close them via
    /// [`close_tmp`](Self::close_tmp). Errors are attributed to the original
    /// op index.
    fn open_path_batch(
        &mut self,
        files: &[&VfFile],
        creation: &[bool],
        for_write: bool,
    ) -> VfResult<Vec<Option<i32>>> {
        // Resolve each parent directory once per distinct dir, then look up
        // every final component in one tolerant batch (which also reports the
        // type, so symlinks can be followed only when actually present).
        let mut dir_cache: std::collections::HashMap<String, FileHandle> =
            std::collections::HashMap::new();
        let mut lookups: Vec<(usize, FileHandle, String)> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            if f.is_descriptor() {
                continue;
            }
            match f {
                VfFile::Cwd => return Err(VfError::failure(i, ERR_ISDIR)),
                VfFile::Saved => return Err(VfError::failure(i, ERR_NOENT)),
                VfFile::Path { .. } | VfFile::CwdPath(_) => {}
                VfFile::Descriptor(_) => unreachable!(),
            }
            let full = self.vf_path(f).map_err(|e| e.with_index(i))?;
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e))?;
            let dirfh = match dir_cache.get(dir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self.resolve_path(dir, true).map_err(|e| e.with_index(i))?;
                    dir_cache.insert(dir.to_string(), fh.clone());
                    fh
                }
            };
            lookups.push((i, dirfh, name.to_string()));
        }
        if lookups.is_empty() {
            return Ok(vec![None; files.len()]);
        }
        let probe: Vec<(FileHandle, String)> = lookups
            .iter()
            .map(|(_, dir, name)| (dir.clone(), name.clone()))
            .collect();
        let results = self
            .nfs
            .lookup_getattr_many(&probe)
            .map_err(|e| VfError::from_rpc(e, 0))?;
        let access = if for_write {
            OPEN4_SHARE_ACCESS_BOTH
        } else {
            OPEN4_SHARE_ACCESS_READ
        };
        let mut opens: Vec<(usize, crate::client::OpenOp)> = Vec::new();
        let mut subset: Vec<usize> = Vec::new();
        for ((orig, dirfh, name), r) in lookups.iter().zip(results) {
            match r {
                Ok((_fh, ftype)) if ftype == nfs_ftype4_NF4LNK => {
                    // OPEN cannot target a symlink: follow the chain (creation
                    // follows dangling links and creates the target).
                    let full = self
                        .follow_target_path(&self.vf_path(files[*orig])?)
                        .map_err(|e| e.with_index(*orig))?;
                    let (dir, name2) = split_path(&full).map_err(|e| VfError::failure(*orig, e))?;
                    let dirfh2 = self
                        .resolve_path(dir, true)
                        .map_err(|e| e.with_index(*orig))?;
                    let create = match self.nfs.lookup_getattr(&dirfh2, name2) {
                        Ok(_) => crate::client::OpenCreate::NoCreate,
                        Err(e) if e.status == nfsstat4_NFS4ERR_NOENT => {
                            if !creation[*orig] {
                                return Err(VfError::failure(*orig, ERR_NOENT));
                            }
                            crate::client::OpenCreate::Guarded
                        }
                        Err(e) => return Err(VfError::from_rpc(e, *orig)),
                    };
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh2,
                            name: name2.to_string(),
                            access,
                            create,
                        },
                    ));
                    subset.push(*orig);
                }
                Ok(_) => {
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh.clone(),
                            name: name.clone(),
                            access,
                            create: crate::client::OpenCreate::NoCreate,
                        },
                    ));
                    subset.push(*orig);
                }
                Err(status) if status == nfsstat4_NFS4ERR_NOENT => {
                    if !creation[*orig] {
                        return Err(VfError::failure(*orig, ERR_NOENT));
                    }
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh.clone(),
                            name: name.clone(),
                            access,
                            create: crate::client::OpenCreate::Guarded,
                        },
                    ));
                    subset.push(*orig);
                }
                Err(status) => return Err(VfError::failure(*orig, status)),
            }
        }
        if opens.is_empty() {
            return Ok(vec![None; files.len()]);
        }
        let open_ops: Vec<crate::client::OpenOp> = opens
            .iter()
            .map(|(_, op)| crate::client::OpenOp {
                dir: op.dir.clone(),
                name: op.name.clone(),
                access: op.access,
                create: op.create,
            })
            .collect();
        let results = self.nfs.open_many_path(&open_ops).map_err(|e| {
            let e = VfError::from_rpc_indexed(e);
            // open_many's index is relative to the path-only subset.
            match subset.get(e.index()) {
                Some(orig) => e.with_index(*orig),
                None => e,
            }
        })?;
        let mut tmp = vec![None; files.len()];
        for (orig, (fh, stateid)) in subset.iter().zip(results) {
            self.next_fd += 1;
            self.open_files.insert(
                self.next_fd,
                OpenFile {
                    fh,
                    stateid,
                    cur_offset: 0,
                    append: false,
                },
            );
            tmp[*orig] = Some(self.next_fd);
        }
        Ok(tmp)
    }

    /// Close and forget temporary descriptors opened by
    /// [`open_path_batch`](Self::open_path_batch). Best-effort: never fails
    /// the caller (the close failure would mask the real error).
    fn close_tmp(&mut self, tmp: &[Option<i32>]) {
        let closes: Vec<crate::client::CloseOp> = tmp
            .iter()
            .filter_map(|fd| {
                let fd = (*fd)?;
                self.open_files.remove(&fd).map(|o| crate::client::CloseOp {
                    fh: o.fh,
                    stateid: o.stateid,
                })
            })
            .collect();
        if !closes.is_empty() {
            let _ = self.nfs.close_many_path(&closes);
        }
    }

    /// Batched readv for open (descriptor) ops: one compound per chunk of
    /// files, each carrying `[PUTFH, READ]` for every op.
    fn readv_batch(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let mut ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        for (i, op) in reads.iter().enumerate() {
            let off = self
                .resolve_offset(&op.file, op.offset)
                .map_err(|e| e.with_index(i))?;
            let o = self
                .open_files
                .get(&op.file.fd().unwrap())
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            ops.push(crate::client::ReadOp {
                fh: o.fh,
                stateid: o.stateid,
                offset: off,
                count: op.length.min(u32::MAX as usize) as u32,
            });
            offsets.push(off);
        }
        let results = self.nfs.readv(&ops).map_err(VfError::from_rpc_indexed)?;
        let mut out = Vec::with_capacity(reads.len());
        for ((op, (data, eof)), off) in reads.iter().zip(results).zip(offsets) {
            self.advance_offset(&op.file, off + data.len() as u64);
            out.push(ReadResult {
                file: op.file.clone(),
                offset: off,
                data,
                eof,
            });
        }
        Ok(out)
    }

    /// Resolve a [`VfOffset`] to a concrete file offset.
    fn resolve_offset(&mut self, file: &VfFile, off: VfOffset) -> VfResult<u64> {
        match off {
            VfOffset::At(offset) => Ok(offset),
            VfOffset::Cur => match file.fd() {
                Some(fd) => Ok(self.open_files.get(&fd).map(|o| o.cur_offset).unwrap_or(0)),
                None => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
            },
            VfOffset::End => {
                let fh = self.resolve_tcfile(file, true)?;
                self.file_size(&fh)
            }
        }
    }

    /// The offset a write should use: like [`resolve_offset`](Self::resolve_offset),
    /// but descriptors opened with `O_APPEND` always write at the end of the
    /// file (one extra size query per write).
    fn write_offset(&mut self, file: &VfFile, off: VfOffset) -> VfResult<u64> {
        let offset = self.resolve_offset(file, off)?;
        let append_fh = match file.fd() {
            Some(fd) => self
                .open_files
                .get(&fd)
                .filter(|o| o.append)
                .map(|o| o.fh.clone()),
            None => None,
        };
        match append_fh {
            Some(fh) => self.file_size(&fh),
            None => Ok(offset),
        }
    }

    /// Record the new read/write offset of an open (descriptor) file.
    fn advance_offset(&mut self, file: &VfFile, new_offset: u64) {
        if let Some(fd) = file.fd()
            && let Some(o) = self.open_files.get_mut(&fd)
        {
            o.cur_offset = new_offset;
        }
    }

    /// Batched writev for open (descriptor) ops.
    fn writev_batch(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        let mut ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        for (i, op) in writes.iter().enumerate() {
            let off = self
                .write_offset(&op.file, op.offset)
                .map_err(|e| e.with_index(i))?;
            let o = self
                .open_files
                .get(&op.file.fd().unwrap())
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            ops.push(crate::client::WriteOp {
                fh: o.fh,
                stateid: o.stateid,
                offset: off,
                data: op.data.clone(),
            });
            offsets.push(off);
        }
        let results = self.nfs.writev(&ops).map_err(VfError::from_rpc_indexed)?;
        let mut out = Vec::with_capacity(writes.len());
        for ((op, (n, committed)), off) in writes.iter().zip(results).zip(offsets) {
            self.advance_offset(&op.file, off + n as u64);
            out.push(WriteResult {
                file: op.file.clone(),
                offset: off,
                written: n as usize,
                stable: committed == stable_how4_FILE_SYNC4,
            });
        }
        Ok(out)
    }

    /// The size in bytes of `fh`.
    fn file_size(&mut self, fh: &FileHandle) -> VfResult<u64> {
        let list = self
            .nfs
            .getattr(fh, &[FATTR4_SIZE])
            .map_err(|e| VfError::from_rpc(e, 0))?;
        let mut off = 0;
        read_u64(&list, &mut off)
    }

    fn resolve_tcfile(&mut self, f: &VfFile, follow: bool) -> VfResult<FileHandle> {
        match f {
            VfFile::Descriptor(fd) => self
                .open_files
                .get(fd)
                .map(|o| o.fh.clone())
                .ok_or_else(|| VfError::failure(0, ERR_EBADF)),
            VfFile::Path { .. } | VfFile::Cwd | VfFile::CwdPath(_) => {
                let path = self.vf_path(f)?;
                if follow {
                    self.resolve_follow(&path)
                } else {
                    self.resolve_path(&path, false)
                }
            }
            VfFile::Saved => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        }
    }

    /// Resolve a symlink target against the link's parent directory (POSIX
    /// semantics), returning a normalized root-relative path. Absolute
    /// targets resolve from the export root.
    fn resolve_target(link_path: &str, target: &str) -> String {
        if let Some(t) = target.strip_prefix('/') {
            normalize_root_relative(t)
        } else {
            let parent = match link_path.rfind('/') {
                Some(idx) => &link_path[..=idx],
                None => "",
            };
            normalize_root_relative(&format!("{}{}", parent, target))
        }
    }

    /// Follow a chain of symlinks at the end of `root_rel` (a root-relative
    /// path), returning the final root-relative path, with a hop limit to
    /// break cycles. A missing final target returns the current path so
    /// creation-style callers can create through a dangling link; other
    /// callers will fail with NOENT when they open it. Intermediate symlinks
    /// are followed via [`resolve_path`](Self::resolve_path).
    fn follow_target_path(&mut self, root_rel: &str) -> VfResult<String> {
        let mut current = root_rel.to_string();
        let mut hops = 0usize;
        loop {
            let abs = format!("/{}", current);
            let st = match self.lstat(&abs) {
                Ok(s) => s,
                Err(e) if e.err_no() == ERR_NOENT => return Ok(current),
                Err(e) => return Err(e),
            };
            if st.ftype != VfType::Symlink {
                return Ok(current);
            }
            if hops >= 40 {
                return Err(VfError::failure(0, nfsstat4_NFS4ERR_IO)); // symlink loop
            }
            let target = self.readlink(&abs)?;
            current = Self::resolve_target(&current, &String::from_utf8_lossy(&target));
            hops += 1;
        }
    }

    /// Resolve `path` and follow a chain of symlinks at the end of it (for
    /// `stat` semantics).
    fn resolve_follow(&mut self, path: &str) -> VfResult<FileHandle> {
        self.resolve_path(path, true)
    }

    fn listdir_rec(
        &mut self,
        dir: &str,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        out: &mut Vec<VfAttrs>,
    ) -> VfRes {
        let reached_limit = |out: &Vec<VfAttrs>| max_count != 0 && out.len() >= max_count;
        if reached_limit(out) {
            return Ok(());
        }
        // A directory argument may itself be a symlink to a directory.
        let dirfh = self.resolve_path(&self.abs_path(dir), true)?;
        let ids = request_mask_to_attr_list(&masks);
        let mut cookie = 0u64;
        loop {
            let entries = self
                .nfs
                .readdir(&dirfh, cookie, &ids)
                .map_err(|e| VfError::from_rpc(e, 0))?;
            if entries.is_empty() {
                break;
            }
            for e in &entries {
                if reached_limit(out) {
                    return Ok(());
                }
                let path = join_path(dir.trim_matches('/'), &e.name);
                let mut a = VfAttrs {
                    file: VfFile::from_path(&format!("/{}", path)),
                    masks,
                    ..VfAttrs::default()
                };
                // Attributes come back inline from READDIR for the requested ids.
                let vals = parse_attr_list(&ids, &e.attrs).unwrap_or_default();
                apply_attrs(&mut a, &vals);
                let is_dir = a.ftype == VfType::Directory;
                out.push(a);
                if recursive && is_dir {
                    self.listdir_rec(&path, masks, max_count, recursive, out)?;
                }
            }
            cookie = entries.last().unwrap().cookie;
            if cookie == 0 {
                break;
            }
        }
        Ok(())
    }

    fn rm_one(&mut self, path: &str, recursive: bool) -> VfResult<()> {
        let ft = self.file_type(path).unwrap_or(VfType::Regular);
        if ft == VfType::Directory && recursive {
            let entries = self.listdir(path, AttrMask::default(), usize::MAX, false)?;
            for e in entries {
                let p = e.file.path().unwrap().to_string_lossy().to_string();
                self.rm_one(&p, true)?;
            }
        }
        self.unlink(path)
    }

    /// Read every page of directory `fh`, returning its entries as `VfAttrs`.
    fn readdir_all(
        &mut self,
        fh: &FileHandle,
        dir_path: &str,
        masks: &AttrMask,
    ) -> VfResult<Vec<VfAttrs>> {
        let ids = request_mask_to_attr_list(masks);
        let mut all: Vec<crate::client::DirEntry> = Vec::new();
        let mut cookie = 0u64;
        loop {
            let page = self
                .nfs
                .readdir(fh, cookie, &ids)
                .map_err(|e| VfError::from_rpc(e, 0))?;
            cookie = page.last().map(|d| d.cookie).unwrap_or(0);
            all.extend(page);
            if cookie == 0 {
                break;
            }
        }
        Ok(all
            .iter()
            .map(|de| Self::dir_entry_to_attrs(dir_path, masks, &ids, de))
            .collect())
    }

    /// Convert a raw READDIR entry into a `VfAttrs` with a full path. `ids`
    /// must be the attribute list that was requested for the READDIR (in the
    /// same order), so the reply values can be decoded positionally.
    fn dir_entry_to_attrs(
        parent_path: &str,
        masks: &AttrMask,
        ids: &[u32],
        de: &crate::client::DirEntry,
    ) -> VfAttrs {
        let path = join_path(parent_path.trim_matches('/'), &de.name);
        let mut a = VfAttrs {
            file: VfFile::from_path(&format!("/{}", path)),
            masks: *masks,
            ..VfAttrs::default()
        };
        let vals = parse_attr_list(ids, &de.attrs).unwrap_or_default();
        apply_attrs(&mut a, &vals);
        a
    }

    fn copy_extent(
        &mut self,
        src_root_rel: &str,
        dst_root_rel: &str,
        p: &ExtentPair,
    ) -> VfResult<()> {
        let (sdir, sname) = split_path(src_root_rel).map_err(|e| VfError::failure(0, e))?;
        let (ddir, dname) = split_path(dst_root_rel).map_err(|e| VfError::failure(0, e))?;
        let sdirfh = self.resolve_path(sdir, true).map_err(|e| e.with_index(0))?;
        let ddirfh = self.resolve_path(ddir, true).map_err(|e| e.with_index(0))?;
        let (sfh, ssid) = self
            .nfs
            .open_path(
                &sdirfh,
                sname,
                OPEN4_SHARE_ACCESS_READ,
                crate::client::OpenCreate::NoCreate,
            )
            .map_err(|e| VfError::from_rpc(e, 0))?;
        // Kernel nfsd rejects CREATE_GUARDED on an existing file, so open the
        // destination without create and fall back to create only on NOENT.
        let (dfh, dsid) = match self
            .nfs
            .open_path(
                &ddirfh,
                dname,
                OPEN4_SHARE_ACCESS_WRITE,
                crate::client::OpenCreate::NoCreate,
            )
            .map_err(|e| VfError::from_rpc(e, 0))
        {
            Ok(x) => x,
            Err(e) if e.err_no() == ERR_NOENT => self
                .nfs
                .open_path(
                    &ddirfh,
                    dname,
                    OPEN4_SHARE_ACCESS_WRITE,
                    crate::client::OpenCreate::Guarded,
                )
                .map_err(|e| VfError::from_rpc(e, 0))?,
            Err(e) => return Err(e),
        };

        let mut so = p.src_offset;
        let mut doff = p.dst_offset;
        let mut copied: u64 = 0;
        let result = loop {
            if let Some(length) = p.length
                && copied >= length
            {
                break Ok(());
            }
            let remaining = p.length.map_or(u64::MAX, |l| l - copied);
            let chunk_len = remaining.min(1 << 20) as u32;
            let chunk = match self.nfs.read(&sfh, &ssid, so, chunk_len) {
                Ok((c, _)) => c,
                Err(e) => break Err(VfError::from_rpc(e, 0)),
            };
            if chunk.is_empty() {
                break Ok(()); // EOF
            }
            let n = match self.nfs.write(&dfh, &dsid, doff, &chunk) {
                Ok((n, _)) => n as u64,
                Err(e) => break Err(VfError::from_rpc(e, 0)),
            };
            so += n;
            doff += n;
            copied += n;
        };
        let _ = self.nfs.close_path(&sfh, &ssid);
        let _ = self.nfs.close_path(&dfh, &dsid);
        if result.is_ok() {
            // Truncate any stale tail beyond what was copied (cp semantics).
            self.nfs
                .setattr(&dfh, None, Some(doff))
                .map_err(|e| VfError::from_rpc(e, 0))?;
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Merged path I/O plumbing (single-compound readv/writev)
// ---------------------------------------------------------------------------

impl NfsVecFs {
    /// The legacy phased path-based readv (resolve + probe + open + read +
    /// close compounds).
    fn readv_path_fallback(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        let mut tmp: Vec<Option<i32>> = vec![None; reads.len()];
        let mut files = Vec::with_capacity(reads.len());
        let mut needs_open = false;
        for (i, r) in reads.iter().enumerate() {
            files.push(&r.file);
            if !r.file.is_descriptor() {
                needs_open = true;
                if r.offset == VfOffset::Cur {
                    // "current position" only exists for open descriptors.
                    return Err(VfError::failure(i, ERR_INVAL));
                }
            }
        }
        if needs_open {
            tmp = self.open_path_batch(&files, &vec![false; reads.len()], false)?;
        }
        let remapped: Vec<ReadOp> = reads
            .iter()
            .enumerate()
            .map(|(i, r)| ReadOp {
                file: match tmp[i] {
                    Some(fd) => VfFile::from_fd(fd),
                    None => r.file.clone(),
                },
                offset: r.offset,
                length: r.length,
            })
            .collect();
        let result = self.readv_batch(&remapped);
        self.close_tmp(&tmp);
        let mut out = result?;
        for (i, r) in out.iter_mut().enumerate() {
            r.file = reads[i].file.clone();
        }
        Ok(out)
    }

    /// One open+read compound, then a separate close compound with the real
    /// stateids (portable fallback).
    fn readv_path_openwrite(
        &mut self,
        reads: &[ReadOp],
        path_ops: &[crate::client::PathReadOp],
        offsets: &[u64],
    ) -> VfResult<Vec<ReadResult>> {
        match self.nfs.readv_path_compound(path_ops, false) {
            Ok(outcome) => {
                if let Some((_i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    return self.readv_path_fallback(reads);
                }
                self.close_path_opens(&outcome.opened);
                Ok(self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof))
            }
            Err(_) => self.readv_path_fallback(reads),
        }
    }

    /// One compound for the whole batch, including the special-stateid
    /// CLOSE. Downgrades to the open+close form if the server rejects that
    /// CLOSE.
    fn readv_path_full(
        &mut self,
        reads: &[ReadOp],
        path_ops: &[crate::client::PathReadOp],
        offsets: &[u64],
    ) -> VfResult<Vec<ReadResult>> {
        match self.nfs.readv_path_compound(path_ops, true) {
            Ok(outcome) => {
                if let Some((_i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        // The special stateid itself is rejected: disable the
                        // merged path entirely.
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    return self.readv_path_fallback(reads);
                }
                if let Some(_st) = outcome.close_failed {
                    self.merged_mode = MergedIoMode::OpenWrite;
                    return self.readv_path_openwrite(reads, path_ops, offsets);
                }
                Ok(self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof))
            }
            Err(_) => self.readv_path_fallback(reads),
        }
    }

    fn assemble_reads(
        &self,
        reads: &[ReadOp],
        offsets: &[u64],
        data: &[Option<Vec<u8>>],
        eof: &[Option<bool>],
    ) -> Vec<ReadResult> {
        reads
            .iter()
            .enumerate()
            .map(|(i, r)| ReadResult {
                file: r.file.clone(),
                offset: offsets[i],
                data: data[i].clone().unwrap_or_default(),
                eof: eof[i].unwrap_or(false),
            })
            .collect()
    }

    /// The legacy phased path-based writev.
    fn writev_path_fallback(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        let mut tmp: Vec<Option<i32>> = vec![None; writes.len()];
        let mut files = Vec::with_capacity(writes.len());
        let mut creation = Vec::with_capacity(writes.len());
        let mut needs_open = false;
        for (i, w) in writes.iter().enumerate() {
            files.push(&w.file);
            creation.push(w.creation);
            if !w.file.is_descriptor() {
                needs_open = true;
                if w.offset == VfOffset::Cur {
                    return Err(VfError::failure(i, ERR_INVAL));
                }
            }
        }
        if needs_open {
            tmp = self.open_path_batch(&files, &creation, true)?;
        }
        let remapped: Vec<WriteOp> = writes
            .iter()
            .enumerate()
            .map(|(i, w)| WriteOp {
                file: match tmp[i] {
                    Some(fd) => VfFile::from_fd(fd),
                    None => w.file.clone(),
                },
                offset: w.offset,
                data: w.data.clone(),
                creation: false,
            })
            .collect();
        let result = self.writev_batch(&remapped);
        self.close_tmp(&tmp);
        let mut out = result?;
        for (i, r) in out.iter_mut().enumerate() {
            r.file = writes[i].file.clone();
        }
        Ok(out)
    }

    fn writev_path_openwrite(
        &mut self,
        writes: &[WriteOp],
        path_ops: &[crate::client::PathWriteOp],
        offsets: &[u64],
    ) -> VfResult<Vec<WriteResult>> {
        match self.nfs.writev_path_compound(path_ops, false) {
            Ok(outcome) => {
                if let Some((_i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    return self.writev_path_fallback(writes);
                }
                self.close_path_opens(&outcome.opened);
                Ok(self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed))
            }
            Err(_) => self.writev_path_fallback(writes),
        }
    }

    fn writev_path_full(
        &mut self,
        writes: &[WriteOp],
        path_ops: &[crate::client::PathWriteOp],
        offsets: &[u64],
    ) -> VfResult<Vec<WriteResult>> {
        match self.nfs.writev_path_compound(path_ops, true) {
            Ok(outcome) => {
                if let Some((_i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    return self.writev_path_fallback(writes);
                }
                if let Some(_st) = outcome.close_failed {
                    self.merged_mode = MergedIoMode::OpenWrite;
                    return self.writev_path_openwrite(writes, path_ops, offsets);
                }
                Ok(self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed))
            }
            Err(_) => self.writev_path_fallback(writes),
        }
    }

    fn assemble_writes(
        &self,
        writes: &[WriteOp],
        offsets: &[u64],
        counts: &[Option<u32>],
        committed: &[Option<u32>],
    ) -> Vec<WriteResult> {
        writes
            .iter()
            .enumerate()
            .map(|(i, w)| WriteResult {
                file: w.file.clone(),
                offset: offsets[i],
                written: counts[i].unwrap_or(0) as usize,
                stable: committed[i].unwrap_or(0) == stable_how4_FILE_SYNC4,
            })
            .collect()
    }

    /// Best-effort close of stateids opened by a merged path compound.
    fn close_path_opens(&mut self, opens: &[(crate::client::FileHandle, stateid4)]) {
        if opens.is_empty() {
            return;
        }
        let ops: Vec<crate::client::CloseOp> = opens
            .iter()
            .map(|(fh, sid)| crate::client::CloseOp {
                fh: fh.clone(),
                stateid: *sid,
            })
            .collect();
        let _ = self.nfs.close_many_path(&ops);
    }

    /// The legacy phased renamev (cached parent resolution + rename_many).
    fn renamev_phased(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        let mut src_cache: std::collections::HashMap<String, FileHandle> =
            std::collections::HashMap::new();
        let mut dst_cache: std::collections::HashMap<String, FileHandle> =
            std::collections::HashMap::new();
        let mut ops = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let s = self.vf_path(src).map_err(|e| e.with_index(i))?;
            let d = self.vf_path(dst).map_err(|e| e.with_index(i))?;
            let (sdir, sname) = split_path(&s).map_err(|e| VfError::failure(i, e))?;
            let (ddir, dname) = split_path(&d).map_err(|e| VfError::failure(i, e))?;
            let sdirfh = match src_cache.get(sdir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self.resolve_path(sdir, true).map_err(|e| e.with_index(i))?;
                    src_cache.insert(sdir.to_string(), fh.clone());
                    fh
                }
            };
            let ddirfh = match dst_cache.get(ddir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self.resolve_path(ddir, true).map_err(|e| e.with_index(i))?;
                    dst_cache.insert(ddir.to_string(), fh.clone());
                    fh
                }
            };
            ops.push(crate::client::RenameOp {
                srcdir: sdirfh,
                oldname: sname.to_string(),
                dstdir: ddirfh,
                newname: dname.to_string(),
            });
        }
        self.nfs
            .rename_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    /// The legacy phased removev (grouped per parent + remove_many).
    fn removev_phased(&mut self, files: &[VfFile]) -> VfRes {
        use std::collections::BTreeMap;
        // Group by parent directory to batch REMOVEs.
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            let path = self.vf_path(f).map_err(|e| e.with_index(i))?;
            let (dir, name) = split_path(&path).map_err(|e| VfError::failure(i, e))?;
            groups
                .entry(dir.to_string())
                .or_default()
                .push(name.to_string());
        }
        for (dir, names) in &groups {
            let dirfh = self.resolve_path(dir, true).map_err(|e| e.with_index(0))?;
            let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            self.nfs
                .remove_many(&dirfh, &refs)
                .map_err(VfError::from_rpc_indexed)?;
        }
        Ok(())
    }

    /// Whether an NFS status indicates the special stateid was rejected
    /// (rather than a per-file failure), so the merged path must be disabled.
    fn is_stateid_error(status: u32) -> bool {
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

impl VecFs for NfsVecFs {
    fn abs_path(&self, path: &str) -> String {
        let root_rel = if path.starts_with('/') {
            path.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(path).to_string_lossy().to_string()
        };
        normalize_root_relative(&root_rel)
    }

    fn open_by_path(
        &mut self,
        base: VfPathBase,
        pathname: &str,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        let full = match base {
            VfPathBase::Abs => pathname.trim_start_matches('/').to_string(),
            VfPathBase::Cwd => self.cwd.join(pathname).to_string_lossy().to_string(),
        };
        let full = normalize_root_relative(&full);
        // Follow a final symlink chain so O_CREAT creates the target
        // (POSIX semantics); OPEN cannot target a symlink directly.
        let full = self.follow_target_path(&full)?;
        let (dir, name) = split_path(&full).map_err(|e| VfError::failure(0, e))?;
        let access = Self::flags_to_access(flags);
        let create = flags & O_CREAT != 0;
        let excl = flags & O_EXCL != 0;
        // The mode only applies when O_CREAT actually creates the file
        // (POSIX ignores it for existing files).
        let created = if create {
            if excl {
                true
            } else {
                match self.nfs.resolve(&full) {
                    Ok(_) => false,
                    Err(e) if e.status == nfsstat4_NFS4ERR_NOENT => true,
                    Err(e) => return Err(VfError::from_rpc(e, 0)),
                }
            }
        } else {
            false
        };
        // Open with NoCreate when the file already exists (kernel nfsd
        // rejects CREATE_GUARDED on existing files with NFS4ERR_EXIST).
        let (fh, stateid) = self.open_impl(dir, name, access, created, excl)?;
        if created {
            self.nfs
                .setattr(&fh, Some(mode & 0o7777), None)
                .map_err(|e| VfError::from_rpc(e, 0))?;
        }
        if flags & O_TRUNC != 0 {
            self.nfs
                .setattr(&fh, None, Some(0))
                .map_err(|e| VfError::from_rpc(e, 0))?;
        }
        self.next_fd += 1;
        let open = OpenFile {
            fh,
            stateid,
            cur_offset: 0,
            append: flags & O_APPEND != 0,
        };
        self.open_files.insert(self.next_fd, open);
        Ok(VfFile::from_fd(self.next_fd))
    }

    fn openv(&mut self, paths: &[&str], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        self.openv_merged(paths, flags, modes)
    }

    fn close(&mut self, tcf: &VfFile) -> VfResult<()> {
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let open = self
            .open_files
            .remove(&tcf.fd().unwrap())
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        self.nfs
            .close(&open.fh, &open.stateid)
            .map_err(|e| VfError::from_rpc(e, 0))
    }

    fn closev(&mut self, files: &[VfFile]) -> VfRes {
        let mut ops = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            if !f.is_descriptor() {
                return Err(VfError::failure(i, nfsstat4_NFS4ERR_INVAL));
            }
            let fd = f.fd().unwrap();
            let open = self
                .open_files
                .remove(&fd)
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            ops.push(crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            });
        }
        self.nfs.close_many(&ops).map_err(VfError::from_rpc_indexed)
    }

    fn chdir(&mut self, path: &str) -> VfResult<()> {
        let st = self.stat(path)?;
        if st.ftype != VfType::Directory {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        self.cwd = PathBuf::from(self.abs_path(path));
        Ok(())
    }

    fn getcwd(&self) -> String {
        format!("/{}", self.cwd.to_string_lossy())
    }

    fn readv(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        if reads.iter().all(|r| r.file.is_descriptor()) {
            return self.readv_batch(reads);
        }
        if reads.iter().any(|r| r.file.is_descriptor()) {
            // Mixed descriptor/path batches keep the phased path.
            return self.readv_path_fallback(reads);
        }
        // All path-based: resolve offsets and try the merged compound.
        let mut path_ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        for (i, r) in reads.iter().enumerate() {
            let path = self.vf_path(&r.file).map_err(|e| e.with_index(i))?;
            let off = match r.offset {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let fh = self.resolve_follow(&path).map_err(|e| e.with_index(i))?;
                    self.file_size(&fh).map_err(|e| e.with_index(i))?
                }
                VfOffset::Cur => return Err(VfError::failure(i, ERR_INVAL)),
            };
            offsets.push(off);
            path_ops.push(crate::client::PathReadOp {
                path,
                offset: off,
                count: r.length.min(u32::MAX as usize) as u32,
            });
        }
        match self.merged_mode {
            MergedIoMode::Off => self.readv_path_fallback(reads),
            MergedIoMode::OpenWrite => self.readv_path_openwrite(reads, &path_ops, &offsets),
            MergedIoMode::Full => self.readv_path_full(reads, &path_ops, &offsets),
        }
    }

    fn writev(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        if writes.iter().all(|w| w.file.is_descriptor()) {
            return self.writev_batch(writes);
        }
        if writes.iter().any(|w| w.file.is_descriptor()) {
            return self.writev_path_fallback(writes);
        }
        let mut path_ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        for (i, w) in writes.iter().enumerate() {
            let path = self.vf_path(&w.file).map_err(|e| e.with_index(i))?;
            let off = match w.offset {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let fh = self.resolve_follow(&path).map_err(|e| e.with_index(i))?;
                    self.file_size(&fh).map_err(|e| e.with_index(i))?
                }
                VfOffset::Cur => return Err(VfError::failure(i, ERR_INVAL)),
            };
            offsets.push(off);
            path_ops.push(crate::client::PathWriteOp {
                path,
                offset: off,
                data: w.data.clone(),
                create: w.creation,
            });
        }
        match self.merged_mode {
            MergedIoMode::Off => self.writev_path_fallback(writes),
            MergedIoMode::OpenWrite => self.writev_path_openwrite(writes, &path_ops, &offsets),
            MergedIoMode::Full => self.writev_path_full(writes, &path_ops, &offsets),
        }
    }

    fn fseek(&mut self, tcf: &VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64> {
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let cur = self
            .open_files
            .get(&tcf.fd().unwrap())
            .map(|o| o.cur_offset)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        let new = match whence {
            SeekFrom::Set => offset,
            SeekFrom::Cur => cur as i64 + offset,
            SeekFrom::End => {
                let fh = self
                    .open_files
                    .get(&tcf.fd().unwrap())
                    .map(|o| o.fh.clone())
                    .unwrap();
                let size = self.file_size(&fh)?;
                size as i64 + offset
            }
        };
        if new < 0 {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let new = new as u64;
        self.advance_offset(tcf, new);
        Ok(new as i64)
    }

    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        self.getattrsv_impl(attrs, true)
    }

    fn lgetattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        self.getattrsv_impl(attrs, false)
    }

    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.setattrsv_impl(attrs, true)
    }

    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        self.setattrsv_impl(attrs, false)
    }

    fn listdir(
        &mut self,
        dir: &str,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        let mut out = Vec::new();
        self.listdir_rec(dir, masks, max_count, recursive, &mut out)?;
        Ok(out)
    }

    /// Recursively enumerate `root`, returning directories in ls -R
    /// pre-order (a worklist: each directory is followed by its sorted
    /// subdirectories, then their subtrees). Listing is batched per level in
    /// compounds of up to `MAX_COMPOUND_OPS` operations
    /// (`[PUTFH parent, LOOKUP child, GETFH, READDIR]` per directory), with
    /// large directories' remaining READDIR pages drained in batched
    /// continuation compounds. `sort` orders each directory's entries (and
    /// hence the subdirectory visit order) exactly as the caller would.
    fn walk(
        &mut self,
        root: &str,
        masks: AttrMask,
        sort: &mut dyn FnMut(&str, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        let root_fh = self.resolve_path(&self.abs_path(root), true)?;
        let ids = request_mask_to_attr_list(&masks);
        let mut collected: std::collections::HashMap<String, Vec<VfAttrs>> =
            std::collections::HashMap::new();
        let root_attrs = self.readdir_all(&root_fh, root, &masks)?;
        let mut root_sorted = root_attrs.clone();
        sort(root, &mut root_sorted);
        collected.insert(root.to_string(), root_attrs);

        // Frontier of (parent handle, child directory path) to list next.
        let mut frontier: Vec<(FileHandle, String)> = root_sorted
            .iter()
            .filter(|e| e.ftype == VfType::Directory)
            .map(|e| {
                (
                    root_fh.clone(),
                    e.file.path().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect();

        while !frontier.is_empty() {
            // Resolve + list every frontier directory in batched compounds.
            let ops: Vec<(FileHandle, String)> = frontier
                .iter()
                .map(|(fh, p)| (fh.clone(), p.rsplit('/').next().unwrap().to_string()))
                .collect();
            let results = self
                .nfs
                .readdir_children(&ops, &ids)
                .map_err(VfError::from_rpc_indexed)?;

            // Drain remaining READDIR pages, batched across all directories.
            let mut accumulated: Vec<Vec<crate::client::DirEntry>> =
                results.iter().map(|r| r.entries.clone()).collect();
            let mut pending: Vec<(usize, FileHandle, u64)> = results
                .iter()
                .enumerate()
                .filter(|(_, r)| r.cookie != 0)
                .map(|(i, r)| (i, r.fh.clone(), r.cookie))
                .collect();
            while !pending.is_empty() {
                let ops: Vec<(FileHandle, u64)> =
                    pending.iter().map(|(_, fh, c)| (fh.clone(), *c)).collect();
                let cont = self
                    .nfs
                    .readdir_pages(&ops, &ids)
                    .map_err(VfError::from_rpc_indexed)?;
                let mut next_pending = Vec::new();
                for ((idx, _, _), (entries, cookie)) in pending.iter().zip(cont) {
                    accumulated[*idx].extend(entries);
                    if cookie != 0 {
                        next_pending.push((*idx, results[*idx].fh.clone(), cookie));
                    }
                }
                pending = next_pending;
            }

            // Record each directory's entries and seed the next level.
            let mut next_frontier: Vec<(FileHandle, String)> = Vec::new();
            for idx in 0..results.len() {
                let result = &results[idx];
                let path = frontier[idx].1.clone();
                let mut attrs = Vec::with_capacity(accumulated[idx].len());
                for de in &accumulated[idx] {
                    attrs.push(Self::dir_entry_to_attrs(&path, &masks, &ids, de));
                }
                let mut sorted = attrs.clone();
                sort(&path, &mut sorted);
                for a in &sorted {
                    if a.ftype == VfType::Directory {
                        next_frontier.push((
                            result.fh.clone(),
                            a.file.path().unwrap().to_string_lossy().into_owned(),
                        ));
                    }
                }
                collected.insert(path, attrs);
            }
            frontier = next_frontier;
        }

        // Emit in ls -R pre-order: a directory, then each of its subdirectories
        // (in sorted order) and their subtrees.
        let mut out = Vec::with_capacity(collected.len());
        let mut stack: Vec<String> = vec![root.to_string()];
        while let Some(dir) = stack.pop() {
            let entries = collected.remove(&dir).unwrap_or_default();
            let mut sorted = entries.clone();
            sort(&dir, &mut sorted);
            let subs: Vec<String> = sorted
                .iter()
                .filter(|e| e.ftype == VfType::Directory)
                .map(|e| e.file.path().unwrap().to_string_lossy().into_owned())
                .collect();
            for s in subs.iter().rev() {
                stack.push(s.clone());
            }
            out.push(WalkEntry { path: dir, entries });
        }
        Ok(out)
    }

    fn renamev(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
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
            let s = self.vf_path(src).map_err(|e| e.with_index(i))?;
            let d = self.vf_path(dst).map_err(|e| e.with_index(i))?;
            prs.push(crate::client::PathRenamePair { src: s, dst: d });
        }
        let outcome = self
            .nfs
            .renamev_path_compound(&prs)
            .map_err(|e| VfError::from_rpc(e, 0))?;
        match outcome.failed {
            Some((i, _st)) => {
                // Prefix [0..i) renamed; retry [i..] via the phased path,
                // re-attributing its error to the original index.
                let suffix = &pairs[i..];
                self.renamev_phased(suffix).map_err(|e| {
                    let rel = e.index();
                    e.with_index(i + rel)
                })
            }
            None => Ok(()),
        }
    }

    fn removev(&mut self, files: &[VfFile]) -> VfRes {
        if files.is_empty() {
            return Ok(());
        }
        if files.iter().any(|f| f.is_descriptor()) {
            return self.removev_phased(files);
        }
        let mut paths = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            paths.push(self.vf_path(f).map_err(|e| e.with_index(i))?);
        }
        let outcome = self
            .nfs
            .removev_path_compound(&paths)
            .map_err(|e| VfError::from_rpc(e, 0))?;
        match outcome.failed {
            Some((i, _st)) => {
                // Prefix [0..i) removed; retry [i..] via the phased path.
                let suffix = &files[i..];
                self.removev_phased(suffix).map_err(|e| {
                    let rel = e.index();
                    e.with_index(i + rel)
                })
            }
            None => Ok(()),
        }
    }

    fn mkdirv(&mut self, dirs: &[VfAttrs]) -> VfRes {
        let mut creates = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            let path = self.vf_path(&a.file).map_err(|e| e.with_index(i))?;
            let (dir, name) = split_path(&path).map_err(|e| VfError::failure(i, e))?;
            let dirfh = self.resolve_path(dir, true).map_err(|e| e.with_index(i))?;
            creates.push(crate::client::CreateOp {
                dir: dirfh,
                name: name.to_string(),
                ftype: nfs_ftype4_NF4DIR,
                linkdata: None,
            });
        }
        self.nfs
            .create_many(&creates)
            .map_err(VfError::from_rpc_indexed)?;

        // Apply modes (the handles come from re-resolving the new dirs).
        let mut setattrs = Vec::new();
        for (i, a) in dirs.iter().enumerate() {
            if a.masks.contains(AttrMask::MODE) {
                let path = self.vf_path(&a.file).map_err(|e| e.with_index(i))?;
                // `path` is already root-relative (from `vf_path`).
                let fh = self
                    .nfs
                    .resolve(&path)
                    .map_err(|e| VfError::from_rpc(e, i))?;
                setattrs.push(crate::client::SetattrOp {
                    fh,
                    mode: Some(a.mode & 0o7777),
                    size: None,
                });
            }
        }
        if !setattrs.is_empty() {
            self.nfs
                .setattr_many(&setattrs)
                .map_err(VfError::from_rpc_indexed)?;
        }
        Ok(())
    }

    fn symlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            let full = self.abs_path(new);
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e))?;
            let dirfh = self.resolve_path(dir, true).map_err(|e| e.with_index(i))?;
            ops.push(crate::client::CreateOp {
                dir: dirfh,
                name: name.to_string(),
                ftype: nfs_ftype4_NF4LNK,
                linkdata: Some(old.as_bytes().to_vec()),
            });
        }
        self.nfs
            .create_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    fn readlinkv(&mut self, paths: &[&str]) -> VfResult<Vec<Vec<u8>>> {
        let mut ops = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let fh = self.resolve(p).map_err(|e| e.with_index(i))?;
            ops.push(crate::client::ReadlinkOp { fh });
        }
        self.nfs
            .readlink_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    fn hardlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            let src = self.resolve(old).map_err(|e| e.with_index(i))?;
            let full = self.abs_path(new);
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e))?;
            let dirfh = self.resolve_path(dir, true).map_err(|e| e.with_index(i))?;
            ops.push(crate::client::LinkOp {
                dstdir: dirfh,
                src,
                newname: name.to_string(),
            });
        }
        self.nfs.link_many(&ops).map_err(VfError::from_rpc_indexed)
    }

    fn dupv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            // Follow final-component symlinks for both ends, matching the
            // `std::fs` backend (OPEN cannot target a symlink directly).
            let src = self
                .follow_target_path(&self.abs_path(&p.src_path))
                .map_err(|e| e.with_index(i))?;
            let dst = self
                .follow_target_path(&self.abs_path(&p.dst_path))
                .map_err(|e| e.with_index(i))?;
            self.copy_extent(&src, &dst, p)
                .map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn lcopyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            let src = self.lstat(&p.src_path).map_err(|e| e.with_index(i))?;
            if src.ftype == VfType::Symlink {
                let target = self.readlink(&p.src_path).map_err(|e| e.with_index(i))?;
                self.symlink(&String::from_utf8_lossy(&target), &p.dst_path)
                    .map_err(|e| e.with_index(i))?;
            } else {
                let dst = self
                    .follow_target_path(&self.abs_path(&p.dst_path))
                    .map_err(|e| e.with_index(i))?;
                self.copy_extent(&self.abs_path(&p.src_path), &dst, p)
                    .map_err(|e| e.with_index(i))?;
            }
        }
        Ok(())
    }

    fn write_adb(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let mut counts = Vec::with_capacity(patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            let full = self.abs_path(&p.path);
            let (dir, name) = match split_path(&full) {
                Ok(x) => x,
                Err(e) => return Err(VfError::failure(i, e)),
            };
            let dirfh = match self.resolve_path(dir, true) {
                Ok(fh) => fh,
                Err(e) => return Err(e.with_index(i)),
            };
            let (fh, sid) = match self
                .nfs
                .open_path(
                    &dirfh,
                    name,
                    OPEN4_SHARE_ACCESS_WRITE,
                    crate::client::OpenCreate::Guarded,
                )
                .map_err(|e| VfError::from_rpc(e, 0))
            {
                Ok(x) => x,
                Err(e) => return Err(e.with_index(i)),
            };
            let mut written = 0usize;
            let mut failed: Option<VfError> = None;
            for b in 0..p.adb_block_count {
                let base = p.adb_offset.saturating_add(b as u64 * p.adb_block_size);
                if let Some(reloff) = p.adb_reloff_blocknum {
                    let adbn = (p.adb_block_num + b as u64).to_be_bytes();
                    if let Err(e) = self.nfs.write(&fh, &sid, base + reloff, &adbn) {
                        failed = Some(VfError::from_rpc(e, i));
                        break;
                    }
                }
                if let Some(reloff) = p.adb_reloff_pattern
                    && !p.adb_pattern_data.is_empty()
                    && let Err(e) = self
                        .nfs
                        .write(&fh, &sid, base + reloff, &p.adb_pattern_data)
                {
                    failed = Some(VfError::from_rpc(e, i));
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

    fn rm(&mut self, objs: &[&str], recursive: bool) -> VfRes {
        for (i, o) in objs.iter().enumerate() {
            self.rm_one(o, recursive).map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn cp_recursive(
        &mut self,
        src_dir: &str,
        dst: &str,
        symlinks: bool,
        _use_server_side_copy: bool,
    ) -> VfRes {
        if !self.exists(dst)? {
            self.ensure_dir(dst, 0o755).map_err(|e| e.with_index(0))?;
        }
        let masks = AttrMask::MODE | AttrMask::SIZE | AttrMask::FILEID;
        let entries = self.listdir(src_dir, masks, 0, false)?;
        for e in entries {
            let name = e
                .file
                .path()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
            let src_child = format!("{}/{}", src_dir.trim_end_matches('/'), name);
            let dst_child = format!("{}/{}", dst.trim_end_matches('/'), name);
            if e.ftype == VfType::Directory {
                self.cp_recursive(&src_child, &dst_child, symlinks, false)?;
            } else if e.ftype == VfType::Symlink && symlinks {
                let target = self.readlink(&src_child).map_err(|e| e.with_index(0))?;
                self.symlink(&String::from_utf8_lossy(&target), &dst_child)
                    .map_err(|e| e.with_index(0))?;
            } else {
                let pair = ExtentPair::new(&src_child, 0, &dst_child, 0, None);
                let src = self
                    .follow_target_path(&self.abs_path(&src_child))
                    .map_err(|e| e.with_index(0))?;
                let dst = self
                    .follow_target_path(&self.abs_path(&dst_child))
                    .map_err(|e| e.with_index(0))?;
                self.copy_extent(&src, &dst, &pair)
                    .map_err(|e| e.with_index(0))?;
            }
        }
        Ok(())
    }
}

impl Drop for NfsVecFs {
    /// `tc_deinit()`: close every open file so the client has no state left,
    /// allowing the session teardown to destroy the clientid on the server.
    fn drop(&mut self) {
        let fds: Vec<i32> = self.open_files.keys().copied().collect();
        for fd in fds {
            let open = self.open_files.remove(&fd);
            if let Some(o) = open {
                let _ = self.nfs.close(&o.fh, &o.stateid);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NFS attribute parsing
// ---------------------------------------------------------------------------

/// Parsed values of a GETATTR reply for the supported FATTR4 attributes.
#[derive(Debug, Clone, Default)]
struct AttrValues {
    ftype: Option<u32>,
    mode: Option<u32>,
    size: Option<u64>,
    nlink: Option<u32>,
    fileid: Option<u64>,
    uid: Option<u32>,
    gid: Option<u32>,
    rdev: Option<u64>,
    blocks: Option<u64>,
    mtime: Option<(i64, u32)>,
    atime: Option<(i64, u32)>,
    ctime: Option<(i64, u32)>,
    has_named_attr: Option<bool>,
}

/// The full set of supported FATTR4 ids, in wire (increasing) order. Must
/// match `crate::client::READDIR_ATTRS`.
const FULL_ATTR_IDS: [u32; 13] = [
    FATTR4_TYPE,
    FATTR4_SIZE,
    FATTR4_NAMED_ATTR,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
    FATTR4_OWNER,
    FATTR4_OWNER_GROUP,
    FATTR4_RAWDEV,
    FATTR4_SPACE_USED,
    FATTR4_TIME_ACCESS,
    FATTR4_TIME_METADATA,
    FATTR4_TIME_MODIFY,
];

fn request_mask_to_attr_list(masks: &AttrMask) -> Vec<u32> {
    let mut ids = Vec::new();
    for id in FULL_ATTR_IDS {
        let wanted = match id {
            FATTR4_TYPE => true, // always fetch type (cheap, aids listdir)
            FATTR4_SIZE => masks.contains(AttrMask::SIZE),
            FATTR4_NAMED_ATTR => masks.contains(AttrMask::NAMED_ATTR),
            FATTR4_FILEID => masks.contains(AttrMask::FILEID),
            FATTR4_MODE => masks.contains(AttrMask::MODE),
            FATTR4_NUMLINKS => masks.contains(AttrMask::NLINK),
            FATTR4_OWNER => masks.contains(AttrMask::UID),
            FATTR4_OWNER_GROUP => masks.contains(AttrMask::GID),
            FATTR4_RAWDEV => masks.contains(AttrMask::RDEV),
            FATTR4_SPACE_USED => masks.contains(AttrMask::BLOCKS),
            FATTR4_TIME_ACCESS => masks.contains(AttrMask::ATIME),
            FATTR4_TIME_METADATA => masks.contains(AttrMask::CTIME),
            FATTR4_TIME_MODIFY => masks.contains(AttrMask::MTIME),
            _ => false,
        };
        if wanted {
            ids.push(id);
        }
    }
    ids
}

/// Parse a raw GETATTR attribute list encoded for the given ids (in id order).
fn parse_attr_list(ids: &[u32], list: &[u8]) -> VfResult<AttrValues> {
    let mut v = AttrValues::default();
    let mut off = 0usize;
    for id in ids {
        match *id {
            FATTR4_TYPE => {
                v.ftype = Some(read_u32(list, &mut off)?);
            }
            FATTR4_SIZE => {
                v.size = Some(read_u64(list, &mut off)?);
            }
            FATTR4_NAMED_ATTR => {
                v.has_named_attr = Some(read_u32(list, &mut off)? != 0);
            }
            FATTR4_FILEID => {
                v.fileid = Some(read_u64(list, &mut off)?);
            }
            FATTR4_MODE => {
                v.mode = Some(read_u32(list, &mut off)?);
            }
            FATTR4_NUMLINKS => {
                v.nlink = Some(read_u32(list, &mut off)?);
            }
            FATTR4_OWNER => {
                v.uid = parse_uid(&read_str(list, &mut off)?);
            }
            FATTR4_OWNER_GROUP => {
                v.gid = parse_gid(&read_str(list, &mut off)?);
            }
            FATTR4_RAWDEV => {
                let major = read_u32(list, &mut off)?;
                let minor = read_u32(list, &mut off)?;
                v.rdev = Some(libc::makedev(major, minor));
            }
            FATTR4_SPACE_USED => {
                v.blocks = Some(read_u64(list, &mut off)? / 512);
            }
            FATTR4_TIME_ACCESS => {
                v.atime = Some(read_nfstime(list, &mut off)?);
            }
            FATTR4_TIME_MODIFY => {
                v.mtime = Some(read_nfstime(list, &mut off)?);
            }
            FATTR4_TIME_METADATA => {
                v.ctime = Some(read_nfstime(list, &mut off)?);
            }
            _ => unreachable!(),
        }
    }
    Ok(v)
}

/// `S_IFMT` type bits for an NFSv4 file type code.
fn s_ifmt(ftype: u32) -> u32 {
    match ftype {
        nfs_ftype4_NF4REG => 0o100000,  // S_IFREG
        nfs_ftype4_NF4DIR => 0o040000,  // S_IFDIR
        nfs_ftype4_NF4LNK => 0o120000,  // S_IFLNK
        nfs_ftype4_NF4BLK => 0o060000,  // S_IFBLK
        nfs_ftype4_NF4CHR => 0o020000,  // S_IFCHR
        nfs_ftype4_NF4FIFO => 0o010000, // S_IFIFO
        nfs_ftype4_NF4SOCK => 0o140000, // S_IFSOCK
        _ => 0,
    }
}

/// Fill `a` from parsed values where the mask requests the attribute. `mode`
/// is the permission bits plus the `S_IFMT` bits derived from `ftype`.
fn apply_attrs(a: &mut VfAttrs, v: &AttrValues) {
    a.ftype = v.ftype.map(VfType::from_nfs).unwrap_or(VfType::Regular);
    a.returned = AttrMask::empty();
    if a.masks.contains(AttrMask::MODE)
        && let Some(mode) = v.mode
    {
        a.mode = mode | s_ifmt(a.ftype.as_nfs());
        a.returned.insert(AttrMask::MODE);
    }
    if a.masks.contains(AttrMask::SIZE)
        && let Some(size) = v.size
    {
        a.size = size;
        a.returned.insert(AttrMask::SIZE);
    }
    if a.masks.contains(AttrMask::NLINK)
        && let Some(nlink) = v.nlink
    {
        a.nlink = nlink;
        a.returned.insert(AttrMask::NLINK);
    }
    if a.masks.contains(AttrMask::FILEID)
        && let Some(fileid) = v.fileid
    {
        a.fileid = fileid;
        a.returned.insert(AttrMask::FILEID);
    }
    if a.masks.contains(AttrMask::UID)
        && let Some(uid) = v.uid
    {
        a.uid = uid;
        a.returned.insert(AttrMask::UID);
    }
    if a.masks.contains(AttrMask::GID)
        && let Some(gid) = v.gid
    {
        a.gid = gid;
        a.returned.insert(AttrMask::GID);
    }
    if a.masks.contains(AttrMask::RDEV)
        && let Some(rdev) = v.rdev
    {
        a.rdev = rdev;
        a.returned.insert(AttrMask::RDEV);
    }
    if a.masks.contains(AttrMask::BLOCKS)
        && let Some(blocks) = v.blocks
    {
        // `v.blocks` is already FATTR4_SPACE_USED converted to 512-byte
        // units in `parse_attr_list` (matching the dummy's `st_blocks`).
        a.blocks = blocks;
        a.returned.insert(AttrMask::BLOCKS);
    }
    if a.masks.contains(AttrMask::MTIME)
        && let Some((s, n)) = v.mtime
    {
        a.mtime_sec = s;
        a.mtime_nsec = n;
        a.returned.insert(AttrMask::MTIME);
    }
    if a.masks.contains(AttrMask::ATIME)
        && let Some((s, n)) = v.atime
    {
        a.atime_sec = s;
        a.atime_nsec = n;
        a.returned.insert(AttrMask::ATIME);
    }
    if a.masks.contains(AttrMask::CTIME)
        && let Some((s, n)) = v.ctime
    {
        a.ctime_sec = s;
        a.ctime_nsec = n;
        a.returned.insert(AttrMask::CTIME);
    }
    if a.masks.contains(AttrMask::NAMED_ATTR)
        && let Some(has) = v.has_named_attr
    {
        a.has_named_attr = has;
        a.returned.insert(AttrMask::NAMED_ATTR);
    }
}

/// Resolve an NFS owner/group string ("1000" or "name@domain") to a numeric id.
fn name_to_id(s: &[u8], is_group: bool) -> Option<u32> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static CACHE: RefCell<HashMap<(String, bool), Option<u32>>> = RefCell::new(HashMap::new());
    }
    let key = (String::from_utf8_lossy(s).into_owned(), is_group);
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some(v) = cache.get(&key) {
            return *v;
        }
        let v = name_to_id_uncached(&key.0, is_group);
        cache.insert(key, v);
        v
    })
}

fn name_to_id_uncached(t: &str, is_group: bool) -> Option<u32> {
    let t = t.trim();
    if let Ok(v) = t.parse::<u32>() {
        return Some(v);
    }
    // Strip an "@domain" suffix and reverse-look-up the name.
    let base = t.split('@').next().unwrap_or(t);
    let cname = std::ffi::CString::new(base).ok()?;
    unsafe {
        if is_group {
            let gr = libc::getgrnam(cname.as_ptr());
            if gr.is_null() {
                None
            } else {
                Some((*gr).gr_gid)
            }
        } else {
            let pw = libc::getpwnam(cname.as_ptr());
            if pw.is_null() {
                None
            } else {
                Some((*pw).pw_uid)
            }
        }
    }
}

fn parse_uid(s: &[u8]) -> Option<u32> {
    name_to_id(s, false)
}

fn parse_gid(s: &[u8]) -> Option<u32> {
    name_to_id(s, true)
}

fn read_u32(buf: &[u8], off: &mut usize) -> VfResult<u32> {
    if *off + 4 > buf.len() {
        return Err(VfError::failure(0, VF_ERR_RPC));
    }
    let v = u32::from_be_bytes(buf[*off..*off + 4].try_into().unwrap());
    *off += 4;
    Ok(v)
}

fn read_u64(buf: &[u8], off: &mut usize) -> VfResult<u64> {
    if *off + 8 > buf.len() {
        return Err(VfError::failure(0, VF_ERR_RPC));
    }
    let v = u64::from_be_bytes(buf[*off..*off + 8].try_into().unwrap());
    *off += 8;
    Ok(v)
}

/// Read an XDR `nfstime4`: `int64 seconds; uint32 nseconds` (12 bytes).
fn read_nfstime(buf: &[u8], off: &mut usize) -> VfResult<(i64, u32)> {
    if *off + 12 > buf.len() {
        return Err(VfError::failure(0, VF_ERR_RPC));
    }
    let secs = i64::from_be_bytes(buf[*off..*off + 8].try_into().unwrap());
    let nsec = u32::from_be_bytes(buf[*off + 8..*off + 12].try_into().unwrap());
    *off += 12;
    Ok((secs, nsec))
}

/// Read an XDR `utf8string`: length + padded bytes.
fn read_str(buf: &[u8], off: &mut usize) -> VfResult<Vec<u8>> {
    let len = read_u32(buf, off)? as usize;
    if *off + len > buf.len() {
        return Err(VfError::failure(0, VF_ERR_RPC));
    }
    let s = buf[*off..*off + len].to_vec();
    *off += (len + 3) & !3;
    Ok(s)
}
