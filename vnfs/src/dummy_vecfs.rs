//! A [`VecFs`] implementation backed by the local filesystem (`std::fs`), so
//! the vectorized API also works on non-NFS filesystems.
//!
//! `"/"` maps to the `root` directory passed to [`DummyVecFs::new`]; all
//! writes stay under that root. Paths are normalized lexically (`.` and `..`
//! cannot escape the root), and path-based file operations resolve symlinks
//! and refuse targets outside the root (`ERR_ACCES`). Operations that never
//! follow symlinks (unlink, readlink, lstat, listing) operate on the
//! normalized path itself.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use crate::vecfs::*;

/// Open state for a descriptor on the local filesystem.
struct DummyOpen {
    file: File,
    path: String,
    cur_offset: u64,
    append: bool,
}

/// A local-filesystem [`VecFs`]. `"/"` is the `root` directory.
pub struct DummyVecFs {
    root: PathBuf,
    /// Canonical form of `root`, used for containment checks.
    root_canon: PathBuf,
    cwd: PathBuf,
    next_fd: i32,
    open_files: HashMap<i32, DummyOpen>,
}

impl DummyVecFs {
    fn getattrsv_impl(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        for (i, a) in attrs.iter_mut().enumerate() {
            let lexical = self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?;
            let p = if follow {
                self.real_path(&lexical).map_err(|e| e.with_index(i))?
            } else {
                lexical
            };
            let md = if follow {
                std::fs::metadata(&p)
            } else {
                std::fs::symlink_metadata(&p)
            }
            .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            self.fill_attrs(a, &p.to_string_lossy(), &md);
        }
        Ok(())
    }

    /// Create a client rooted at `root` (created if missing).
    pub fn new(root: PathBuf) -> DummyVecFs {
        std::fs::create_dir_all(&root).expect("create dummy root");
        let root_canon = std::fs::canonicalize(&root).expect("canonicalize dummy root");
        DummyVecFs {
            root,
            root_canon,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: HashMap::new(),
        }
    }

    /// Map a (possibly cwd-relative) path onto the real filesystem.
    /// The result is lexically normalized and cannot escape `root` via `..`.
    fn resolve(&self, path: &str) -> PathBuf {
        let suffix = if path.starts_with('/') {
            path.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(path).to_string_lossy().into_owned()
        };
        self.root.join(normalize_root_relative(&suffix))
    }

    /// Resolve `p` (already mapped under the root) to a real path that is
    /// guaranteed to stay under the root, following symlinks for existing
    /// objects and verifying the parent for paths that do not exist yet.
    /// Dangling symlinks are resolved one level at a time so an external
    /// target is refused even when it does not exist.
    fn real_path(&self, p: &Path) -> VfResult<PathBuf> {
        self.real_path_depth(p, 0)
    }

    fn real_path_depth(&self, p: &Path, depth: usize) -> VfResult<PathBuf> {
        if depth > 40 {
            return Err(VfError::failure(0, ERR_ACCES)); // symlink loop / too deep
        }
        // Resolve symlinks manually (before canonicalize, which would follow
        // an OS-absolute target outside the root): an absolute target is
        // chroot-relative, matching the NFS backend.
        if let Ok(md) = std::fs::symlink_metadata(p)
            && md.file_type().is_symlink()
        {
            let target = std::fs::read_link(p).map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            let target_path = if target.is_absolute() {
                // Chroot semantics, matching the NFS backend: an absolute
                // target resolves inside the root (leading "/" is the root,
                // and ".." components are clamped), so it can never escape.
                let stripped = target.strip_prefix("/").unwrap_or(&target);
                self.root
                    .join(normalize_root_relative(&stripped.to_string_lossy()))
            } else {
                p.parent().unwrap_or(Path::new("")).join(target)
            };
            return self.real_path_depth(&target_path, depth + 1);
        }
        if let Ok(c) = std::fs::canonicalize(p) {
            if c.starts_with(&self.root_canon) {
                return Ok(c);
            }
            return Err(VfError::failure(0, ERR_ACCES));
        }
        // Does not exist yet: the parent must be inside the root.
        let parent = p.parent().unwrap_or(Path::new(""));
        let canon_parent =
            std::fs::canonicalize(parent).map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        if !canon_parent.starts_with(&self.root_canon) {
            return Err(VfError::failure(0, ERR_ACCES));
        }
        Ok(canon_parent.join(p.file_name().unwrap_or_default()))
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
            VfFile::Path { .. } | VfFile::Cwd | VfFile::CwdPath(_) => {
                let path = self.vf_path(f)?;
                Ok(self.root.join(path))
            }
            VfFile::Saved => Err(VfError::unsupported(0)),
        }
    }

    fn resolve_offset(&self, file: &VfFile, off: VfOffset, len: u64) -> VfResult<u64> {
        match off {
            VfOffset::At(offset) => Ok(offset),
            VfOffset::Cur => match file.fd() {
                Some(fd) => Ok(self.open_files.get(&fd).map(|o| o.cur_offset).unwrap_or(0)),
                None => Err(VfError::failure(0, ERR_INVAL)),
            },
            VfOffset::End => Ok(len),
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
        } else if ft.is_fifo() {
            VfType::Fifo
        } else if ft.is_socket() {
            VfType::Socket
        } else if ft.is_block_device() {
            VfType::BlockDevice
        } else if ft.is_char_device() {
            VfType::CharDevice
        } else {
            VfType::Regular
        };
        a.has_named_attr = Self::has_xattr(path);
        a.returned = AttrMask::empty();
        if a.masks.contains(AttrMask::MODE) {
            a.mode = md.mode();
            a.returned.insert(AttrMask::MODE);
        }
        if a.masks.contains(AttrMask::SIZE) {
            a.size = md.len();
            a.returned.insert(AttrMask::SIZE);
        }
        if a.masks.contains(AttrMask::NLINK) {
            a.nlink = md.nlink() as u32;
            a.returned.insert(AttrMask::NLINK);
        }
        if a.masks.contains(AttrMask::FILEID) {
            a.fileid = md.ino();
            a.returned.insert(AttrMask::FILEID);
        }
        if a.masks.contains(AttrMask::UID) {
            a.uid = md.uid();
            a.returned.insert(AttrMask::UID);
        }
        if a.masks.contains(AttrMask::GID) {
            a.gid = md.gid();
            a.returned.insert(AttrMask::GID);
        }
        if a.masks.contains(AttrMask::RDEV) {
            a.rdev = md.rdev();
            a.returned.insert(AttrMask::RDEV);
        }
        if a.masks.contains(AttrMask::BLOCKS) {
            a.blocks = md.blocks();
            a.returned.insert(AttrMask::BLOCKS);
        }
        if a.masks.contains(AttrMask::MTIME) {
            a.mtime_sec = md.mtime();
            a.mtime_nsec = md.mtime_nsec() as u32;
            a.returned.insert(AttrMask::MTIME);
        }
        if a.masks.contains(AttrMask::ATIME) {
            a.atime_sec = md.atime();
            a.atime_nsec = md.atime_nsec() as u32;
            a.returned.insert(AttrMask::ATIME);
        }
        if a.masks.contains(AttrMask::CTIME) {
            a.ctime_sec = md.ctime();
            a.ctime_nsec = md.ctime_nsec() as u32;
            a.returned.insert(AttrMask::CTIME);
        }
        if a.masks.contains(AttrMask::NAMED_ATTR) {
            a.has_named_attr = Self::has_xattr(path);
            a.returned.insert(AttrMask::NAMED_ATTR);
        }
    }

    fn setattr_one(&mut self, a: &VfAttrs, i: usize) -> VfResult<()> {
        let p = self
            .real_path(&self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?)
            .map_err(|e| e.with_index(i))?;
        if a.masks.contains(AttrMask::MODE) {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(a.mode & 0o7777))
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        if a.masks.contains(AttrMask::SIZE) {
            let f = OpenOptions::new()
                .write(true)
                .open(&p)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            f.set_len(a.size)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
    }

    fn readv_one(&mut self, op: &ReadOp) -> VfResult<ReadResult> {
        let (file, off, descriptor) = match &op.file {
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
                let off = self.resolve_offset(&op.file, op.offset, len)?;
                let f = o
                    .file
                    .try_clone()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                (f, off, true)
            }
            VfFile::Path { .. } | VfFile::Cwd | VfFile::CwdPath(_) => {
                let p = self.real_path(&self.tcfile_path(&op.file)?)?;
                let f = OpenOptions::new()
                    .read(true)
                    .open(&p)
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                let len = f
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                    .len();
                let off = self.resolve_offset(&op.file, op.offset, len)?;
                (f, off, false)
            }
            VfFile::Saved => {
                return Err(VfError::unsupported(0));
            }
        };
        let mut buf = vec![0u8; op.length];
        let n = file
            .read_at(&mut buf, off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        buf.truncate(n);
        if descriptor {
            self.advance_offset(&op.file, off + n as u64);
        }
        Ok(ReadResult {
            file: op.file.clone(),
            offset: off,
            data: buf,
            eof: n < op.length,
        })
    }

    fn writev_one(&mut self, op: &WriteOp) -> VfResult<WriteResult> {
        let (file, off, descriptor) = match &op.file {
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
                // O_APPEND writes always land at EOF regardless of the
                // requested offset; report and track the real position.
                let off = if o.append {
                    len
                } else {
                    self.resolve_offset(&op.file, op.offset, len)?
                };
                let f = o
                    .file
                    .try_clone()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                (f, off, true)
            }
            VfFile::Path { .. } | VfFile::Cwd | VfFile::CwdPath(_) => {
                let p = self.real_path(&self.tcfile_path(&op.file)?)?;
                let mut opts = OpenOptions::new();
                opts.write(true);
                if op.creation {
                    opts.create(true);
                }
                if op.truncate {
                    opts.truncate(true);
                }
                let f = opts
                    .open(&p)
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                let len = f
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                    .len();
                let off = self.resolve_offset(&op.file, op.offset, len)?;
                (f, off, false)
            }
            VfFile::Saved => {
                return Err(VfError::unsupported(0));
            }
        };
        file.write_all_at(&op.data, off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        if descriptor {
            self.advance_offset(&op.file, off + op.data.len() as u64);
        }
        Ok(WriteResult {
            file: op.file.clone(),
            offset: off,
            written: op.data.len(),
            stable: true,
        })
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
            if reached_limit(out) {
                break;
            }
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
        let sp = self.real_path(&self.resolve(&p.src_path))?;
        let dp = self.real_path(&self.resolve(&p.dst_path))?;
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
            if let Some(length) = p.length
                && copied >= length
            {
                break;
            }
            let remaining = p.length.map_or(u64::MAX, |l| l - copied);
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
        // Truncate any stale tail beyond what was copied (cp semantics).
        dst.set_len(doff)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
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
        use libc::O_CREAT;
        // `VfPathBase::Abs` treats the path as root-relative (like NFS);
        // `Cwd` resolves against the current directory.
        let resolved = match base {
            VfPathBase::Abs => self
                .root
                .join(normalize_root_relative(pathname.trim_start_matches('/'))),
            VfPathBase::Cwd => self.resolve(pathname),
        };
        let p = self.real_path(&resolved)?;
        // The mode only applies when O_CREAT actually creates the file
        // (POSIX ignores it for existing files). Try create_new to detect
        // creation, then fall back to a plain open on EEXIST.
        let mut created = false;
        let file = if flags & O_CREAT != 0 {
            let mut opts = Self::open_options(flags);
            opts.create_new(true);
            match opts.open(&p) {
                Ok(f) => {
                    created = true;
                    f
                }
                Err(e)
                    if flags & libc::O_EXCL == 0
                        && e.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    Self::open_options(flags)
                        .open(&p)
                        .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                }
                Err(e) => return Err(VfError::failure(0, Self::errno(&e))),
            }
        } else {
            Self::open_options(flags)
                .open(&p)
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?
        };
        if created {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode & 0o7777));
        }
        self.next_fd += 1;
        self.open_files.insert(
            self.next_fd,
            DummyOpen {
                file,
                path: pathname.to_string(),
                cur_offset: 0,
                append: flags & libc::O_APPEND != 0,
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
        let p = self.real_path(&self.resolve(path))?;
        if !p.is_dir() {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        self.cwd = PathBuf::from(self.abs_path(path));
        Ok(())
    }

    fn getcwd(&self) -> String {
        format!("/{}", self.cwd.to_string_lossy())
    }

    fn readv(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let mut out = Vec::with_capacity(reads.len());
        for (i, op) in reads.iter().enumerate() {
            out.push(self.readv_one(op).map_err(|e| e.with_index(i))?);
        }
        Ok(out)
    }

    fn writev(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        let mut out = Vec::with_capacity(writes.len());
        for (i, op) in writes.iter().enumerate() {
            out.push(self.writev_one(op).map_err(|e| e.with_index(i))?);
        }
        Ok(out)
    }

    fn fseek(&mut self, tcf: &VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64> {
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, ERR_INVAL));
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
        };
        if new < 0 {
            return Err(VfError::failure(0, ERR_INVAL));
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
        const SETTABLE: AttrMask = AttrMask::MODE.union(AttrMask::SIZE);
        for (i, a) in attrs.iter().enumerate() {
            if !a.masks.difference(SETTABLE).is_empty() {
                return Err(VfError::unsupported(i));
            }
            self.setattr_one(a, i)?;
        }
        Ok(())
    }

    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE.union(AttrMask::SIZE);
        for (i, a) in attrs.iter().enumerate() {
            if !a.masks.difference(SETTABLE).is_empty() {
                return Err(VfError::unsupported(i));
            }
            let p = self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?;
            let md =
                std::fs::symlink_metadata(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if md.file_type().is_symlink() {
                // No portable lchmod/ltruncate: refuse rather than follow.
                return Err(VfError::unsupported(i));
            }
            self.setattr_one(a, i)?;
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
            let sp = self.vf_path(src).map_err(|e| e.with_index(i))?;
            let dp = self.vf_path(dst).map_err(|e| e.with_index(i))?;
            let dp = self
                .real_path(&self.root.join(dp))
                .map_err(|e| e.with_index(i))?;
            std::fs::rename(self.root.join(sp), dp)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
    }

    fn removev(&mut self, files: &[VfFile]) -> VfRes {
        for (i, f) in files.iter().enumerate() {
            let p = self
                .root
                .join(self.vf_path(f).map_err(|e| e.with_index(i))?);
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
            let p = self
                .real_path(
                    &self
                        .root
                        .join(self.vf_path(&a.file).map_err(|e| e.with_index(i))?),
                )
                .map_err(|e| e.with_index(i))?;
            std::fs::create_dir(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if a.masks.contains(AttrMask::MODE) {
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
            let new = self
                .real_path(&self.resolve(new))
                .map_err(|e| e.with_index(i))?;
            symlink(old, new).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
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
            let new = self
                .real_path(&self.resolve(new))
                .map_err(|e| e.with_index(i))?;
            std::fs::hard_link(self.resolve(old), new)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
    }

    fn dupv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            self.copy_extent(p).map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn lcopyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            let sp = self.resolve(&p.src_path);
            let md =
                std::fs::symlink_metadata(&sp).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if md.file_type().is_symlink() {
                let target =
                    std::fs::read_link(&sp).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                let dst = self
                    .real_path(&self.resolve(&p.dst_path))
                    .map_err(|e| e.with_index(i))?;
                symlink(&target, dst).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            } else {
                self.copy_extent(p).map_err(|e| e.with_index(i))?;
            }
        }
        Ok(())
    }

    fn write_adb(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let mut counts = Vec::with_capacity(patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            let path = self
                .real_path(&self.resolve(&p.path))
                .map_err(|e| e.with_index(i))?;
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            let mut written = 0usize;
            for b in 0..p.adb_block_count {
                let base = p.adb_offset.saturating_add(b as u64 * p.adb_block_size);
                if let Some(reloff) = p.adb_reloff_blocknum {
                    let adbn = (p.adb_block_num + b as u64).to_be_bytes();
                    file.write_all_at(&adbn, base + reloff)
                        .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                }
                if let Some(reloff) = p.adb_reloff_pattern
                    && !p.adb_pattern_data.is_empty()
                {
                    file.write_all_at(&p.adb_pattern_data, base + reloff)
                        .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                }
                written += 1;
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
                .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
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
                self.dupv(std::slice::from_ref(&pair))
                    .map_err(|e| e.with_index(0))?;
            }
        }
        Ok(())
    }
}
