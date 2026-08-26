//! A [`VecFs`] implementation backed by the local filesystem (`std::fs`), so
//! the vectorized API also works on non-NFS filesystems.
//!
//! `"/"` maps to the `root` directory passed to [`DummyVecFs::new`]; all
//! writes stay under that root.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use crate::vecfs::*;

/// Open state for a descriptor on the local filesystem.
struct DummyOpen {
    file: File,
    path: String,
    cur_offset: u64,
}

/// A local-filesystem [`VecFs`]. `"/"` is the `root` directory.
pub struct DummyVecFs {
    root: PathBuf,
    cwd: PathBuf,
    next_fd: i32,
    open_files: HashMap<i32, DummyOpen>,
}

impl DummyVecFs {
    /// Create a client rooted at `root` (created if missing).
    pub fn new(root: PathBuf) -> DummyVecFs {
        std::fs::create_dir_all(&root).expect("create dummy root");
        DummyVecFs {
            root,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: HashMap::new(),
        }
    }

    /// Map a (possibly cwd-relative) path onto the real filesystem.
    fn resolve(&self, path: &str) -> PathBuf {
        if path.starts_with('/') {
            self.root.join(path.trim_start_matches('/'))
        } else {
            self.root.join(&self.cwd).join(path)
        }
    }

    fn errno(e: &std::io::Error) -> u32 {
        e.raw_os_error().map(|n| n as u32).unwrap_or(VF_ERR_RPC)
    }

    /// Build `OpenOptions` from fcntl-style flags.
    fn open_options(flags: i32) -> OpenOptions {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_RDWR, O_TRUNC, O_WRONLY};
        let mut o = OpenOptions::new();
        let read = (flags & (O_RDWR | O_WRONLY)) == 0 || (flags & O_RDWR) != 0;
        let write = (flags & (O_RDWR | O_WRONLY)) != 0;
        o.read(read).write(write);
        if flags & O_CREAT != 0 {
            o.create(true);
        }
        if flags & O_EXCL != 0 {
            o.create_new(true);
        }
        if flags & O_TRUNC != 0 {
            o.truncate(true);
        }
        if flags & O_APPEND != 0 {
            o.append(true);
        }
        o
    }

    /// The real path a `VfFile` refers to.
    fn tcfile_path(&self, f: &VfFile) -> VfResult<PathBuf> {
        match f {
            VfFile::Descriptor(fd) => {
                let open = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
                Ok(self.resolve(&open.path))
            }
            VfFile::Path { path, .. } | VfFile::Current(Some(path)) => {
                Ok(self.resolve(&path.to_string_lossy()))
            }
            VfFile::Current(None) | VfFile::Null | VfFile::Saved => Err(VfError::unsupported(0)),
        }
    }

    fn resolve_offset(&self, file: &VfFile, off: u64, len: u64) -> VfResult<u64> {
        if off == VF_OFFSET_CUR {
            if let Some(fd) = file.fd() {
                Ok(self.open_files.get(&fd).map(|o| o.cur_offset).unwrap_or(0))
            } else {
                Ok(0)
            }
        } else if off == VF_OFFSET_END {
            Ok(len)
        } else {
            Ok(off)
        }
    }

    fn advance_offset(&mut self, file: &VfFile, new: u64) {
        if let Some(fd) = file.fd()
            && let Some(o) = self.open_files.get_mut(&fd)
        {
            o.cur_offset = new;
        }
    }

    /// Whether the object has at least one extended attribute (llistxattr).
    fn has_xattr(path: &str) -> bool {
        use std::ffi::CString;
        let Ok(cpath) = CString::new(path) else {
            return false;
        };
        unsafe { libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) > 0 }
    }

    fn fill_attrs(&self, a: &mut VfAttrs, path: &str, md: &std::fs::Metadata) {
        let ft = md.file_type();
        a.ftype = if ft.is_dir() {
            VfType::Directory
        } else if ft.is_symlink() {
            VfType::Symlink
        } else {
            VfType::Regular
        };
        a.has_named_attr = Self::has_xattr(path);
        if a.masks.has_mode {
            a.mode = md.mode();
        }
        if a.masks.has_size {
            a.size = md.len();
        }
        if a.masks.has_nlink {
            a.nlink = md.nlink() as u32;
        }
        if a.masks.has_fileid {
            a.fileid = md.ino();
        }
        if a.masks.has_uid {
            a.uid = md.uid();
        }
        if a.masks.has_gid {
            a.gid = md.gid();
        }
        if a.masks.has_rdev {
            a.rdev = md.rdev();
        }
        if a.masks.has_blocks {
            a.blocks = md.blocks();
        }
        if a.masks.has_mtime {
            a.mtime_sec = md.mtime();
            a.mtime_nsec = md.mtime_nsec() as u32;
        }
        if a.masks.has_atime {
            a.atime_sec = md.atime();
            a.atime_nsec = md.atime_nsec() as u32;
        }
        if a.masks.has_ctime {
            a.ctime_sec = md.ctime();
            a.ctime_nsec = md.ctime_nsec() as u32;
        }
    }

    fn readv_one(&mut self, iov: &mut VfIoVec) -> VfResult<usize> {
        let (file, off, descriptor) = match &iov.file {
            VfFile::Descriptor(fd) => {
                let o = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
                let len = o
                    .file
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                    .len();
                let off = self.resolve_offset(&iov.file, iov.offset, len)?;
                let f = o
                    .file
                    .try_clone()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                (f, off, true)
            }
            VfFile::Path { .. } | VfFile::Current(_) => {
                let p = self.tcfile_path(&iov.file)?;
                let mut opts = OpenOptions::new();
                opts.read(true);
                if iov.is_creation {
                    opts.create(true);
                }
                let f = opts
                    .open(&p)
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                let len = f
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                    .len();
                let off = self.resolve_offset(&iov.file, iov.offset, len)?;
                (f, off, false)
            }
            _ => return Err(VfError::unsupported(0)),
        };
        let mut buf = vec![0u8; iov.length];
        let n = file
            .read_at(&mut buf, off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        buf.truncate(n);
        iov.data = buf;
        if descriptor {
            self.advance_offset(&iov.file, off + n as u64);
        }
        Ok(n)
    }

    fn writev_one(&mut self, iov: &mut VfIoVec) -> VfResult<usize> {
        let (file, off, descriptor) = match &iov.file {
            VfFile::Descriptor(fd) => {
                let o = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
                let len = o
                    .file
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                    .len();
                let off = self.resolve_offset(&iov.file, iov.offset, len)?;
                let f = o
                    .file
                    .try_clone()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                (f, off, true)
            }
            VfFile::Path { .. } | VfFile::Current(_) => {
                let p = self.tcfile_path(&iov.file)?;
                let mut opts = OpenOptions::new();
                opts.write(true);
                if iov.is_creation {
                    opts.create(true);
                }
                let f = opts
                    .open(&p)
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                let len = f
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                    .len();
                let off = self.resolve_offset(&iov.file, iov.offset, len)?;
                (f, off, false)
            }
            _ => return Err(VfError::unsupported(0)),
        };
        file.write_all_at(&iov.data, off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        if descriptor {
            self.advance_offset(&iov.file, off + iov.data.len() as u64);
        }
        Ok(iov.data.len())
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
        let p = self.resolve(dir);
        let rd = std::fs::read_dir(&p).map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        for entry in rd {
            let entry = entry.map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = join_path(dir.trim_matches('/'), &name);
            let mut a = VfAttrs {
                file: VfFile::from_path(&format!("/{}", path)),
                masks,
                ..VfAttrs::default()
            };
            let md = entry
                .metadata()
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            let real = self.resolve(&path);
            self.fill_attrs(&mut a, &real.to_string_lossy(), &md);
            let is_dir = a.ftype == VfType::Directory;
            out.push(a);
            if recursive && is_dir {
                self.listdir_rec(&path, masks, max_count, recursive, out)?;
            }
        }
        Ok(())
    }

    fn copy_extent(&mut self, p: &ExtentPair) -> VfResult<()> {
        let sp = self.resolve(&p.src_path);
        let dp = self.resolve(&p.dst_path);
        let src = OpenOptions::new()
            .read(true)
            .open(&sp)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        let dst = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&dp)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        let mut so = p.src_offset;
        let mut doff = p.dst_offset;
        let mut copied: u64 = 0;
        loop {
            if p.length != u64::MAX && copied >= p.length {
                break;
            }
            let remaining = if p.length == u64::MAX {
                u64::MAX
            } else {
                p.length - copied
            };
            let chunk_len = remaining.min(1 << 20) as usize;
            let mut buf = vec![0u8; chunk_len];
            let n = src
                .read_at(&mut buf, so)
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            if n == 0 {
                break; // EOF
            }
            buf.truncate(n);
            dst.write_all_at(&buf, doff)
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            so += n as u64;
            doff += n as u64;
            copied += n as u64;
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
}

impl VecFs for DummyVecFs {
    fn abs_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            path.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(path).to_string_lossy().to_string()
        }
    }

    fn open_by_path(
        &mut self,
        base: VfPathBase,
        pathname: &str,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        use libc::O_CREAT;
        if base != VfPathBase::Cwd && !pathname.starts_with('/') {
            return Err(VfError::unsupported(0));
        }
        let p = self.resolve(pathname);
        let file = Self::open_options(flags)
            .open(&p)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        if flags & O_CREAT != 0 {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode & 0o7777));
        }
        self.next_fd += 1;
        self.open_files.insert(
            self.next_fd,
            DummyOpen {
                file,
                path: pathname.to_string(),
                cur_offset: 0,
            },
        );
        Ok(VfFile::from_fd(self.next_fd))
    }

    fn close(&mut self, tcf: &VfFile) -> VfResult<()> {
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        self.open_files
            .remove(&tcf.fd().unwrap())
            .map(|_| ())
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))
    }

    fn chdir(&mut self, path: &str) -> VfResult<()> {
        let p = self.resolve(path);
        if !p.is_dir() {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
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
        for (i, iov) in reads.iter_mut().enumerate() {
            iov.is_failure = false;
            iov.is_eof = false;
            let requested = iov.length;
            match self.readv_one(iov) {
                Ok(n) => {
                    iov.length = n;
                    iov.is_eof = n < requested;
                }
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
        for (i, iov) in writes.iter_mut().enumerate() {
            iov.is_failure = false;
            iov.is_eof = false;
            match self.writev_one(iov) {
                Ok(n) => {
                    iov.length = n;
                    iov.is_write_stable = true;
                }
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
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let cur = self
            .open_files
            .get(&tcf.fd().unwrap())
            .map(|o| o.cur_offset)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        let new = match whence {
            SEEK_SET => offset,
            SEEK_CUR => cur as i64 + offset,
            SEEK_END => {
                let o = self
                    .open_files
                    .get(&tcf.fd().unwrap())
                    .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
                let len = o
                    .file
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                    .len();
                len as i64 + offset
            }
            _ => return Err(VfError::failure(0, ERR_INVAL)),
        };
        if new < 0 {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let new = new as u64;
        self.advance_offset(tcf, new);
        Ok(new as i64)
    }

    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        for (i, a) in attrs.iter_mut().enumerate() {
            let p = self.tcfile_path(&a.file).map_err(|mut e| {
                e.index = i;
                e
            })?;
            let md = std::fs::metadata(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            self.fill_attrs(a, &p.to_string_lossy(), &md);
        }
        Ok(())
    }

    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        for (i, a) in attrs.iter().enumerate() {
            let p = self.tcfile_path(&a.file).map_err(|mut e| {
                e.index = i;
                e
            })?;
            if a.masks.has_mode {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(a.mode & 0o7777))
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            }
            if a.masks.has_size {
                let f = OpenOptions::new()
                    .write(true)
                    .open(&p)
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                f.set_len(a.size)
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            }
        }
        Ok(())
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
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let sp = src.path().ok_or_else(|| VfError::failure(i, ERR_INVAL))?;
            let dp = dst.path().ok_or_else(|| VfError::failure(i, ERR_INVAL))?;
            std::fs::rename(
                self.resolve(&sp.to_string_lossy()),
                self.resolve(&dp.to_string_lossy()),
            )
            .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
    }

    fn removev(&mut self, files: &[VfFile]) -> VfRes {
        for (i, f) in files.iter().enumerate() {
            let path = f
                .path()
                .ok_or_else(|| VfError::failure(i, ERR_INVAL))?
                .to_string_lossy()
                .to_string();
            let p = self.resolve(&path);
            let md =
                std::fs::symlink_metadata(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if md.is_dir() && !md.file_type().is_symlink() {
                std::fs::remove_dir(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            } else {
                std::fs::remove_file(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            }
        }
        Ok(())
    }

    fn mkdirv(&mut self, dirs: &[VfAttrs]) -> VfRes {
        for (i, a) in dirs.iter().enumerate() {
            let path = a
                .file
                .path()
                .ok_or_else(|| VfError::failure(i, ERR_INVAL))?
                .to_string_lossy()
                .to_string();
            let p = self.resolve(&path);
            std::fs::create_dir(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if a.masks.has_mode {
                let _ =
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(a.mode & 0o7777));
            }
        }
        Ok(())
    }

    fn symlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            symlink(old, self.resolve(new)).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
    }

    fn readlinkv(&mut self, paths: &[&str]) -> VfResult<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let target = std::fs::read_link(self.resolve(p))
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            out.push(target.as_os_str().as_encoded_bytes().to_vec());
        }
        Ok(out)
    }

    fn hardlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            std::fs::hard_link(self.resolve(old), self.resolve(new))
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
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
            let path = self.resolve(&p.path);
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            let mut written = 0usize;
            for b in 0..p.adb_block_count {
                let base = p.adb_offset.saturating_add(b as u64 * p.adb_block_size);
                if p.adb_reloff_blocknum != u64::MAX {
                    let adbn = (p.adb_block_num + b as u64).to_be_bytes();
                    file.write_all_at(&adbn, base + p.adb_reloff_blocknum)
                        .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                }
                if p.adb_reloff_pattern != u64::MAX && !p.adb_pattern_data.is_empty() {
                    file.write_all_at(&p.adb_pattern_data, base + p.adb_reloff_pattern)
                        .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                }
                written += 1;
            }
            p.adb_block_count = written;
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
                .path()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
            let src_child = format!("{}/{}", src_dir.trim_end_matches('/'), name);
            let dst_child = format!("{}/{}", dst.trim_end_matches('/'), name);
            if e.ftype == VfType::Directory {
                self.cp_recursive(&src_child, &dst_child, symlinks, false)?;
            } else if e.ftype == VfType::Symlink && symlinks {
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
                self.dupv(std::slice::from_ref(&pair)).map_err(|mut e| {
                    e.index = 0;
                    e
                })?;
            }
        }
        Ok(())
    }
}
