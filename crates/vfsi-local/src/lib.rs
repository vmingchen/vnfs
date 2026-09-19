//! A [`VecFs`] implementation backed by the local filesystem (`std::fs`), so
//! the vectorized API also works on non-NFS filesystems.
//!
//! `"/"` maps to the `root` directory passed to [`DummyVecFs::new`]; all
//! writes stay under that root. Paths are normalized lexically (`.` and `..`
//! cannot escape the root), and path-based file operations resolve symlinks
//! and refuse targets outside the root (`ERR_ACCES`). Operations that never
//! follow symlinks (unlink, readlink, lstat, listing) operate on the
//! normalized path itself.
//!
//! On Linux, containment is enforced with `openat2(RESOLVE_IN_ROOT)` and
//! held directory descriptors, closing symlink-swap races as well as lexical
//! escapes. This requires Linux 5.6 or newer and a mounted `/proc` filesystem.

use std::collections::HashMap;
use std::fs::{File, FileTimes, OpenOptions};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
#[cfg(feature = "test-faults")]
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use vfsi_core::internal::ManyResults;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};
use vfsi_core::path::{cstring_from_bytes, normalize_bytes, path_bytes, path_from_bytes};
use vfsi_sync::*;

fn checked_offset(base: u64, delta: u64, index: usize) -> VfResult<u64> {
    base.checked_add(delta)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))
}

/// Open state for a descriptor on the local filesystem.
struct DummyOpen {
    file: File,
    path: PathBuf,
    cur_offset: u64,
    append: bool,
}

/// A path whose parent directory is held open, preventing a concurrent
/// symlink rename from redirecting a no-follow operation after validation.
struct AnchoredPath {
    path: PathBuf,
    #[allow(dead_code)]
    anchor: Option<File>,
    /// A missing final component must be opened with O_NOFOLLOW so it cannot
    /// be swapped for an escaping symlink after its parent was anchored.
    nofollow_on_open: bool,
}

impl AsRef<Path> for AnchoredPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for AnchoredPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

/// A local-filesystem [`VecFs`]. `"/"` is the `root` directory.
pub struct DummyVecFs {
    root: PathBuf,
    /// Canonical form of `root`, used for containment checks on platforms
    /// without Linux's dirfd-relative `openat2` resolution.
    #[cfg(not(target_os = "linux"))]
    root_canon: PathBuf,
    #[cfg(target_os = "linux")]
    root_dir: File,
    cwd: PathBuf,
    next_fd: i32,
    open_files: HashMap<i32, DummyOpen>,
    #[cfg(feature = "test-faults")]
    fault_injector: Option<Arc<dyn FaultInjector>>,
}

impl DummyVecFs {
    fn system_time(
        seconds: i64,
        nanoseconds: u32,
        index: usize,
    ) -> VfResult<std::time::SystemTime> {
        if nanoseconds >= 1_000_000_000 {
            return Err(VfError::failure(index, ERR_INVAL));
        }
        let seconds_only = if seconds >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
        }
        .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
        seconds_only
            .checked_add(Duration::from_nanos(u64::from(nanoseconds)))
            .ok_or_else(|| VfError::failure(index, ERR_INVAL))
    }

    fn getattrsv_impl(&mut self, attrs: &mut [VfAttrs], follow: bool) -> VfRes {
        for (i, a) in attrs.iter_mut().enumerate() {
            let lexical = self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?;
            if follow {
                let p = self.real_path(&lexical).map_err(|e| e.with_index(i))?;
                let md = std::fs::metadata(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                self.fill_attrs(a, &p, &md);
            } else {
                let p = self.no_follow_path(&lexical).map_err(|e| e.with_index(i))?;
                let md = std::fs::symlink_metadata(&p)
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                self.fill_attrs(a, &p, &md);
            }
        }
        Ok(())
    }

    /// Create a client rooted at `root` (created if missing).
    ///
    /// This compatibility constructor panics if the root cannot be prepared.
    /// New applications should prefer [`Self::try_new`].
    pub fn new(root: PathBuf) -> DummyVecFs {
        Self::try_new(root).expect("prepare dummy root")
    }

    /// Fallibly create a client rooted at `root` (created if missing).
    pub fn try_new(root: PathBuf) -> VfResult<DummyVecFs> {
        std::fs::create_dir_all(&root).map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        let root_canon = std::fs::canonicalize(&root)
            .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        #[cfg(target_os = "linux")]
        let root_dir =
            File::open(&root_canon).map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        Ok(DummyVecFs {
            root: root_canon.clone(),
            #[cfg(not(target_os = "linux"))]
            root_canon,
            #[cfg(target_os = "linux")]
            root_dir,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: HashMap::new(),
            #[cfg(feature = "test-faults")]
            fault_injector: None,
        })
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn set_fault_injector(&mut self, injector: Arc<dyn FaultInjector>) {
        self.fault_injector = Some(injector);
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_open_handle_count(&self) -> usize {
        self.open_files.len()
    }

    #[cfg(feature = "test-faults")]
    fn inject_open_fault(&self, point: OpenFaultPoint) -> VfResult<()> {
        self.fault_injector
            .as_ref()
            .map_or(Ok(()), |injector| injector.check(&point))
    }

    fn insert_open_file(&mut self, open: DummyOpen) -> VfResult<Fd> {
        vfsi_core::insert_fd(&mut self.next_fd, &mut self.open_files, open)
    }

    /// Map a (possibly cwd-relative) path onto the real filesystem.
    /// The result is lexically normalized and cannot escape `root` via `..`.
    fn resolve(&self, path: &Path) -> PathBuf {
        let suffix = if path.is_absolute() {
            path.strip_prefix("/").unwrap_or(path).to_path_buf()
        } else {
            self.cwd.join(path)
        };
        self.root
            .join(path_from_bytes(&normalize_bytes(path_bytes(&suffix))))
    }

    /// Resolve `p` (already mapped under the root) to a real path that is
    /// guaranteed to stay under the root, following symlinks for existing
    /// objects and verifying the parent for paths that do not exist yet.
    /// Dangling symlinks are resolved one level at a time so an external
    /// target is refused even when it does not exist.
    fn real_path(&self, p: &Path) -> VfResult<AnchoredPath> {
        self.real_path_depth(p, 0)
    }

    /// Resolve every parent component while deliberately leaving the final
    /// component untouched. This is used by lstat/unlink/readlink/rename so a
    /// final symlink is operated on, while a symlink in a parent directory
    /// can never redirect the operation outside the configured root.
    fn no_follow_path(&self, p: &Path) -> VfResult<AnchoredPath> {
        #[cfg(target_os = "linux")]
        {
            self.no_follow_path_linux(p)
        }
        #[cfg(not(target_os = "linux"))]
        {
            if p == self.root {
                return Ok(AnchoredPath {
                    path: self.root_canon.clone(),
                    anchor: None,
                    nofollow_on_open: false,
                });
            }
            let parent = p.parent().unwrap_or(&self.root);
            let canonical = std::fs::canonicalize(parent)
                .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
            if !canonical.starts_with(&self.root_canon) {
                return Err(VfError::failure(0, ERR_ACCES));
            }
            Ok(AnchoredPath {
                path: canonical.join(p.file_name().unwrap_or_default()),
                anchor: None,
                nofollow_on_open: true,
            })
        }
    }

    #[cfg(target_os = "linux")]
    fn no_follow_path_linux(&self, p: &Path) -> VfResult<AnchoredPath> {
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }

        const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
        const RESOLVE_IN_ROOT: u64 = 0x10;

        let relative = p
            .strip_prefix(&self.root)
            .map_err(|_| VfError::failure(0, ERR_ACCES))?;
        let parent = relative.parent().unwrap_or(Path::new(""));
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let name = relative.file_name();
        let c_parent =
            cstring_from_bytes(path_bytes(parent)).ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        let how = OpenHow {
            flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.root_dir.as_raw_fd(),
                c_parent.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            ) as i32
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            let errno = match error.raw_os_error() {
                Some(code) if code == libc::EXDEV || code == libc::ELOOP => ERR_ACCES,
                _ => Self::errno(&error),
            };
            return Err(VfError::failure(0, errno));
        }
        let anchor = unsafe { File::from_raw_fd(fd) };
        let mut path = PathBuf::from(format!("/proc/self/fd/{fd}"));
        if let Some(name) = name {
            path.push(name);
        } else {
            // Force normal path traversal through the procfs descriptor;
            // symlink_metadata on the bare fd entry would describe procfs's
            // magic link rather than the directory it anchors.
            path.push(".");
        }
        Ok(AnchoredPath {
            path,
            anchor: Some(anchor),
            nofollow_on_open: true,
        })
    }

    fn real_path_depth(&self, p: &Path, depth: usize) -> VfResult<AnchoredPath> {
        if depth > 40 {
            return Err(VfError::failure(0, ERR_ACCES)); // symlink loop / too deep
        }
        // Resolve the final symlink manually so absolute targets retain the
        // backend's chroot-relative semantics. The parent is held open while
        // it is inspected and read, eliminating parent-component swap races.
        let no_follow = self.no_follow_path(p)?;
        if let Ok(md) = std::fs::symlink_metadata(&no_follow)
            && md.file_type().is_symlink()
        {
            let target =
                std::fs::read_link(&no_follow).map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            let target_path = if target.is_absolute() {
                // Chroot semantics, matching the NFS backend: an absolute
                // target resolves inside the root (leading "/" is the root,
                // and ".." components are clamped), so it can never escape.
                let stripped = target.strip_prefix("/").unwrap_or(&target);
                self.root
                    .join(path_from_bytes(&normalize_bytes(path_bytes(stripped))))
            } else {
                p.parent().unwrap_or(Path::new("")).join(target)
            };
            return self.real_path_depth(&target_path, depth + 1);
        }

        #[cfg(target_os = "linux")]
        {
            #[repr(C)]
            struct OpenHow {
                flags: u64,
                mode: u64,
                resolve: u64,
            }
            const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
            const RESOLVE_IN_ROOT: u64 = 0x10;

            let relative = p
                .strip_prefix(&self.root)
                .map_err(|_| VfError::failure(0, ERR_ACCES))?;
            let relative = if relative.as_os_str().is_empty() {
                Path::new(".")
            } else {
                relative
            };
            let c_path = cstring_from_bytes(path_bytes(relative))
                .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
            let how = OpenHow {
                flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
                mode: 0,
                resolve: RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS,
            };
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    self.root_dir.as_raw_fd(),
                    c_path.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                ) as i32
            };
            if fd >= 0 {
                return Ok(AnchoredPath {
                    path: PathBuf::from(format!("/proc/self/fd/{fd}")),
                    anchor: Some(unsafe { File::from_raw_fd(fd) }),
                    nofollow_on_open: false,
                });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                // `no_follow` keeps the verified parent alive. Callers that
                // create the leaf add O_NOFOLLOW before opening it.
                return Ok(no_follow);
            }
            let errno = match error.raw_os_error() {
                Some(code) if code == libc::EXDEV || code == libc::ELOOP => ERR_ACCES,
                _ => Self::errno(&error),
            };
            Err(VfError::failure(0, errno))
        }

        #[cfg(not(target_os = "linux"))]
        {
            if let Ok(c) = std::fs::canonicalize(p) {
                if c.starts_with(&self.root_canon) {
                    return Ok(AnchoredPath {
                        path: c,
                        anchor: None,
                        nofollow_on_open: false,
                    });
                }
                return Err(VfError::failure(0, ERR_ACCES));
            }
            Ok(no_follow)
        }
    }

    fn protect_create_open(path: &AnchoredPath, options: &mut OpenOptions) {
        #[cfg(target_os = "linux")]
        if path.nofollow_on_open {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
    }

    fn errno(e: &std::io::Error) -> u32 {
        e.raw_os_error().map(|n| n as u32).unwrap_or(VF_ERR_RPC)
    }

    fn overflow(index: usize) -> VfError {
        VfError::failure(index, libc::EOVERFLOW as u32)
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
            _ => Err(VfError::unsupported(0)),
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
            _ => Err(VfError::failure(0, ERR_INVAL)),
        }
    }

    fn advance_offset(&mut self, file: &VfFile, new: u64) {
        if let Some(fd) = file.fd()
            && let Some(o) = self.open_files.get_mut(&fd)
        {
            o.cur_offset = new;
        }
    }

    /// Whether the anchored object has at least one extended attribute.
    fn has_xattr(path: &AnchoredPath) -> bool {
        let Some(cpath) = cstring_from_bytes(path_bytes(path.as_ref())) else {
            return false;
        };
        unsafe {
            if path.nofollow_on_open {
                libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) > 0
            } else {
                libc::listxattr(cpath.as_ptr(), std::ptr::null_mut(), 0) > 0
            }
        }
    }

    fn fill_attrs(&self, a: &mut VfAttrs, path: &AnchoredPath, md: &std::fs::Metadata) {
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
        if a.masks.contains(AttrMask::SIZE) {
            let mut options = OpenOptions::new();
            options.write(true);
            Self::protect_create_open(&p, &mut options);
            let f = options
                .open(&p)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            f.set_len(a.size)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        // Apply permissions last so a combined truncate+chmod cannot revoke
        // the write access needed by its own size update.
        if a.masks.contains(AttrMask::MODE) {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(a.mode & 0o7777))
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        if a.masks.intersects(AttrMask::ATIME | AttrMask::MTIME) {
            let mut options = OpenOptions::new();
            options.read(true);
            Self::protect_create_open(&p, &mut options);
            let file = options
                .open(&p)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            let mut times = FileTimes::new();
            if a.masks.contains(AttrMask::ATIME) {
                times = times.set_accessed(Self::system_time(a.atime_sec, a.atime_nsec, i)?);
            }
            if a.masks.contains(AttrMask::MTIME) {
                times = times.set_modified(Self::system_time(a.mtime_sec, a.mtime_nsec, i)?);
            }
            file.set_times(times)
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
                let mut options = OpenOptions::new();
                options.read(true);
                Self::protect_create_open(&p, &mut options);
                let f = options
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
            _ => return Err(VfError::unsupported(0)),
        };
        let requested = u64::try_from(op.length).map_err(|_| Self::overflow(0))?;
        checked_offset(off, requested, 0)?;
        let mut buf = vec![0u8; op.length];
        let n = file
            .read_at(&mut buf, off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        buf.truncate(n);
        if descriptor {
            let next = checked_offset(off, n as u64, 0)?;
            self.advance_offset(&op.file, next);
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
                Self::protect_create_open(&p, &mut opts);
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
            _ => return Err(VfError::unsupported(0)),
        };
        let requested = u64::try_from(op.data.len()).map_err(|_| Self::overflow(0))?;
        checked_offset(off, requested, 0)?;
        file.write_all_at(&op.data, off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        if descriptor {
            let next = checked_offset(off, op.data.len() as u64, 0)?;
            self.advance_offset(&op.file, next);
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
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        out: &mut Vec<VfAttrs>,
    ) -> VfRes {
        let reached_limit = |out: &Vec<VfAttrs>| max_count != 0 && out.len() >= max_count;
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            if reached_limit(out) {
                break;
            }
            let p = self.no_follow_path(&self.resolve(&current))?;
            let dir_metadata = std::fs::symlink_metadata(&p)
                .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
            if dir_metadata.file_type().is_symlink() {
                return Err(VfError::failure(0, ERR_ACCES));
            }
            let rd = std::fs::read_dir(&p).map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            let mut children = Vec::new();
            for entry in rd {
                let entry = entry.map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                if reached_limit(out) {
                    break;
                }
                let name = entry.file_name().as_bytes().to_vec();
                let path = current.join(path_from_bytes(&name));
                let mut attrs = VfAttrs {
                    file: VfFile::from_os_path(&path),
                    masks,
                    ..VfAttrs::default()
                };
                let metadata = std::fs::symlink_metadata(entry.path())
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                let real = self.no_follow_path(&self.resolve(&path))?;
                self.fill_attrs(&mut attrs, &real, &metadata);
                if recursive && attrs.ftype == VfType::Directory {
                    children.push(path);
                }
                out.push(attrs);
            }
            for child in children.into_iter().rev() {
                pending.push(child);
            }
        }
        Ok(())
    }

    fn copy_extent(&mut self, p: &ExtentPair) -> VfResult<()> {
        let sp = self.real_path(&self.resolve(&p.src_path))?;
        let dp = self.real_path(&self.resolve(&p.dst_path))?;
        let mut src_options = OpenOptions::new();
        src_options.read(true);
        Self::protect_create_open(&sp, &mut src_options);
        let src = src_options
            .open(&sp)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        let mut dst_options = OpenOptions::new();
        dst_options.write(true).create(true).truncate(false);
        Self::protect_create_open(&dp, &mut dst_options);
        let dst = dst_options
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
            so = so.checked_add(n as u64).ok_or_else(|| Self::overflow(0))?;
            doff = doff
                .checked_add(n as u64)
                .ok_or_else(|| Self::overflow(0))?;
            copied = copied
                .checked_add(n as u64)
                .ok_or_else(|| Self::overflow(0))?;
        }
        // Truncate any stale tail beyond what was copied (cp semantics).
        dst.set_len(doff)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        Ok(())
    }

    fn rm_one(&mut self, path: &Path, recursive: bool) -> VfResult<()> {
        let mut pending = vec![(path.to_path_buf(), false)];
        while let Some((current, visited)) = pending.pop() {
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeRemoveType { index: 0 })?;
            let file_type = self.file_type(&current)?;
            if file_type == VfType::Directory && recursive && !visited {
                pending.push((current.clone(), true));
                let entries = self.listdir(&current, AttrMask::default(), usize::MAX, false)?;
                for entry in entries.into_iter().rev() {
                    let child = entry
                        .file
                        .path()
                        .ok_or_else(|| VfError::failure(0, ERR_INVAL))?
                        .to_path_buf();
                    pending.push((child, false));
                }
            } else {
                self.unlink(&current)?;
            }
        }
        Ok(())
    }
}

impl VecFs for DummyVecFs {
    fn capabilities(&self) -> u64 {
        VF_CAP_UNIX_SEMANTICS
    }

    fn abs_path(&self, path: &Path) -> PathBuf {
        let root_rel = if path.is_absolute() {
            path.strip_prefix("/").unwrap_or(path).to_path_buf()
        } else {
            self.cwd.join(path)
        };
        path_from_bytes(&normalize_bytes(path_bytes(&root_rel)))
    }

    fn before_open_cleanup(&mut self, _index: usize, _file: &VfFile) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCleanup { index: _index })?;
        Ok(())
    }

    fn open_many(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeDispatch { chunk: 0 })?;
        let mut results = Vec::with_capacity(paths.len());
        for ((path, flags), mode) in paths.iter().zip(flags).zip(modes) {
            #[cfg(feature = "test-faults")]
            let index = results.len();
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeRegister { index }) {
                results.push(Err(error));
                break;
            }
            match self.open(path, *flags, *mode) {
                Ok(file) => {
                    #[cfg(feature = "test-faults")]
                    if let Err(error) =
                        self.inject_open_fault(OpenFaultPoint::AfterRegister { index })
                    {
                        let _ = self.close(&file);
                        results.push(Err(error));
                        break;
                    }
                    results.push(Ok(file));
                }
                Err(error) => {
                    results.push(Err(error));
                    break;
                }
            }
        }
        Ok(ManyResults::new(paths.len(), results))
    }

    fn open_by_path(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        use libc::O_CREAT;
        // `VfPathBase::Abs` treats the path as root-relative (like NFS);
        // `Cwd` resolves against the current directory.
        let resolved = match base {
            VfPathBase::Abs => self
                .root
                .join(path_from_bytes(&normalize_bytes(path_bytes(pathname)))),
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
            Self::protect_create_open(&p, &mut opts);
            match opts.open(&p) {
                Ok(f) => {
                    created = true;
                    f
                }
                Err(e)
                    if flags & libc::O_EXCL == 0
                        && e.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    let mut fallback = Self::open_options(flags);
                    Self::protect_create_open(&p, &mut fallback);
                    fallback
                        .open(&p)
                        .map_err(|e| VfError::failure(0, Self::errno(&e)))?
                }
                Err(e) => return Err(VfError::failure(0, Self::errno(&e))),
            }
        } else {
            let mut options = Self::open_options(flags);
            Self::protect_create_open(&p, &mut options);
            options
                .open(&p)
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?
        };
        if created {
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeSetPermissions { index: 0 })?;
            file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        }
        let fd = self.insert_open_file(DummyOpen {
            file,
            path: pathname.to_path_buf(),
            cur_offset: 0,
            append: flags & libc::O_APPEND != 0,
        })?;
        Ok(VfFile::from_fd(fd))
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

    fn sync_data(&mut self, tcf: &VfFile) -> VfResult<()> {
        let fd = tcf.fd().ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        self.open_files
            .get(&fd)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?
            .file
            .sync_data()
            .map_err(|error| VfError::failure(0, Self::errno(&error)))
    }

    fn sync_all(&mut self, tcf: &VfFile) -> VfResult<()> {
        let fd = tcf.fd().ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        self.open_files
            .get(&fd)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?
            .file
            .sync_all()
            .map_err(|error| VfError::failure(0, Self::errno(&error)))
    }

    fn chdir(&mut self, path: &Path) -> VfResult<()> {
        let p = self.real_path(&self.resolve(path))?;
        if !p.is_dir() {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        self.cwd = self.abs_path(path);
        Ok(())
    }

    fn getcwd(&self) -> PathBuf {
        Path::new("/").join(&self.cwd)
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
        let base = match whence {
            SeekFrom::Set => 0i128,
            SeekFrom::Cur => i128::from(cur),
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
                i128::from(len)
            }
            _ => return Err(VfError::failure(0, ERR_INVAL)),
        };
        let new = base
            .checked_add(i128::from(offset))
            .ok_or_else(|| Self::overflow(0))?;
        if new < 0 {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let new = u64::try_from(new).map_err(|_| Self::overflow(0))?;
        let reported = i64::try_from(new).map_err(|_| Self::overflow(0))?;
        self.advance_offset(tcf, new);
        Ok(reported)
    }

    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        self.getattrsv_impl(attrs, true)
    }

    fn lgetattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        self.getattrsv_impl(attrs, false)
    }

    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME);
        for (i, a) in attrs.iter().enumerate() {
            if !a.masks.difference(SETTABLE).is_empty() {
                return Err(VfError::unsupported(i));
            }
            self.setattr_one(a, i)?;
        }
        Ok(())
    }

    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME);
        for (i, a) in attrs.iter().enumerate() {
            if !a.masks.difference(SETTABLE).is_empty() {
                return Err(VfError::unsupported(i));
            }
            let p = self
                .no_follow_path(&self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?)
                .map_err(|e| e.with_index(i))?;
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
        dir: &Path,
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
            let sp = self
                .no_follow_path(&self.root.join(sp))
                .map_err(|e| e.with_index(i))?;
            let dp = self.vf_path(dst).map_err(|e| e.with_index(i))?;
            let dp = self
                .no_follow_path(&self.root.join(dp))
                .map_err(|e| e.with_index(i))?;
            std::fs::rename(sp, dp).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
    }

    fn removev(&mut self, files: &[VfFile]) -> VfRes {
        for (i, f) in files.iter().enumerate() {
            let p = self
                .no_follow_path(
                    &self
                        .root
                        .join(self.vf_path(f).map_err(|e| e.with_index(i))?),
                )
                .map_err(|e| e.with_index(i))?;
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
                .no_follow_path(
                    &self
                        .root
                        .join(self.vf_path(&a.file).map_err(|e| e.with_index(i))?),
                )
                .map_err(|e| e.with_index(i))?;
            std::fs::create_dir(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if a.masks.contains(AttrMask::MODE) {
                #[cfg(feature = "test-faults")]
                self.inject_open_fault(OpenFaultPoint::BeforeSetPermissions { index: i })?;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(a.mode & 0o7777))
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            }
        }
        Ok(())
    }

    fn symlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            let new = self
                .no_follow_path(&self.resolve(new))
                .map_err(|e| e.with_index(i))?;
            symlink(old, new).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
        }
        Ok(())
    }

    fn readlinkv(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let path = self
                .no_follow_path(&self.resolve(p))
                .map_err(|e| e.with_index(i))?;
            let target =
                std::fs::read_link(path).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            out.push(path_bytes(&target).to_vec());
        }
        Ok(out)
    }

    fn hardlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            let new = self
                .no_follow_path(&self.resolve(new))
                .map_err(|e| e.with_index(i))?;
            let old = self
                .no_follow_path(&self.resolve(old))
                .map_err(|e| e.with_index(i))?;
            std::fs::hard_link(old, new).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
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
            let sp = self
                .no_follow_path(&self.resolve(&p.src_path))
                .map_err(|e| e.with_index(i))?;
            let md =
                std::fs::symlink_metadata(&sp).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if md.file_type().is_symlink() {
                let target =
                    std::fs::read_link(&sp).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                let dst = self
                    .no_follow_path(&self.resolve(&p.dst_path))
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
            let pattern_len =
                u64::try_from(p.adb_pattern_data.len()).map_err(|_| Self::overflow(i))?;
            let mut layout = Vec::with_capacity(p.adb_block_count);
            for b in 0..p.adb_block_count {
                let relative = (b as u64)
                    .checked_mul(p.adb_block_size)
                    .ok_or_else(|| Self::overflow(i))?;
                let base = p
                    .adb_offset
                    .checked_add(relative)
                    .ok_or_else(|| Self::overflow(i))?;
                let block_number = p
                    .adb_block_num
                    .checked_add(b as u64)
                    .ok_or_else(|| Self::overflow(i))?;
                let number_offset = p
                    .adb_reloff_blocknum
                    .map(|field| {
                        let offset = base.checked_add(field).ok_or_else(|| Self::overflow(i))?;
                        offset.checked_add(8).ok_or_else(|| Self::overflow(i))?;
                        Ok(offset)
                    })
                    .transpose()?;
                let pattern_offset = p
                    .adb_reloff_pattern
                    .map(|field| {
                        let offset = base.checked_add(field).ok_or_else(|| Self::overflow(i))?;
                        offset
                            .checked_add(pattern_len)
                            .ok_or_else(|| Self::overflow(i))?;
                        Ok(offset)
                    })
                    .transpose()?;
                layout.push((block_number, number_offset, pattern_offset));
            }
            let path = self
                .real_path(&self.resolve(&p.path))
                .map_err(|e| e.with_index(i))?;
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(false);
            Self::protect_create_open(&path, &mut options);
            let file = options
                .open(&path)
                .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            let mut written = 0usize;
            for (block_number, number_offset, pattern_offset) in layout {
                if let Some(offset) = number_offset {
                    let adbn = block_number.to_be_bytes();
                    file.write_all_at(&adbn, offset)
                        .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                }
                if let Some(offset) = pattern_offset
                    && !p.adb_pattern_data.is_empty()
                {
                    file.write_all_at(&p.adb_pattern_data, offset)
                        .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                }
                written += 1;
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
        let mut pending = vec![(src_dir.to_path_buf(), dst.to_path_buf())];
        while let Some((source, destination)) = pending.pop() {
            let entries = self.listdir(&source, masks, 0, false)?;
            let mut directories = Vec::new();
            for entry in entries {
                let name = entry
                    .file
                    .path()
                    .and_then(|path| path.file_name())
                    .map(|name| name.as_bytes().to_vec())
                    .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
                let source_child = source.join(path_from_bytes(&name));
                let destination_child = destination.join(path_from_bytes(&name));
                if entry.ftype == VfType::Directory {
                    self.ensure_dir(&destination_child, 0o755)
                        .map_err(|error| error.with_index(0))?;
                    directories.push((source_child, destination_child));
                } else if entry.ftype == VfType::Symlink && symlinks {
                    let target = self
                        .readlink(&source_child)
                        .map_err(|error| error.with_index(0))?;
                    self.symlink(&path_from_bytes(&target), &destination_child)
                        .map_err(|error| error.with_index(0))?;
                } else {
                    let pair =
                        ExtentPair::from_os_paths(&source_child, 0, &destination_child, 0, None);
                    self.dupv(std::slice::from_ref(&pair))
                        .map_err(|error| error.with_index(0))?;
                }
            }
            for directory in directories.into_iter().rev() {
                pending.push(directory);
            }
        }
        Ok(())
    }
}

/// Compatibility module matching the historical `vnfs::dummy_vecfs` path.
pub mod dummy_vecfs {
    pub use super::DummyVecFs;
}

#[cfg(test)]
mod contract_tests;
