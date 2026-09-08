//! NFSv4.1 implementation of the vectorized [`VecFs`] API.
//!
//! [`NfsVecFs`] is the analog of the C `tc_init()` module handle: it connects
//! to an NFSv4.1 server, coalesces vector operations into as few compounds as
//! the server supports, and destroys its session/clientid on drop.

// bindgen emits lowercase constants (e.g. nfs_ftype4_NF4DIR) matched here.
#![allow(non_upper_case_globals)]

use std::path::{Path, PathBuf};

use nfsv41_sys::*;

use crate::client::{FileHandle, NfsClient, OpenCreate};
use crate::path::{
    components_bytes, join_path_bytes, normalize_bytes, path_bytes, path_from_bytes,
    split_path_bytes,
};
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

/// Per-connection telemetry for NFSv4.2 server-side COPY.
///
/// `requests` counts COPY compounds sent to the server, `operations` counts
/// COPY operations acknowledged successfully, and `fallbacks` counts runtime
/// downgrades to the client-side implementation after the server rejected
/// COPY.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NfsServerCopyStats {
    pub requests: u64,
    pub operations: u64,
    pub fallbacks: u64,
}

/// An NFSv4 client exposing the vectorized [`VecFs`] API.
pub struct NfsVecFs {
    nfs: NfsClient,
    cwd: PathBuf,
    next_fd: i32,
    /// Canonical open-file state, keyed by the client-assigned descriptor.
    open_files: std::collections::HashMap<i32, OpenFile>,
    server_copy_enabled: bool,
    server_copy_stats: NfsServerCopyStats,
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

    /// Set the per-compound payload cap for merged path I/O (bytes; 0 =
    /// unlimited).
    pub fn set_max_compound_bytes(&mut self, bytes: usize) {
        self.nfs.set_max_compound_bytes(bytes);
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
        let mut groups: BTreeMap<Vec<u8>, Vec<(usize, Vec<u8>)>> = BTreeMap::new();
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
        let mut dir_cache: HashMap<Vec<u8>, FileHandle> = HashMap::new();
        for (dir, entries) in groups {
            let dirfh = match dir_cache.get(&dir) {
                Some(fh) => fh.clone(),
                None => match self.resolve_path(&path_from_bytes(&dir), true) {
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
            let ops: Vec<(FileHandle, Vec<u8>)> = entries
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
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let file = if a.file.is_descriptor() {
                let fd = a.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(&self.vf_path(&a.file).map_err(|e| e.with_index(i))?)
                        .to_vec(),
                )
            };
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
                file,
                mode,
                size,
                check_type: true,
            });
        }
        match self.nfs.setattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    for (k, a) in attrs.iter().enumerate().take(i) {
                        let own_type = outcome.types[k];
                        if own_type == Some(nfs_ftype4_NF4LNK) {
                            if !follow {
                                return Err(VfError::unsupported(k));
                            }
                            self.setattr_one_following(k, a)?;
                        }
                    }
                    if let Err(e) = self.setattrsv_phased(&attrs[i..], follow) {
                        let rel = e.index();
                        return Err(e.with_index(i + rel));
                    }
                    return Ok(());
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
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let file = if a.file.is_descriptor() {
                let fd = a.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(&self.vf_path(&a.file).map_err(|e| e.with_index(i))?)
                        .to_vec(),
                )
            };
            ops.push(crate::client::PathGetattrOp {
                file,
                attrs: request_mask_to_attr_list(&a.masks),
            });
        }
        match self.nfs.getattr_path_compound(&ops) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    let mut prefix_attrs = attrs[..i].to_vec();
                    for (k, a) in prefix_attrs.iter_mut().enumerate() {
                        let list = outcome.lists[k].as_deref().unwrap_or_default();
                        let v = parse_attr_list(&ops[k].attrs, list)?;
                        apply_attrs(a, &v);
                        if follow && a.ftype == VfType::Symlink {
                            self.stat_one_following(k, a)?;
                        }
                    }
                    attrs[..i].clone_from_slice(&prefix_attrs);
                    if let Err(e) = self.getattrsv_phased(&mut attrs[i..], follow) {
                        let rel = e.index();
                        return Err(e.with_index(i + rel));
                    }
                    return Ok(());
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
        Ok(Self::from_client(nfs))
    }

    /// Connect using an explicit NFS minor version (2 enables server COPY).
    pub fn connect_minor(host: &str, minorversion: u32) -> VfResult<NfsVecFs> {
        let nfs =
            NfsClient::connect_minor(host, minorversion).map_err(|e| VfError::from_rpc(e, 0))?;
        Ok(Self::from_client(nfs))
    }

    fn from_client(nfs: NfsClient) -> NfsVecFs {
        let server_copy_enabled = cfg!(feature = "server-copy") && nfs.minorversion() >= 2;
        NfsVecFs {
            nfs,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: std::collections::HashMap::new(),
            server_copy_enabled,
            server_copy_stats: NfsServerCopyStats::default(),
            merged_mode: MergedIoMode::Full,
        }
    }

    /// Negotiated NFS minor version for this connection.
    pub fn minorversion(&self) -> u32 {
        self.nfs.minorversion()
    }

    /// Whether this connection will currently attempt NFSv4.2 server COPY.
    /// The value becomes false if the server rejects COPY at runtime.
    pub fn server_copy_enabled(&self) -> bool {
        self.server_copy_enabled
    }

    /// Return server-side COPY activity for this connection.
    pub fn server_copy_stats(&self) -> NfsServerCopyStats {
        self.server_copy_stats
    }

    // -- private helpers ----------------------------------------------------

    fn insert_open_file(&mut self, open: OpenFile) -> VfResult<i32> {
        crate::vecfs::insert_fd(&mut self.next_fd, &mut self.open_files, open)
    }

    /// Resolve a root-relative path to a file handle, following symlinks in
    /// intermediate components (POSIX pathwalk) and, when `follow_final` is
    /// set, the final component too (for `stat`/`open` semantics).
    ///
    /// The fast path is a single deep-resolve compound; when the server
    /// reports `NFS4ERR_SYMLINK` mid-path, resolution falls back to a
    /// component-wise walk that follows each symlink with READLINK, splicing
    /// its target into the remaining path (hop-capped at 40).
    fn resolve_path(&mut self, root_rel: &Path, follow_final: bool) -> VfResult<FileHandle> {
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
                        .map_err(|e| VfError::from_rpc(e, 0))?;
                    let mut off = 0;
                    if read_u32(&t, &mut off).unwrap_or(0) != nfs_ftype4_NF4LNK {
                        return Ok(fh);
                    }
                    let target = self
                        .nfs
                        .readlink(&fh)
                        .map_err(|e| VfError::from_rpc(e, 0))?;
                    path = Self::resolve_target(&path, &target);
                    hops += 1;
                }
                Err(e) if e.status == nfsstat4_NFS4ERR_SYMLINK => {
                    // An intermediate component is a symlink: walk component
                    // by component, following each link we encounter.
                    let comps = components_bytes(&path);
                    if comps.is_empty() {
                        return Err(VfError::from_rpc(e, 0));
                    }
                    let mut cur_fh = self.nfs.root().clone();
                    let mut consumed: Vec<u8> = Vec::new();
                    let mut followed = false;
                    for (i, comp) in comps.iter().enumerate() {
                        let is_last = i + 1 == comps.len();
                        let (child, ftype) = self
                            .nfs
                            .lookup_getattr(&cur_fh, comp)
                            .map_err(|e| VfError::from_rpc(e, 0))?;
                        let full_comp = join_path_bytes(&consumed, comp);
                        if ftype == nfs_ftype4_NF4LNK && (follow_final || !is_last) {
                            let target = self
                                .nfs
                                .readlink(&child)
                                .map_err(|e| VfError::from_rpc(e, 0))?;
                            let mut rest = Vec::new();
                            for (j, r) in comps.iter().enumerate().skip(i + 1) {
                                if j > i + 1 {
                                    rest.push(b'/');
                                }
                                rest.extend_from_slice(r);
                            }
                            let base = Self::resolve_target(&full_comp, &target);
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
        dir: &Path,
        name: &[u8],
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
    /// OPEN + GETFH per file. UNCHECKED creates carry the mode and the
    /// size=0 `O_TRUNC` in their createattrs, so no existence probe or
    /// separate SETATTR is needed (RFC 8881 §18.16.3).
    fn openv_merged(
        &mut self,
        paths: &[&Path],
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
                path: path_bytes(&self.abs_path(p)).to_vec(),
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
        let mut out: Vec<Option<VfFile>> = vec![None; paths.len()];
        let mut prefix = paths.len();
        if let Some((i, _st)) = outcome.failed {
            // Prefix [0..i) opened; resume [i..] via the phased path. If the
            // suffix fails, the prefix opens are abandoned to session
            // teardown (same as a whole-batch fallback would).
            prefix = i;
            let suffix = self
                .openv_phased(&paths[i..], &flags[i..], &modes[i..])
                .map_err(|e| {
                    let rel = e.index();
                    e.with_index(i + rel)
                })?;
            for (k, fd) in suffix.into_iter().enumerate() {
                out[i + k] = Some(fd);
            }
        }
        for (i, o) in outcome.opened.iter().enumerate().take(prefix) {
            let (fh, stateid) = o.clone().expect("completed open");
            let fd = self.insert_open_file(OpenFile {
                fh,
                stateid,
                cur_offset: 0,
                append: flags[i] & O_APPEND != 0,
            })?;
            out[i] = Some(VfFile::from_fd(fd));
        }
        Ok(out.into_iter().map(|o| o.expect("opened")).collect())
    }

    /// The legacy phased openv (batched existence probe + OPENs + SETATTRs).
    fn openv_phased(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<Vec<VfFile>> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        // Batched existence probe for O_CREAT-without-O_EXCL entries, so the
        // mode is only applied to files this call actually creates.
        let mut dir_cache: std::collections::HashMap<Vec<u8>, FileHandle> =
            std::collections::HashMap::new();
        let mut probe: Vec<(usize, FileHandle, Vec<u8>)> = Vec::new();
        let mut entries: Vec<(usize, FileHandle, Vec<u8>, u32, bool, bool)> = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let full = self.abs_path(p);
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let dirfh = match dir_cache.get(&dir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&dir), true)
                        .map_err(|e| e.with_index(i))?;
                    dir_cache.insert(dir.clone(), fh.clone());
                    fh
                }
            };
            let access = Self::flags_to_access(flags[i]);
            let create = flags[i] & O_CREAT != 0;
            let excl = flags[i] & O_EXCL != 0;
            if create && !excl {
                probe.push((i, dirfh.clone(), name.clone()));
            }
            entries.push((i, dirfh, name, access, excl, flags[i] & O_TRUNC != 0));
        }

        let mut created = vec![false; paths.len()];
        if !probe.is_empty() {
            let lookup: Vec<(FileHandle, Vec<u8>)> = probe
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
            let open = OpenFile {
                fh,
                stateid,
                cur_offset: 0,
                append: flags[i] & O_APPEND != 0,
            };
            let fd = self.insert_open_file(open)?;
            out.push(VfFile::from_fd(fd));
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
        truncate: &[bool],
    ) -> VfResult<Vec<Option<i32>>> {
        // Resolve each parent directory once per distinct dir, then look up
        // every final component in one tolerant batch (which also reports the
        // type, so symlinks can be followed only when actually present).
        let mut dir_cache: std::collections::HashMap<Vec<u8>, FileHandle> =
            std::collections::HashMap::new();
        let mut lookups: Vec<(usize, FileHandle, Vec<u8>)> = Vec::new();
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
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let dirfh = match dir_cache.get(&dir) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&dir), true)
                        .map_err(|e| e.with_index(i))?;
                    dir_cache.insert(dir.clone(), fh.clone());
                    fh
                }
            };
            lookups.push((i, dirfh, name));
        }
        if lookups.is_empty() {
            return Ok(vec![None; files.len()]);
        }
        let probe: Vec<(FileHandle, Vec<u8>)> = lookups
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
                    let full = &self.vf_path(files[*orig])?;
                    let full = self
                        .follow_target_path(full)
                        .map_err(|e| e.with_index(*orig))?;
                    let (dir, name2) = split_path_bytes(path_bytes(&full))
                        .map_err(|_| VfError::failure(*orig, ERR_NOENT))?;
                    let dirfh2 = self
                        .resolve_path(&path_from_bytes(&dir), true)
                        .map_err(|e| e.with_index(*orig))?;
                    let create = match self.nfs.lookup_getattr(&dirfh2, &name2) {
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
                            name: name2,
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
        // Apply O_TRUNC semantics in this phased fallback.
        let mut setattr_ops = Vec::new();
        for (&orig, (fh, _)) in subset.iter().zip(results.iter()) {
            if truncate.get(orig).copied().unwrap_or(false) {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: None,
                    size: Some(0),
                });
            }
        }
        if !setattr_ops.is_empty() {
            self.nfs.setattr_many(&setattr_ops).map_err(|e| {
                let e = VfError::from_rpc_indexed(e);
                match subset.get(e.index()) {
                    Some(orig) => e.with_index(*orig),
                    None => e,
                }
            })?;
        }
        let mut tmp = vec![None; files.len()];
        for (orig, (fh, stateid)) in subset.iter().zip(results) {
            let fd = self.insert_open_file(OpenFile {
                fh,
                stateid,
                cur_offset: 0,
                append: false,
            })?;
            tmp[*orig] = Some(fd);
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
        let per = self.nfs.read_per_op_bytes();
        let mut ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        let mut owner = Vec::with_capacity(reads.len());
        for (i, op) in reads.iter().enumerate() {
            let off = self
                .resolve_offset(&op.file, op.offset)
                .map_err(|e| e.with_index(i))?;
            let length = u64::try_from(op.length)
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            let o = self
                .open_files
                .get(&op.file.fd().unwrap())
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            let mut remaining = op.length;
            let mut chunk_off = 0u64;
            loop {
                let n = remaining.min(per);
                ops.push(crate::client::ReadOp {
                    fh: o.fh.clone(),
                    stateid: o.stateid,
                    offset: off
                        .checked_add(chunk_off)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?,
                    count: n as u32,
                });
                owner.push(i);
                remaining -= n;
                if remaining == 0 {
                    break;
                }
                chunk_off += n as u64;
            }
            offsets.push(off);
        }
        // The server validates the summed READ counts of a compound against
        // ca_maxresponsesize, so split the chunk list into reply-sized
        // sub-batches even though each op is already per-op capped.
        let chunks_per_compound = if self.nfs.max_response_bytes > 0 {
            (self.nfs.read_compound_bytes().saturating_sub(128) / per).max(1)
        } else {
            usize::MAX
        };
        let mut results = Vec::with_capacity(ops.len());
        for sub in ops.chunks(chunks_per_compound) {
            let r = self.nfs.readv(sub).map_err(VfError::from_rpc_indexed)?;
            results.extend(r);
        }
        let mut out = Vec::with_capacity(reads.len());
        let mut ci = 0usize;
        for (i, op) in reads.iter().enumerate() {
            let off = offsets[i];
            let mut data = Vec::new();
            let mut eof = false;
            while ci < owner.len() && owner[ci] == i {
                let (chunk, e) = &results[ci];
                data.extend_from_slice(chunk);
                eof = *e;
                ci += 1;
            }
            let new_offset = off
                .checked_add(data.len() as u64)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            self.advance_offset(&op.file, new_offset);
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
        let per = self.nfs.per_op_bytes();
        let mut ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        let mut owner = Vec::with_capacity(writes.len());
        for (i, op) in writes.iter().enumerate() {
            let off = self
                .write_offset(&op.file, op.offset)
                .map_err(|e| e.with_index(i))?;
            let length = u64::try_from(op.data.len())
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            let o = self
                .open_files
                .get(&op.file.fd().unwrap())
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            let mut remaining = op.data.len();
            let mut chunk_off = 0u64;
            loop {
                let n = remaining.min(per);
                ops.push(crate::client::WriteOp {
                    fh: o.fh.clone(),
                    stateid: o.stateid,
                    offset: off
                        .checked_add(chunk_off)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?,
                    data: op.data[chunk_off as usize..chunk_off as usize + n].to_vec(),
                });
                owner.push(i);
                remaining -= n;
                if remaining == 0 {
                    break;
                }
                chunk_off += n as u64;
            }
            offsets.push(off);
        }
        // Keep each compound's request under ca_maxrequestsize: the op-count
        // batching alone would pack hundreds of MiB of WRITEs together.
        let chunks_per_compound = if self.nfs.max_compound_bytes > 0 {
            (self.nfs.max_compound_bytes.saturating_sub(128) / per).max(1)
        } else {
            usize::MAX
        };
        let mut results = Vec::with_capacity(ops.len());
        for sub in ops.chunks(chunks_per_compound) {
            let r = self.nfs.writev(sub).map_err(VfError::from_rpc_indexed)?;
            results.extend(r);
        }
        let mut out = Vec::with_capacity(writes.len());
        let mut ci = 0usize;
        for (i, op) in writes.iter().enumerate() {
            let off = offsets[i];
            let mut written = 0u64;
            let mut stable = true;
            while ci < owner.len() && owner[ci] == i {
                let (n, committed) = &results[ci];
                written += *n as u64;
                stable = stable && *committed == stable_how4_FILE_SYNC4;
                ci += 1;
            }
            let new_offset = off
                .checked_add(written)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            self.advance_offset(&op.file, new_offset);
            out.push(WriteResult {
                file: op.file.clone(),
                offset: off,
                written: written as usize,
                stable,
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
    fn resolve_target(link_path: &[u8], target: &[u8]) -> Vec<u8> {
        if target.first() == Some(&b'/') {
            normalize_bytes(&target[1..])
        } else {
            let mut combined = Vec::new();
            if let Some(idx) = link_path.iter().rposition(|&b| b == b'/') {
                combined.extend_from_slice(&link_path[..=idx]);
            };
            combined.extend_from_slice(target);
            normalize_bytes(&combined)
        }
    }

    fn follow_target_path(&mut self, root_rel: &Path) -> VfResult<PathBuf> {
        let mut current = normalize_bytes(path_bytes(root_rel));
        let mut hops = 0usize;
        loop {
            let mut abs = b"/".to_vec();
            abs.extend_from_slice(&current);
            let abs_path = path_from_bytes(&abs);
            let st = match self.lstat(&abs_path) {
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
            let target = self.readlink(&abs_path)?;
            current = Self::resolve_target(&current, &target);
            hops += 1;
        }
    }

    fn resolve_follow(&mut self, path: &Path) -> VfResult<FileHandle> {
        self.resolve_path(path, true)
    }

    fn listdir_rec(
        &mut self,
        dir: &Path,
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
                let path = dir.join(path_from_bytes(&e.name));
                let mut a = VfAttrs {
                    file: VfFile::from_os_path(&path),
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

    fn rm_one(&mut self, path: &Path, recursive: bool) -> VfResult<()> {
        let ft = self.file_type(path).unwrap_or(VfType::Regular);
        if ft == VfType::Directory && recursive {
            let entries = self.listdir(path, AttrMask::default(), usize::MAX, false)?;
            for e in entries {
                let p = e.file.path().unwrap().to_path_buf();
                self.rm_one(&p, true)?;
            }
        }
        self.unlink(path)
    }

    /// Read every page of directory `fh`, returning its entries as `VfAttrs`.
    fn readdir_all(
        &mut self,
        fh: &FileHandle,
        dir_path: &Path,
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
        parent_path: &Path,
        masks: &AttrMask,
        ids: &[u32],
        de: &crate::client::DirEntry,
    ) -> VfAttrs {
        let path = parent_path.join(path_from_bytes(&de.name));
        let mut a = VfAttrs {
            file: VfFile::from_os_path(&path),
            masks: *masks,
            ..VfAttrs::default()
        };
        let vals = parse_attr_list(ids, &de.attrs).unwrap_or_default();
        apply_attrs(&mut a, &vals);
        a
    }

    fn copy_extent(
        &mut self,
        src_root_rel: &Path,
        dst_root_rel: &Path,
        p: &ExtentPair,
    ) -> VfResult<()> {
        let (sdir, sname) = split_path_bytes(path_bytes(src_root_rel))
            .map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let (ddir, dname) = split_path_bytes(path_bytes(dst_root_rel))
            .map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let sdirfh = self
            .resolve_path(&path_from_bytes(&sdir), true)
            .map_err(|e| e.with_index(0))?;
        let ddirfh = self
            .resolve_path(&path_from_bytes(&ddir), true)
            .map_err(|e| e.with_index(0))?;
        let (sfh, ssid) = self
            .nfs
            .open_path(
                &sdirfh,
                &sname,
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
                &dname,
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
                    &dname,
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
            so = so
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
            doff = doff
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
            copied = copied
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
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

    fn copy_extents_server_side(&mut self, pairs: &[ExtentPair]) -> VfRes {
        let src_files: Vec<VfFile> = pairs
            .iter()
            .map(|p| VfFile::from_os_path(&self.abs_path(&p.src_path)))
            .collect();
        let dst_files: Vec<VfFile> = pairs
            .iter()
            .map(|p| VfFile::from_os_path(&self.abs_path(&p.dst_path)))
            .collect();
        let src_refs: Vec<&VfFile> = src_files.iter().collect();
        let dst_refs: Vec<&VfFile> = dst_files.iter().collect();
        let no_create = vec![false; pairs.len()];
        let create = vec![true; pairs.len()];
        let no_truncate = vec![false; pairs.len()];

        let src_tmp = self.open_path_batch(&src_refs, &no_create, false, &no_truncate)?;
        let dst_tmp = match self.open_path_batch(&dst_refs, &create, true, &no_truncate) {
            Ok(tmp) => tmp,
            Err(e) => {
                self.close_tmp(&src_tmp);
                return Err(e);
            }
        };

        let result = (|| {
            let explicit: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter_map(|(i, pair)| pair.length.is_some_and(|length| length > 0).then_some(i))
                .collect();
            let mut source_attrs: Vec<VfAttrs> = explicit
                .iter()
                .map(|&i| VfAttrs {
                    file: VfFile::from_fd(src_tmp[i].expect("path source was opened")),
                    masks: AttrMask::SIZE,
                    ..VfAttrs::default()
                })
                .collect();
            if !source_attrs.is_empty() {
                self.getattrsv(&mut source_attrs)?;
            }
            let mut effective_lengths: Vec<Option<u64>> =
                pairs.iter().map(|pair| pair.length).collect();
            for (&i, attrs) in explicit.iter().zip(&source_attrs) {
                effective_lengths[i] = pairs[i]
                    .length
                    .map(|length| length.min(attrs.size.saturating_sub(pairs[i].src_offset)));
            }
            let mut copies = Vec::with_capacity(pairs.len());
            for (i, p) in pairs.iter().enumerate() {
                let src = self
                    .open_files
                    .get(&src_tmp[i].expect("path source was opened"))
                    .expect("temporary source descriptor");
                let dst = self
                    .open_files
                    .get(&dst_tmp[i].expect("path destination was opened"))
                    .expect("temporary destination descriptor");
                copies.push(crate::client::CopyOp {
                    src_fh: src.fh.clone(),
                    src_stateid: src.stateid,
                    dst_fh: dst.fh.clone(),
                    dst_stateid: dst.stateid,
                    src_offset: p.src_offset,
                    dst_offset: p.dst_offset,
                    count: effective_lengths[i].unwrap_or(0),
                });
            }
            let mut totals = vec![0u64; copies.len()];
            // NFSv4.2 uses count=0 to mean "through EOF", while the VFSI API
            // uses Some(0) for an explicit zero-byte copy. Do not put those
            // operations on the wire; the SETATTR phase below still applies
            // the destination-size semantics.
            let mut pending: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter_map(|(i, _)| (effective_lengths[i] != Some(0)).then_some(i))
                .collect();
            while !pending.is_empty() {
                let active: Vec<crate::client::CopyOp> = pending
                    .iter()
                    .map(|&i| crate::client::CopyOp {
                        src_fh: copies[i].src_fh.clone(),
                        src_stateid: copies[i].src_stateid,
                        dst_fh: copies[i].dst_fh.clone(),
                        dst_stateid: copies[i].dst_stateid,
                        src_offset: copies[i].src_offset,
                        dst_offset: copies[i].dst_offset,
                        count: effective_lengths[i]
                            .map(|length| length.saturating_sub(totals[i]))
                            .unwrap_or(0),
                    })
                    .collect();
                self.server_copy_stats.requests += 1;
                let counts = self.nfs.copy_many(&active).map_err(|e| {
                    let original = pending.get(e.op_index).copied().unwrap_or(0);
                    VfError::from_rpc_indexed(e.with_op_index(original))
                })?;
                self.server_copy_stats.operations += counts.len() as u64;
                let mut next = Vec::new();
                for (&i, n) in pending.iter().zip(counts) {
                    totals[i] = totals[i]
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    copies[i].src_offset = copies[i]
                        .src_offset
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    copies[i].dst_offset = copies[i]
                        .dst_offset
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    // A zero-byte continuation means EOF. For an explicit
                    // count this matches dupv's short-at-EOF behavior; for a
                    // count of zero it confirms that a possibly partial COPY
                    // has reached EOF.
                    if n != 0
                        && effective_lengths[i]
                            .map(|length| totals[i] < length)
                            .unwrap_or(true)
                    {
                        next.push(i);
                    }
                }
                pending = next;
            }
            let mut attrs = Vec::with_capacity(pairs.len());
            for (i, (&n, p)) in totals.iter().zip(pairs).enumerate() {
                let size = p
                    .dst_offset
                    .checked_add(n)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                attrs.push(crate::client::SetattrOp {
                    fh: copies[i].dst_fh.clone(),
                    mode: None,
                    size: Some(size),
                });
            }
            self.nfs
                .setattr_many(&attrs)
                .map_err(VfError::from_rpc_indexed)
        })();
        self.close_tmp(&src_tmp);
        self.close_tmp(&dst_tmp);
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
            tmp = self.open_path_batch(
                &files,
                &vec![false; reads.len()],
                false,
                &vec![false; reads.len()],
            )?;
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
                if let Some((i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    let mut out = self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof);
                    match self.readv_path_fallback(&reads[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            let rel = e.index();
                            return Err(e.with_index(i + rel));
                        }
                    }
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
                if let Some((i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        // The special stateid itself is rejected: disable the
                        // merged path entirely.
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    let mut out = self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof);
                    match self.readv_path_fallback(&reads[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            let rel = e.index();
                            return Err(e.with_index(i + rel));
                        }
                    }
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
            let truncation: Vec<bool> = writes.iter().map(|w| w.truncate).collect();
            tmp = self.open_path_batch(&files, &creation, true, &truncation)?;
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
                truncate: false,
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
                if let Some((i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    let mut out =
                        self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed);
                    match self.writev_path_fallback(&writes[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            let rel = e.index();
                            return Err(e.with_index(i + rel));
                        }
                    }
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
                if let Some((i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    let mut out =
                        self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed);
                    match self.writev_path_fallback(&writes[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            let rel = e.index();
                            return Err(e.with_index(i + rel));
                        }
                    }
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
        let mut src_cache: std::collections::HashMap<Vec<u8>, FileHandle> =
            std::collections::HashMap::new();
        let mut dst_cache: std::collections::HashMap<Vec<u8>, FileHandle> =
            std::collections::HashMap::new();
        let mut ops = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let s = self.vf_path(src).map_err(|e| e.with_index(i))?;
            let d = self.vf_path(dst).map_err(|e| e.with_index(i))?;
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
            .map_err(VfError::from_rpc_indexed)
    }

    /// The legacy phased removev (grouped per parent + remove_many).
    fn removev_phased(&mut self, files: &[VfFile]) -> VfRes {
        use std::collections::BTreeMap;
        // Group by parent directory to batch REMOVEs.
        let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            let path = self.vf_path(f).map_err(|e| e.with_index(i))?;
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
                .map_err(VfError::from_rpc_indexed)?;
        }
        Ok(())
    }

    /// Apply the requested modes of `dirs` in one batched resolve + SETATTR.
    /// Used by mkdirv after creation (NFSv4 CREATE cannot carry mode attrs).
    fn apply_dir_modes(&mut self, dirs: &[VfAttrs]) -> VfRes {
        let mut paths: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        let mut indices = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            if a.masks.contains(AttrMask::MODE) {
                let path = match self.vf_path(&a.file) {
                    Ok(p) => p,
                    Err(e) => return Err(e.with_index(i)),
                };
                paths.push(Path::new("/").join(&path));
                indices.push(i);
            }
        }
        if paths.is_empty() {
            return Ok(());
        }
        let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = match self.resolve_many_tcfile(&refs, true) {
            Ok(r) => r,
            Err(e) => {
                let rel = e.index();
                return Err(e.with_index(indices.get(rel).copied().unwrap_or(0)));
            }
        };
        let mut setattrs = Vec::with_capacity(dirs.len());
        for (k, r) in resolved.iter().enumerate() {
            match r {
                Ok((fh, _)) => setattrs.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: Some(dirs[indices[k]].mode & 0o7777),
                    size: None,
                }),
                Err(status) => return Err(VfError::failure(indices[k], *status)),
            }
        }
        if !setattrs.is_empty() {
            self.nfs.setattr_many(&setattrs).map_err(|e| {
                let rel = e.op_index;
                let orig = indices.get(rel).copied().unwrap_or(0);
                VfError::from_rpc_indexed(e.with_op_index(orig))
            })?;
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
    fn nfs_minorversion(&self) -> Option<u32> {
        Some(self.minorversion())
    }

    fn capabilities(&self) -> u64 {
        VF_CAP_UNIX_SEMANTICS
            | if self.server_copy_enabled() {
                VF_CAP_SERVER_COPY
            } else {
                0
            }
    }

    fn abs_path(&self, path: &Path) -> PathBuf {
        let root_rel = if path.is_absolute() {
            path.strip_prefix("/").unwrap_or(path).to_path_buf()
        } else {
            self.cwd.join(path)
        };
        path_from_bytes(&normalize_bytes(path_bytes(&root_rel)))
    }

    fn open_by_path(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        let full = match base {
            VfPathBase::Abs => pathname.strip_prefix("/").unwrap_or(pathname).to_path_buf(),
            VfPathBase::Cwd => self.cwd.join(pathname),
        };
        let full = path_from_bytes(&normalize_bytes(path_bytes(&full)));
        // Follow a final symlink chain so O_CREAT creates the target
        // (POSIX semantics); OPEN cannot target a symlink directly.
        let full = self.follow_target_path(&full)?;
        let (dir, name) =
            split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let access = Self::flags_to_access(flags);
        let create = flags & O_CREAT != 0;
        let excl = flags & O_EXCL != 0;
        // The mode only applies when O_CREAT actually creates the file
        // (POSIX ignores it for existing files).
        let created = if create {
            if excl {
                true
            } else {
                match self.nfs.resolve(path_bytes(&full)) {
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
        let (fh, stateid) = self.open_impl(&path_from_bytes(&dir), &name, access, created, excl)?;
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
        let open = OpenFile {
            fh,
            stateid,
            cur_offset: 0,
            append: flags & O_APPEND != 0,
        };
        let fd = self.insert_open_file(open)?;
        Ok(VfFile::from_fd(fd))
    }

    fn openv(&mut self, paths: &[&Path], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
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

    fn chdir(&mut self, path: &Path) -> VfResult<()> {
        let st = self.stat(path)?;
        if st.ftype != VfType::Directory {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        self.cwd = self.abs_path(path);
        Ok(())
    }

    fn getcwd(&self) -> PathBuf {
        Path::new("/").join(&self.cwd)
    }

    fn readv(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        if reads.iter().all(|r| r.file.is_descriptor()) {
            return self.readv_batch(reads);
        }
        // Path-based (and mixed descriptor/path) batches: resolve offsets and
        // try the merged compound.
        let mut path_ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        for (i, r) in reads.iter().enumerate() {
            let off = match r.offset {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let path = self.vf_path(&r.file).map_err(|e| e.with_index(i))?;
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
                    crate::path::path_bytes(&self.vf_path(&r.file).map_err(|e| e.with_index(i))?)
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
            MergedIoMode::Off => self.readv_path_fallback(reads),
            MergedIoMode::OpenWrite => self.readv_path_openwrite(reads, &path_ops, &offsets),
            MergedIoMode::Full => self.readv_path_full(reads, &path_ops, &offsets),
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

    fn read_allv(&mut self, files: &[VfFile]) -> VfResult<Vec<Vec<u8>>> {
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
        let mut active: Vec<usize> = (0..files.len()).collect();
        while !active.is_empty() {
            // Reserve each active file's ~128-byte per-op overhead so the
            // whole batch packs into one compound when it fits.
            let window = (compound_budget / active.len())
                .saturating_sub(128)
                .min(max_window)
                .max(1);
            let reads: Vec<ReadOp> = active
                .iter()
                .map(|&i| ReadOp::at(files[i].clone(), offsets[i], window))
                .collect();
            let results = self.readv(&reads)?;
            let mut next = Vec::with_capacity(active.len());
            for (k, &i) in active.iter().enumerate() {
                let r = &results[k];
                out[i].extend_from_slice(&r.data);
                offsets[i] = r
                    .offset
                    .checked_add(r.data.len() as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                if !r.eof {
                    next.push(i);
                }
            }
            active = next;
        }
        Ok(out)
    }

    fn writev(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        if writes.iter().all(|w| w.file.is_descriptor()) {
            return self.writev_batch(writes);
        }
        let mut path_ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        for (i, w) in writes.iter().enumerate() {
            let off = match w.offset {
                VfOffset::At(o) => o,
                VfOffset::End => {
                    let path = self.vf_path(&w.file).map_err(|e| e.with_index(i))?;
                    let fh = self.resolve_follow(&path).map_err(|e| e.with_index(i))?;
                    self.file_size(&fh).map_err(|e| e.with_index(i))?
                }
                VfOffset::Cur => match w.file.fd() {
                    Some(fd) => self
                        .open_files
                        .get(&fd)
                        .map(|o| o.cur_offset)
                        .ok_or_else(|| VfError::failure(i, ERR_EBADF))?,
                    None => return Err(VfError::failure(i, ERR_INVAL)),
                },
            };
            let length = u64::try_from(w.data.len())
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            offsets.push(off);
            let file = if w.file.is_descriptor() {
                let fd = w.file.fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                crate::client::FileRef::Handle(open.fh.clone())
            } else {
                crate::client::FileRef::Path(
                    crate::path::path_bytes(&self.vf_path(&w.file).map_err(|e| e.with_index(i))?)
                        .to_vec(),
                )
            };
            path_ops.push(crate::client::PathWriteOp {
                file,
                offset: off,
                data: w.data.clone(),
                create: w.creation && !w.file.is_descriptor(),
                truncate: w.truncate && !w.file.is_descriptor(),
                stateid: w
                    .file
                    .fd()
                    .and_then(|fd| self.open_files.get(&fd).map(|o| o.stateid)),
            });
        }
        let out = match self.merged_mode {
            MergedIoMode::Off => self.writev_path_fallback(writes),
            MergedIoMode::OpenWrite => self.writev_path_openwrite(writes, &path_ops, &offsets),
            MergedIoMode::Full => self.writev_path_full(writes, &path_ops, &offsets),
        }?;
        // Advance descriptor cursors for Cur-offset writes.
        for (i, w) in writes.iter().enumerate() {
            if w.offset == VfOffset::Cur && w.file.is_descriptor() {
                let new = out[i]
                    .offset
                    .checked_add(out[i].written as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                self.advance_offset(&w.file, new);
            }
        }
        Ok(out)
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
            SeekFrom::Cur => i64::try_from(cur)
                .ok()
                .and_then(|cur| cur.checked_add(offset))
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?,
            SeekFrom::End => {
                let fh = self
                    .open_files
                    .get(&tcf.fd().unwrap())
                    .map(|o| o.fh.clone())
                    .unwrap();
                let size = self.file_size(&fh)?;
                i64::try_from(size)
                    .ok()
                    .and_then(|size| size.checked_add(offset))
                    .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?
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
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        let mut out = Vec::new();
        self.listdir_rec(dir, masks, max_count, recursive, &mut out)?;
        Ok(out)
    }

    /// List many directories in a few compounds: the directories are
    /// resolved in one batched lookup, then their first READDIR pages (and
    /// any continuation pages) are drained in batched compounds — mirroring
    /// the txn-compound client's 64-READDIRs-per-compound listdirv.
    fn listdirv(
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
            let resolved = self.resolve_many_tcfile(&refs, true)?;
            let mut level: Vec<(FileHandle, PathBuf)> = Vec::with_capacity(level_paths.len());
            for (i, r) in resolved.iter().enumerate() {
                match r {
                    Ok((fh, ftype)) if *ftype == nfs_ftype4_NF4DIR => {
                        level.push((fh.clone(), level_paths[i].clone()));
                    }
                    Ok((_, _)) => return Err(VfError::failure(i, nfsstat4_NFS4ERR_NOTDIR)),
                    Err(status) => return Err(VfError::failure(i, *status)),
                }
            }
            // First pages for all directories in one compound.
            let ops: Vec<(FileHandle, u64)> = level.iter().map(|(fh, _)| (fh.clone(), 0)).collect();
            let results = self
                .nfs
                .readdir_pages(&ops, &ids)
                .map_err(|e| VfError::from_rpc(e, 0))?;
            let mut accumulated: Vec<Vec<crate::client::DirEntry>> =
                results.iter().map(|r| r.0.clone()).collect();
            let mut pending: Vec<(usize, FileHandle, u64)> = results
                .iter()
                .enumerate()
                .filter(|(_, r)| r.1 != 0)
                .map(|(i, r)| (i, level[i].0.clone(), r.1))
                .collect();
            // Drain continuation pages, batched across directories.
            while !pending.is_empty() {
                let cont_ops: Vec<(FileHandle, u64)> = pending
                    .iter()
                    .map(|(_, fh, cookie)| (fh.clone(), *cookie))
                    .collect();
                let cont = self
                    .nfs
                    .readdir_pages(&cont_ops, &ids)
                    .map_err(|e| VfError::from_rpc(e, 0))?;
                let mut next_pending = Vec::new();
                for ((idx, fh, _), (entries, cookie)) in pending.iter().zip(cont) {
                    accumulated[*idx].extend(entries);
                    if cookie != 0 {
                        next_pending.push((*idx, fh.clone(), cookie));
                    }
                }
                pending = next_pending;
            }
            // Emit entries and collect subdirectories for the next level.
            let mut next_level: Vec<PathBuf> = Vec::new();
            for (idx, entries) in accumulated.iter().enumerate() {
                let dir = &level[idx].1;
                for e in entries {
                    if max_entries != 0 && counted >= max_entries {
                        return Ok(());
                    }
                    let path = dir.join(path_from_bytes(&e.name));
                    let mut a = VfAttrs {
                        file: VfFile::from_os_path(&path),
                        masks,
                        ..VfAttrs::default()
                    };
                    let vals = parse_attr_list(&ids, &e.attrs).unwrap_or_default();
                    apply_attrs(&mut a, &vals);
                    if recursive && a.ftype == VfType::Directory {
                        next_level.push(path);
                    }
                    if !cb(&a, dir) {
                        return Ok(());
                    }
                    counted += 1;
                }
            }
            if !recursive {
                return Ok(());
            }
            level_paths = next_level;
        }
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
        root: &Path,
        masks: AttrMask,
        sort: &mut dyn FnMut(&Path, &mut Vec<VfAttrs>),
    ) -> VfResult<Vec<WalkEntry>> {
        let root_fh = self.resolve_path(&self.abs_path(root), true)?;
        let ids = request_mask_to_attr_list(&masks);
        let mut collected: std::collections::HashMap<PathBuf, Vec<VfAttrs>> =
            std::collections::HashMap::new();
        let mut root_attrs = self.readdir_all(&root_fh, root, &masks)?;
        sort(root, &mut root_attrs);

        // Frontier of (parent handle, child directory path) to list next.
        let mut frontier: Vec<(FileHandle, PathBuf)> = root_attrs
            .iter()
            .filter(|e| e.ftype == VfType::Directory)
            .map(|e| (root_fh.clone(), e.file.path().unwrap().to_path_buf()))
            .collect();
        collected.insert(root.to_path_buf(), root_attrs);

        while !frontier.is_empty() {
            // Resolve + list every frontier directory in batched compounds.
            let ops: Vec<(FileHandle, Vec<u8>)> = frontier
                .iter()
                .map(|(fh, p)| {
                    (
                        fh.clone(),
                        p.file_name()
                            .map(|n| path_bytes(Path::new(n)).to_vec())
                            .unwrap_or_default(),
                    )
                })
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
            let mut next_frontier: Vec<(FileHandle, PathBuf)> = Vec::new();
            for idx in 0..results.len() {
                let result = &results[idx];
                let path = frontier[idx].1.clone();
                let mut attrs = Vec::with_capacity(accumulated[idx].len());
                for de in &accumulated[idx] {
                    attrs.push(Self::dir_entry_to_attrs(&path, &masks, &ids, de));
                }
                sort(&path, &mut attrs);
                for a in &attrs {
                    if a.ftype == VfType::Directory {
                        next_frontier
                            .push((result.fh.clone(), a.file.path().unwrap().to_path_buf()));
                    }
                }
                collected.insert(path, attrs);
            }
            frontier = next_frontier;
        }

        // Emit in ls -R pre-order: a directory, then each of its subdirectories
        // (in sorted order) and their subtrees.
        let mut out = Vec::with_capacity(collected.len());
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = collected.remove(&dir).unwrap_or_default();
            let subs: Vec<PathBuf> = entries
                .iter()
                .filter(|e| e.ftype == VfType::Directory)
                .map(|e| e.file.path().unwrap().to_path_buf())
                .collect();
            for s in subs.into_iter().rev() {
                stack.push(s);
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
            prs.push(crate::client::PathRenamePair {
                src: path_bytes(&s).to_vec(),
                dst: path_bytes(&d).to_vec(),
            });
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
            paths.push(path_bytes(&self.vf_path(f).map_err(|e| e.with_index(i))?).to_vec());
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
        if dirs.is_empty() {
            return Ok(());
        }
        // Batch-resolve the parents, then CREATE in one compound.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            let path = self.vf_path(&a.file).map_err(|e| e.with_index(i))?;
            let (dir, name) =
                split_path_bytes(path_bytes(&path)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(Path::new("/").join(path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_many_tcfile(&refs, true)?;
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
                Err(status) => return Err(VfError::failure(i, *status)),
            }
        }
        if let Err(e) = self.nfs.create_many(&creates) {
            let i = e.op_index;
            // The prefix [0..i) was created; apply its modes before
            // reporting the failure (modes cannot ride in the CREATE).
            if i > 0 {
                let _ = self.apply_dir_modes(&dirs[..i]);
            }
            return Err(VfError::from_rpc_indexed(e));
        }
        self.apply_dir_modes(dirs)
    }

    fn symlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        // Batch-resolve the destination parents, then CREATE in one compound.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(newpaths.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(newpaths.len());
        for (i, new) in newpaths.iter().enumerate() {
            let full = self.abs_path(new);
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(Path::new("/").join(path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_many_tcfile(&refs, true)?;
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
                Err(status) => return Err(VfError::failure(i, *status)),
            }
        }
        self.nfs
            .create_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    fn readlinkv(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        // Batch-resolve the links themselves (no final-component follow),
        // then READLINK in one compound.
        let files: Vec<VfFile> = paths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let resolved = self.resolve_many_tcfile(&refs, false)?;
        let mut ops = Vec::with_capacity(paths.len());
        for (i, r) in resolved.iter().enumerate() {
            match r {
                Ok((fh, _)) => ops.push(crate::client::ReadlinkOp { fh: fh.clone() }),
                Err(status) => return Err(VfError::failure(i, *status)),
            }
        }
        self.nfs
            .readlink_many(&ops)
            .map_err(VfError::from_rpc_indexed)
    }

    fn hardlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
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
        let src_resolved = self.resolve_many_tcfile(&src_refs, false)?;
        let mut parents: Vec<PathBuf> = Vec::with_capacity(newpaths.len());
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(newpaths.len());
        for (i, new) in newpaths.iter().enumerate() {
            let full = self.abs_path(new);
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            parents.push(Path::new("/").join(path_from_bytes(&dir)));
            names.push(name);
        }
        let files: Vec<VfFile> = parents.iter().map(|p| VfFile::from_os_path(p)).collect();
        let refs: Vec<&VfFile> = files.iter().collect();
        let dst_resolved = self.resolve_many_tcfile(&refs, true)?;
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
                (Err(status), _) => return Err(VfError::failure(i, *status)),
                (_, Err(status)) => return Err(VfError::failure(i, *status)),
            }
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
                let target_path = path_from_bytes(&target);
                self.symlink(&target_path, &p.dst_path)
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

    fn copyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        if !self.server_copy_enabled {
            return self.dupv(pairs);
        }
        // Keep the number of simultaneously open source/destination states
        // bounded while still amortizing OPEN, COPY, SETATTR, and CLOSE.
        const FILES_PER_COPY_BATCH: usize = 8;
        for (base, batch) in pairs.chunks(FILES_PER_COPY_BATCH).enumerate() {
            if let Err(e) = self.copy_extents_server_side(batch) {
                if matches!(
                    e.err_no(),
                    nfsstat4_NFS4ERR_NOTSUPP
                        | nfsstat4_NFS4ERR_OP_ILLEGAL
                        | nfsstat4_NFS4ERR_OFFLOAD_DENIED
                        | nfsstat4_NFS4ERR_OFFLOAD_NO_REQS
                        | nfsstat4_NFS4ERR_STALE_STATEID
                        | nfsstat4_NFS4ERR_OLD_STATEID
                        | nfsstat4_NFS4ERR_BAD_STATEID
                ) {
                    self.server_copy_enabled = false;
                    self.server_copy_stats.fallbacks += 1;
                    let start = base * FILES_PER_COPY_BATCH;
                    return self.dupv(&pairs[start..]).map_err(|fallback| {
                        let index = fallback.index();
                        fallback.with_index(start + index)
                    });
                }
                let index = base * FILES_PER_COPY_BATCH + e.index();
                return Err(e.with_index(index));
            }
        }
        Ok(())
    }

    fn write_adb(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let mut counts = Vec::with_capacity(patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            let full = self.abs_path(&p.path);
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

    fn rm(&mut self, objs: &[&Path], recursive: bool) -> VfRes {
        for (i, o) in objs.iter().enumerate() {
            self.rm_one(o, recursive).map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn cp_recursive(
        &mut self,
        src_dir: &Path,
        dst: &Path,
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
                .map(|f| path_bytes(Path::new(f)).to_vec())
                .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
            let src_child = src_dir.join(path_from_bytes(&name));
            let dst_child = dst.join(path_from_bytes(&name));
            if e.ftype == VfType::Directory {
                self.cp_recursive(&src_child, &dst_child, symlinks, false)?;
            } else if e.ftype == VfType::Symlink && symlinks {
                let target = self.readlink(&src_child).map_err(|e| e.with_index(0))?;
                let target_path = path_from_bytes(&target);
                self.symlink(&target_path, &dst_child)
                    .map_err(|e| e.with_index(0))?;
            } else {
                let pair = ExtentPair::from_os_paths(&src_child, 0, &dst_child, 0, None);
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
        let closes: Vec<crate::client::CloseOp> = self
            .open_files
            .drain()
            .map(|(_, open)| crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            })
            .collect();
        let _ = self.nfs.close_many(&closes);
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
