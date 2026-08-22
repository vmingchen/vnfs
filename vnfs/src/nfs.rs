//! NFSv4.1 implementation of the vectorized [`VecFs`] API.
//!
//! [`NfsVecFs`] is the analog of the C `tc_init()` module handle: it connects
//! to an NFSv4.1 server, coalesces vector operations into as few compounds as
//! the server supports, and destroys its session/clientid on drop.

use std::path::PathBuf;

use nfsv41_sys::*;

use crate::client::{FileHandle, NfsClient, OpenCreate};
use crate::vecfs::*;

// Re-export the shared types/trait so `use vnfs::nfs::*` works.
pub use crate::vecfs::*;

/// An open file on the NFS server: resolved handle, open stateid and the
/// current read/write offset (for `tc_fseek`).
#[derive(Debug, Clone)]
struct OpenFile {
    fh: FileHandle,
    stateid: stateid4,
    cur_offset: u64,
}

/// An NFSv4.1 client exposing the vectorized [`VecFs`] API.
pub struct NfsVecFs {
    nfs: NfsClient,
    cwd: PathBuf,
    next_fd: i32,
    /// Canonical open-file state, keyed by the client-assigned descriptor.
    open_files: std::collections::HashMap<i32, OpenFile>,
}

impl NfsVecFs {
    /// Connect to the NFS server at `host` and resolve the export root.
    pub fn connect(host: &str) -> VfResult<NfsVecFs> {
        let nfs = NfsClient::connect(host).map_err(|e| VfError::from_rpc(0, e))?;
        Ok(NfsVecFs {
            nfs,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: std::collections::HashMap::new(),
        })
    }

    // -- private helpers ----------------------------------------------------

    /// Resolve `path` (absolute, or relative to the client cwd) to a handle.
    fn resolve(&mut self, path: &str) -> VfResult<FileHandle> {
        self.nfs
            .resolve(&self.abs_path(path))
            .map_err(|e| VfError::from_rpc(0, e))
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
        let dirfh = self.nfs.resolve(dir).map_err(|e| VfError::from_rpc(0, e))?;
        let mode = match (create, excl) {
            (false, _) => OpenCreate::NoCreate,
            (true, true) => OpenCreate::Exclusive,
            (true, false) => OpenCreate::Guarded,
        };
        self.nfs
            .open(&dirfh, name, access, mode)
            .map_err(|e| VfError::from_rpc(0, e))
    }

    /// Shared implementation of the openv variants using batched OPENs.
    fn openv_impl(
        &mut self,
        paths: &[&str],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<Vec<VfFile>> {
        use libc::{O_CREAT, O_EXCL, O_TRUNC};
        let mut opens = Vec::with_capacity(paths.len());
        let mut need_mode = Vec::with_capacity(paths.len());
        let mut need_trunc = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let full = self.abs_path(p);
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| VfError::from_rpc(i, e))?;
            let access = Self::flags_to_access(flags[i]);
            let create = flags[i] & O_CREAT != 0;
            let excl = flags[i] & O_EXCL != 0;
            let cmode = match (create, excl) {
                (false, _) => OpenCreate::NoCreate,
                (true, true) => OpenCreate::Exclusive,
                (true, false) => OpenCreate::Guarded,
            };
            opens.push(crate::client::OpenOp {
                dir: dirfh,
                name: name.to_string(),
                access,
                create: cmode,
            });
            need_mode.push(create);
            need_trunc.push(flags[i] & O_TRUNC != 0);
        }

        let results = self
            .nfs
            .open_many(&opens)
            .map_err(|e| VfError::from_rpc(0, e))?;

        // Apply per-file mode / O_TRUNC with a single SETATTR compound.
        let mut setattr_ops = Vec::new();
        for (i, (fh, _)) in results.iter().enumerate() {
            if need_mode[i] {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: Some(modes[i] & 0o7777),
                    size: None,
                });
            }
            if need_trunc[i] {
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
                .map_err(|e| VfError::from_rpc(0, e))?;
        }

        let mut out = Vec::with_capacity(results.len());
        for (fh, stateid) in results {
            self.next_fd += 1;
            let open = OpenFile {
                fh,
                stateid,
                cur_offset: 0,
            };
            self.open_files.insert(self.next_fd, open);
            out.push(VfFile::from_fd(self.next_fd));
        }
        Ok(out)
    }

    /// Resolve the file of an iovec to an (fh, stateid) pair, opening it
    /// implicitly for path-based iovecs. The third element reports whether
    /// the file was opened here and must be closed again (to avoid leaving
    /// open-owner state that would make the client undestroyable).
    fn resolve_iov_file(
        &mut self,
        iov: &VfIoVec,
        for_write: bool,
    ) -> VfResult<(FileHandle, stateid4, bool)> {
        match &iov.file {
            f if f.ftype == VfFileType::Descriptor => {
                let o = self
                    .open_files
                    .get(&f.fd)
                    .cloned()
                    .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_BAD_STATEID))?;
                Ok((o.fh, o.stateid, false))
            }
            f if f.ftype == VfFileType::Path || f.ftype == VfFileType::Current => {
                let path = match f.path.as_ref() {
                    Some(p) => p.to_string_lossy().to_string(),
                    None => return Err(VfError::failure(0, nfsstat4_NFS4ERR_NOENT)),
                };
                let full = self.abs_path(&path);
                let (dir, name) = split_path(&full)?;
                let access = if for_write {
                    OPEN4_SHARE_ACCESS_BOTH
                } else {
                    OPEN4_SHARE_ACCESS_READ
                };
                let (fh, sid) = self.open_impl(dir, name, access, iov.is_creation, false)?;
                Ok((fh, sid, true))
            }
            _ => Err(VfError::unsupported(0)),
        }
    }

    /// Batched readv for open (descriptor) iovecs: one compound per chunk of
    /// files, each carrying `[PUTFH, READ]` for every iovec.
    fn readv_batch(&mut self, reads: &mut [VfIoVec]) -> VfRes {
        let mut ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        for (i, iov) in reads.iter_mut().enumerate() {
            let off = match self.resolve_offset(&iov.file, iov.offset) {
                Ok(off) => off,
                Err(e) => {
                    iov.is_failure = true;
                    return Err(VfError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            };
            let o = match self.open_files.get(&iov.file.fd).cloned() {
                Some(o) => o,
                None => {
                    iov.is_failure = true;
                    return Err(VfError::failure(i, nfsstat4_NFS4ERR_BAD_STATEID));
                }
            };
            ops.push(crate::client::ReadOp {
                fh: o.fh,
                stateid: o.stateid,
                offset: off,
                count: iov.length.min(u32::MAX as usize) as u32,
            });
            offsets.push(off);
        }
        match self.nfs.readv(&ops) {
            Ok(results) => {
                for ((iov, data), off) in reads.iter_mut().zip(results).zip(offsets) {
                    let requested = iov.length;
                    iov.data = data;
                    iov.length = iov.data.len();
                    iov.is_eof = iov.data.len() < requested;
                    self.advance_offset(&iov.file, off + iov.data.len() as u64);
                }
                Ok(())
            }
            Err(e) => {
                let idx = e.op_index.saturating_sub(2) / 2;
                let idx = idx.min(reads.len());
                for (i, iov) in reads.iter_mut().enumerate() {
                    if i >= idx {
                        iov.is_failure = true;
                        iov.length = 0;
                    }
                }
                Err(VfError {
                    index: idx,
                    err_no: e.status,
                })
            }
        }
    }

    /// Resolve a special offset (`VF_OFFSET_CUR` / `VF_OFFSET_END`) to a
    /// concrete file offset; plain offsets pass through.
    fn resolve_offset(&mut self, file: &VfFile, off: u64) -> VfResult<u64> {
        if off == VF_OFFSET_CUR {
            if file.ftype == VfFileType::Descriptor {
                Ok(self
                    .open_files
                    .get(&file.fd)
                    .map(|o| o.cur_offset)
                    .unwrap_or(0))
            } else {
                Ok(0)
            }
        } else if off == VF_OFFSET_END {
            let fh = self.resolve_tcfile(file)?;
            self.file_size(&fh)
        } else {
            Ok(off)
        }
    }

    /// Record the new read/write offset of an open (descriptor) file.
    fn advance_offset(&mut self, file: &VfFile, new_offset: u64) {
        if file.ftype == VfFileType::Descriptor {
            if let Some(o) = self.open_files.get_mut(&file.fd) {
                o.cur_offset = new_offset;
            }
        }
    }

    fn readv_one(&mut self, iov: &mut VfIoVec) -> VfResult<()> {
        let (fh, stateid, close_after) = self.resolve_iov_file(iov, false)?;
        let want = iov.length.min(u32::MAX as usize) as u32;
        let mut offset = self.resolve_offset(&iov.file, iov.offset)?;
        let mut got = Vec::new();
        let result = loop {
            let remaining = want.saturating_sub(got.len() as u32);
            let chunk = self
                .nfs
                .read(&fh, &stateid, offset, remaining)
                .map_err(|e| VfError::from_rpc(0, e))?;
            if chunk.is_empty() {
                iov.is_eof = true;
                break Ok(());
            }
            let before = got.len();
            got.extend_from_slice(&chunk);
            offset += chunk.len() as u64;
            if chunk.len() < (want as usize).saturating_sub(before) {
                iov.is_eof = true;
            }
            if chunk.len() < remaining as usize || got.len() >= want as usize {
                break Ok(());
            }
        };
        if close_after {
            let _ = self.nfs.close(&fh, &stateid);
        }
        result?;
        iov.length = got.len();
        iov.data = got;
        self.advance_offset(&iov.file, offset);
        Ok(())
    }

    /// Batched writev for open (descriptor) iovecs.
    fn writev_batch(&mut self, writes: &mut [VfIoVec]) -> VfRes {
        let mut ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        for (i, iov) in writes.iter_mut().enumerate() {
            let off = match self.resolve_offset(&iov.file, iov.offset) {
                Ok(off) => off,
                Err(e) => {
                    iov.is_failure = true;
                    return Err(VfError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            };
            let o = match self.open_files.get(&iov.file.fd).cloned() {
                Some(o) => o,
                None => {
                    iov.is_failure = true;
                    return Err(VfError::failure(i, nfsstat4_NFS4ERR_BAD_STATEID));
                }
            };
            ops.push(crate::client::WriteOp {
                fh: o.fh,
                stateid: o.stateid,
                offset: off,
                data: iov.data.clone(),
            });
            offsets.push(off);
        }
        match self.nfs.writev(&ops) {
            Ok(results) => {
                for ((iov, (n, committed)), off) in writes.iter_mut().zip(results).zip(offsets) {
                    iov.length = n as usize;
                    iov.is_write_stable = committed == stable_how4_FILE_SYNC4;
                    self.advance_offset(&iov.file, off + n as u64);
                }
                Ok(())
            }
            Err(e) => {
                let idx = e.op_index.saturating_sub(2) / 2;
                let idx = idx.min(writes.len());
                for (i, iov) in writes.iter_mut().enumerate() {
                    if i >= idx {
                        iov.is_failure = true;
                        iov.length = 0;
                    }
                }
                Err(VfError {
                    index: idx,
                    err_no: e.status,
                })
            }
        }
    }

    fn writev_one(&mut self, iov: &mut VfIoVec) -> VfResult<()> {
        let (fh, stateid, close_after) = self.resolve_iov_file(iov, true)?;
        let offset = self.resolve_offset(&iov.file, iov.offset)?;
        let result = self
            .nfs
            .write(&fh, &stateid, offset, &iov.data)
            .map_err(|e| VfError::from_rpc(0, e));
        if close_after {
            let _ = self.nfs.close(&fh, &stateid);
        }
        let (n, committed) = result?;
        iov.length = n as usize;
        iov.is_write_stable = committed == stable_how4_FILE_SYNC4;
        self.advance_offset(&iov.file, offset + n as u64);
        Ok(())
    }

    /// The size in bytes of `fh`.
    fn file_size(&mut self, fh: &FileHandle) -> VfResult<u64> {
        let list = self
            .nfs
            .getattr(fh, &[FATTR4_SIZE])
            .map_err(|e| VfError::from_rpc(0, e))?;
        let mut off = 0;
        read_u64(&list, &mut off)
    }

    fn resolve_tcfile(&mut self, f: &VfFile) -> VfResult<FileHandle> {
        match f.ftype {
            VfFileType::Descriptor => self
                .open_files
                .get(&f.fd)
                .map(|o| o.fh.clone())
                .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_BAD_STATEID)),
            VfFileType::Path | VfFileType::Current => {
                let path = f
                    .path
                    .as_ref()
                    .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
                self.resolve(&path.to_string_lossy())
            }
            _ => Err(VfError::unsupported(0)),
        }
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
        let dirfh = self.resolve(dir)?;
        let mut cookie = 0u64;
        loop {
            let entries = self
                .nfs
                .readdir(&dirfh, cookie)
                .map_err(|e| VfError::from_rpc(0, e))?;
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
                // Attributes come back inline from READDIR for ATTR_IDS.
                let vals = parse_attrs(&e.attrs).unwrap_or_default();
                a.ftype = vals.ftype.unwrap_or(0);
                if masks.has_mode {
                    a.mode = vals.mode.unwrap_or(0);
                }
                if masks.has_size {
                    a.size = vals.size.unwrap_or(0);
                }
                if masks.has_nlink {
                    a.nlink = vals.nlink.unwrap_or(0);
                }
                if masks.has_fileid {
                    a.fileid = vals.fileid.unwrap_or(0);
                }
                let is_dir = a.ftype == nfs_ftype4_NF4DIR;
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
        let ft = self.file_type(path).unwrap_or(0);
        if ft == nfs_ftype4_NF4DIR && recursive {
            let entries = self.listdir(path, AttrMask::default(), usize::MAX, false)?;
            for e in entries {
                let p = e.file.path.unwrap().to_string_lossy().to_string();
                self.rm_one(&p, true)?;
            }
        }
        self.unlink(path)
    }

    fn copy_extent(&mut self, p: &ExtentPair) -> VfResult<()> {
        let sfull = self.abs_path(&p.src_path);
        let dfull = self.abs_path(&p.dst_path);
        let (sdir, sname) = split_path(&sfull)?;
        let (ddir, dname) = split_path(&dfull)?;
        let (sfh, ssid) = self.open_impl(sdir, sname, OPEN4_SHARE_ACCESS_READ, false, false)?;
        let (dfh, dsid) = self.open_impl(ddir, dname, OPEN4_SHARE_ACCESS_WRITE, true, false)?;

        let mut so = p.src_offset;
        let mut doff = p.dst_offset;
        let mut copied: u64 = 0;
        let result = loop {
            if p.length != u64::MAX && copied >= p.length {
                break Ok(());
            }
            let remaining = if p.length == u64::MAX {
                u64::MAX
            } else {
                p.length - copied
            };
            let chunk_len = remaining.min(1 << 20) as u32;
            let chunk = match self.nfs.read(&sfh, &ssid, so, chunk_len) {
                Ok(c) => c,
                Err(e) => break Err(VfError::from_rpc(0, e)),
            };
            if chunk.is_empty() {
                break Ok(()); // EOF
            }
            let n = match self.nfs.write(&dfh, &dsid, doff, &chunk) {
                Ok((n, _)) => n as u64,
                Err(e) => break Err(VfError::from_rpc(0, e)),
            };
            so += n;
            doff += n;
            copied += n;
        };
        let _ = self.nfs.close(&sfh, &ssid);
        let _ = self.nfs.close(&dfh, &dsid);
        result
    }
}

impl VecFs for NfsVecFs {
    fn abs_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            path.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(path).to_string_lossy().to_string()
        }
    }

    fn open_by_path(
        &mut self,
        dirfd: i32,
        pathname: &str,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        use libc::{O_CREAT, O_EXCL, O_TRUNC};
        let full = if pathname.starts_with('/') {
            pathname.trim_start_matches('/').to_string()
        } else if dirfd == VF_FD_CWD {
            self.cwd.join(pathname).to_string_lossy().to_string()
        } else {
            return Err(VfError::unsupported(0));
        };
        let (dir, name) = split_path(&full)?;
        let access = Self::flags_to_access(flags);
        let create = flags & O_CREAT != 0;
        let excl = flags & O_EXCL != 0;
        let (fh, stateid) = self.open_impl(dir, name, access, create, excl)?;
        if create {
            self.nfs
                .setattr(&fh, Some(mode & 0o7777), None)
                .map_err(|e| VfError::from_rpc(0, e))?;
        }
        if flags & O_TRUNC != 0 {
            self.nfs
                .setattr(&fh, None, Some(0))
                .map_err(|e| VfError::from_rpc(0, e))?;
        }
        self.next_fd += 1;
        let open = OpenFile {
            fh,
            stateid,
            cur_offset: 0,
        };
        self.open_files.insert(self.next_fd, open);
        Ok(VfFile::from_fd(self.next_fd))
    }

    fn openv(&mut self, paths: &[&str], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        self.openv_impl(paths, flags, modes)
    }

    fn close(&mut self, tcf: &VfFile) -> VfResult<()> {
        if tcf.ftype != VfFileType::Descriptor {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let open = self
            .open_files
            .remove(&tcf.fd)
            .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_BAD_STATEID))?;
        self.nfs
            .close(&open.fh, &open.stateid)
            .map_err(|e| VfError::from_rpc(0, e))
    }

    fn chdir(&mut self, path: &str) -> VfResult<()> {
        let _fh = self.resolve(path)?; // verify it exists
        self.cwd = if path.starts_with('/') {
            PathBuf::from(path.trim_start_matches('/'))
        } else {
            self.cwd.join(path)
        };
        Ok(())
    }

    fn getcwd(&self) -> String {
        format!("/{}", self.cwd.to_string_lossy())
    }

    fn readv(&mut self, reads: &mut [VfIoVec]) -> VfRes {
        for iov in reads.iter_mut() {
            iov.is_failure = false;
            iov.is_eof = false;
        }
        if !reads.is_empty() && reads.iter().all(|i| i.file.ftype == VfFileType::Descriptor) {
            return self.readv_batch(reads);
        }
        for (i, iov) in reads.iter_mut().enumerate() {
            let res = self.readv_one(iov);
            match res {
                Ok(()) => {}
                Err(e) => {
                    iov.is_failure = true;
                    iov.length = 0;
                    return Err(VfError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            }
        }
        Ok(())
    }

    fn writev(&mut self, writes: &mut [VfIoVec]) -> VfRes {
        for iov in writes.iter_mut() {
            iov.is_failure = false;
            iov.is_eof = false;
        }
        if !writes.is_empty()
            && writes
                .iter()
                .all(|i| i.file.ftype == VfFileType::Descriptor)
        {
            return self.writev_batch(writes);
        }
        for (i, iov) in writes.iter_mut().enumerate() {
            match self.writev_one(iov) {
                Ok(()) => {}
                Err(e) => {
                    iov.is_failure = true;
                    iov.length = 0;
                    return Err(VfError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            }
        }
        Ok(())
    }

    fn fseek(&mut self, tcf: &mut VfFile, offset: i64, whence: i32) -> VfResult<i64> {
        use libc::{SEEK_CUR, SEEK_END, SEEK_SET};
        if tcf.ftype != VfFileType::Descriptor {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let cur = self
            .open_files
            .get(&tcf.fd)
            .map(|o| o.cur_offset)
            .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_BAD_STATEID))?;
        let new = match whence {
            SEEK_SET => offset,
            SEEK_CUR => cur as i64 + offset,
            SEEK_END => {
                let fh = self.open_files.get(&tcf.fd).map(|o| o.fh.clone()).unwrap();
                let size = self.file_size(&fh)?;
                size as i64 + offset
            }
            _ => return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        };
        if new < 0 {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let new = new as u64;
        self.advance_offset(tcf, new);
        Ok(new as i64)
    }

    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        let mut ops = Vec::with_capacity(attrs.len());
        let mut ids_list = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let fh = self.resolve_tcfile(&a.file).map_err(|mut e| {
                e.index = i;
                e
            })?;
            let ids = request_mask_to_attr_list(&a.masks);
            ids_list.push(ids.clone());
            ops.push(crate::client::GetattrOp { fh, attrs: ids });
        }
        let results = self
            .nfs
            .getattr_many(&ops)
            .map_err(|e| VfError::from_rpc(0, e))?;
        for ((a, ids), list) in attrs.iter_mut().zip(ids_list).zip(results) {
            let v = parse_attr_list(&ids, &list)?;
            apply_attrs(a, &v);
        }
        Ok(())
    }

    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let fh = self.resolve_tcfile(&a.file).map_err(|mut e| {
                e.index = i;
                e
            })?;
            let mode = if a.masks.has_mode { Some(a.mode) } else { None };
            let size = if a.masks.has_size { Some(a.size) } else { None };
            if mode.is_none() && size.is_none() {
                return Err(VfError {
                    index: i,
                    err_no: VF_ERR_UNSUPPORTED,
                });
            }
            ops.push(crate::client::SetattrOp { fh, mode, size });
        }
        self.nfs
            .setattr_many(&ops)
            .map_err(|e| VfError::from_rpc(0, e))
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

    fn renamev(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        let mut ops = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let sp = src
                .path
                .as_ref()
                .ok_or_else(|| VfError::failure(i, nfsstat4_NFS4ERR_INVAL))?;
            let dp = dst
                .path
                .as_ref()
                .ok_or_else(|| VfError::failure(i, nfsstat4_NFS4ERR_INVAL))?;
            let s = sp.to_string_lossy().to_string();
            let d = dp.to_string_lossy().to_string();
            let (sdir, sname) = split_path(&s).map_err(|e| VfError::failure(i, e.err_no))?;
            let (ddir, dname) = split_path(&d).map_err(|e| VfError::failure(i, e.err_no))?;
            let sdirfh = self
                .nfs
                .resolve(sdir)
                .map_err(|e| VfError::from_rpc(i, e))?;
            let ddirfh = self
                .nfs
                .resolve(ddir)
                .map_err(|e| VfError::from_rpc(i, e))?;
            ops.push(crate::client::RenameOp {
                srcdir: sdirfh,
                oldname: sname.to_string(),
                dstdir: ddirfh,
                newname: dname.to_string(),
            });
        }
        self.nfs
            .rename_many(&ops)
            .map_err(|e| VfError::from_rpc(0, e))
    }

    fn removev(&mut self, files: &[VfFile]) -> VfRes {
        use std::collections::BTreeMap;
        // Group by parent directory to batch REMOVEs.
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            let path = f
                .path
                .as_ref()
                .ok_or_else(|| VfError::failure(i, nfsstat4_NFS4ERR_INVAL))?
                .to_string_lossy()
                .to_string();
            let (dir, name) = split_path(&path).map_err(|e| VfError::failure(i, e.err_no))?;
            groups
                .entry(dir.to_string())
                .or_default()
                .push(name.to_string());
        }
        for (dir, names) in &groups {
            let dirfh = self.nfs.resolve(dir).map_err(|e| VfError::from_rpc(0, e))?;
            let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            self.nfs
                .remove_many(&dirfh, &refs)
                .map_err(|e| VfError::from_rpc(0, e))?;
        }
        Ok(())
    }

    fn mkdirv(&mut self, dirs: &[VfAttrs]) -> VfRes {
        let mut creates = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            let path = a
                .file
                .path
                .as_ref()
                .ok_or_else(|| VfError::failure(i, nfsstat4_NFS4ERR_INVAL))?
                .to_string_lossy()
                .to_string();
            let full = self.abs_path(&path);
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| VfError::from_rpc(i, e))?;
            creates.push(crate::client::CreateOp {
                dir: dirfh,
                name: name.to_string(),
                ftype: nfs_ftype4_NF4DIR,
                linkdata: None,
            });
        }
        self.nfs
            .create_many(&creates)
            .map_err(|e| VfError::from_rpc(0, e))?;

        // Apply modes (the handles come from re-resolving the new dirs).
        let mut setattrs = Vec::new();
        for (i, a) in dirs.iter().enumerate() {
            if a.masks.has_mode {
                let path = a.file.path.as_ref().unwrap().to_string_lossy().to_string();
                let fh = self
                    .resolve(&path)
                    .map_err(|e| VfError::failure(i, e.err_no))?;
                setattrs.push(crate::client::SetattrOp {
                    fh,
                    mode: Some(a.mode),
                    size: None,
                });
            }
        }
        if !setattrs.is_empty() {
            self.nfs
                .setattr_many(&setattrs)
                .map_err(|e| VfError::from_rpc(0, e))?;
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
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| VfError::from_rpc(i, e))?;
            ops.push(crate::client::CreateOp {
                dir: dirfh,
                name: name.to_string(),
                ftype: nfs_ftype4_NF4LNK,
                linkdata: Some(old.as_bytes().to_vec()),
            });
        }
        self.nfs
            .create_many(&ops)
            .map_err(|e| VfError::from_rpc(0, e))
    }

    fn readlinkv(&mut self, paths: &[&str]) -> VfResult<Vec<Vec<u8>>> {
        let mut ops = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let fh = self.resolve(p).map_err(|e| VfError::failure(i, e.err_no))?;
            ops.push(crate::client::ReadlinkOp { fh });
        }
        self.nfs
            .readlink_many(&ops)
            .map_err(|e| VfError::from_rpc(0, e))
    }

    fn hardlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            let src = self
                .resolve(old)
                .map_err(|e| VfError::failure(i, e.err_no))?;
            let full = self.abs_path(new);
            let (dir, name) = split_path(&full).map_err(|e| VfError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| VfError::from_rpc(i, e))?;
            ops.push(crate::client::LinkOp {
                dstdir: dirfh,
                src,
                newname: name.to_string(),
            });
        }
        self.nfs
            .link_many(&ops)
            .map_err(|e| VfError::from_rpc(0, e))
    }

    fn dupv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            self.copy_extent(p).map_err(|mut e| {
                e.index = i;
                e
            })?;
        }
        Ok(())
    }

    fn write_adb(&mut self, patterns: &mut [Adb]) -> VfRes {
        for (i, p) in patterns.iter_mut().enumerate() {
            let full = self.abs_path(&p.path);
            let (dir, name) = match split_path(&full) {
                Ok(x) => x,
                Err(e) => return Err(VfError::failure(i, e.err_no)),
            };
            let (fh, sid) = match self.open_impl(dir, name, OPEN4_SHARE_ACCESS_WRITE, true, false) {
                Ok(x) => x,
                Err(e) => return Err(VfError::failure(i, e.err_no)),
            };
            let mut written = 0usize;
            let mut failed: Option<VfError> = None;
            for b in 0..p.adb_block_count {
                let base = p.adb_offset.saturating_add(b as u64 * p.adb_block_size);
                if p.adb_reloff_blocknum != u64::MAX {
                    let adbn = (p.adb_block_num + b as u64).to_be_bytes();
                    if let Err(e) = self
                        .nfs
                        .write(&fh, &sid, base + p.adb_reloff_blocknum, &adbn)
                    {
                        failed = Some(VfError::from_rpc(i, e));
                        break;
                    }
                }
                if p.adb_reloff_pattern != u64::MAX && !p.adb_pattern_data.is_empty() {
                    if let Err(e) =
                        self.nfs
                            .write(&fh, &sid, base + p.adb_reloff_pattern, &p.adb_pattern_data)
                    {
                        failed = Some(VfError::from_rpc(i, e));
                        break;
                    }
                }
                written += 1;
            }
            p.adb_block_count = written;
            let _ = self.nfs.close(&fh, &sid);
            if let Some(e) = failed {
                return Err(e);
            }
        }
        Ok(())
    }

    fn rm(&mut self, objs: &[&str], recursive: bool) -> VfRes {
        for (i, o) in objs.iter().enumerate() {
            self.rm_one(o, recursive).map_err(|mut e| {
                e.index = i;
                e
            })?;
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
        if !self.exists(dst) {
            self.ensure_dir(dst, 0o755).map_err(|mut e| {
                e.index = 0;
                e
            })?;
        }
        let masks = AttrMask {
            has_mode: true,
            has_size: true,
            has_fileid: true,
            ..AttrMask::default()
        };
        let entries = self.listdir(src_dir, masks, 0, false)?;
        for e in entries {
            let name = e
                .file
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .ok_or_else(|| VfError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
            let src_child = format!("{}/{}", src_dir.trim_end_matches('/'), name);
            let dst_child = format!("{}/{}", dst.trim_end_matches('/'), name);
            if e.ftype == nfs_ftype4_NF4DIR {
                self.cp_recursive(&src_child, &dst_child, symlinks, false)?;
            } else if e.ftype == nfs_ftype4_NF4LNK && symlinks {
                let target = self.readlink(&src_child).map_err(|mut e| {
                    e.index = 0;
                    e
                })?;
                self.symlink(&String::from_utf8_lossy(&target), &dst_child)
                    .map_err(|mut e| {
                        e.index = 0;
                        e
                    })?;
            } else {
                let pair = ExtentPair::new(&src_child, 0, &dst_child, 0, u64::MAX);
                self.copy_extent(&pair).map_err(|mut e| {
                    e.index = 0;
                    e
                })?;
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
}

/// Supported FATTR4 attribute ids, in the order they are encoded.
const ATTR_IDS: [u32; 5] = [
    FATTR4_TYPE,
    FATTR4_SIZE,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
];

fn request_mask_to_attr_list(masks: &AttrMask) -> Vec<u32> {
    let mut ids = Vec::new();
    for id in ATTR_IDS {
        let wanted = match id {
            FATTR4_TYPE => true, // always fetch type (cheap, aids listdir)
            FATTR4_SIZE => masks.has_size,
            FATTR4_FILEID => masks.has_fileid,
            FATTR4_MODE => masks.has_mode,
            FATTR4_NUMLINKS => masks.has_nlink,
            _ => false,
        };
        if wanted {
            ids.push(id);
        }
    }
    ids
}

/// Parse a GETATTR / READDIR-entry attribute list encoded for `ATTR_IDS`.
fn parse_attrs(list: &[u8]) -> VfResult<AttrValues> {
    let mut off = 0usize;
    let mut v = AttrValues::default();
    let rd32 = |off: &mut usize| -> VfResult<u32> {
        if *off + 4 > list.len() {
            return Err(VfError::failure(0, VF_ERR_RPC));
        }
        let r = u32::from_be_bytes([list[*off], list[*off + 1], list[*off + 2], list[*off + 3]]);
        *off += 4;
        Ok(r)
    };
    let rd64 = |off: &mut usize| -> VfResult<u64> {
        if *off + 8 > list.len() {
            return Err(VfError::failure(0, VF_ERR_RPC));
        }
        let r = u64::from_be_bytes(list[*off..*off + 8].try_into().unwrap());
        *off += 8;
        Ok(r)
    };
    for id in ATTR_IDS {
        match id {
            FATTR4_TYPE => v.ftype = Some(rd32(&mut off)?),
            FATTR4_SIZE => v.size = Some(rd64(&mut off)?),
            FATTR4_FILEID => v.fileid = Some(rd64(&mut off)?),
            FATTR4_MODE => v.mode = Some(rd32(&mut off)?),
            FATTR4_NUMLINKS => v.nlink = Some(rd32(&mut off)?),
            _ => unreachable!(),
        }
    }
    Ok(v)
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
            FATTR4_FILEID => {
                v.fileid = Some(read_u64(list, &mut off)?);
            }
            FATTR4_MODE => {
                v.mode = Some(read_u32(list, &mut off)?);
            }
            FATTR4_NUMLINKS => {
                v.nlink = Some(read_u32(list, &mut off)?);
            }
            _ => unreachable!(),
        }
    }
    Ok(v)
}

/// Fill `a` from parsed values where the mask requests the attribute.
fn apply_attrs(a: &mut VfAttrs, v: &AttrValues) {
    a.ftype = v.ftype.unwrap_or(0);
    if a.masks.has_mode {
        if let Some(mode) = v.mode {
            a.mode = mode;
        }
    }
    if a.masks.has_size {
        if let Some(size) = v.size {
            a.size = size;
        }
    }
    if a.masks.has_nlink {
        if let Some(nlink) = v.nlink {
            a.nlink = nlink;
        }
    }
    if a.masks.has_fileid {
        if let Some(fileid) = v.fileid {
            a.fileid = fileid;
        }
    }
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
