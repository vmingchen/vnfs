//! Shared local namespace, handle ownership and ordering machinery.
//!
//! Backend implementers supply a descriptor executor to [`LocalBackend::new`].
//! Applications normally use `vfsi-posix` or `vfsi-uring` instead.
//!
//! `"/"` maps to the `root` directory passed to [`LocalBackend::new`]; all
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
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::*;

use vfsi_core::internal::ManyResults;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};
use vfsi_core::path::{cstring_from_bytes, normalize_bytes, path_bytes, path_from_bytes};

#[cfg(test)]
std::thread_local! {
    static XATTR_QUERY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static XATTR_QUERY_ERROR: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

pub mod io;

fn checked_offset(base: u64, delta: u64, index: usize) -> VfResult<u64> {
    base.checked_add(delta)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))
}

// Sort by object identity and start offset, so aliases are checked together
// without comparing every pair of unrelated writes. Empty writes cannot race.
fn writes_overlap(ranges: &mut Vec<((u64, u64), u64, u64)>) -> bool {
    ranges.retain(|&(_, start, end)| start < end);
    ranges.sort_unstable();
    ranges
        .windows(2)
        .any(|pair| pair[0].0 == pair[1].0 && pair[1].1 < pair[0].2)
}

#[cfg(test)]
mod io_range_tests {
    use super::writes_overlap;

    #[test]
    fn sorted_overlap_checks_preserve_alias_empty_and_adjacent_semantics() {
        for (mut ranges, expected) in [
            (vec![((1, 1), 8, 12), ((1, 1), 0, 8)], false),
            (vec![((1, 1), 8, 12), ((1, 1), 0, 9)], true),
            (vec![((1, 1), 0, 20), ((1, 1), 2, 3)], true),
            (vec![((1, 1), 0, 20), ((1, 1), 2, 2)], false),
            (vec![((1, 1), 0, 8), ((2, 1), 0, 8)], false),
            (vec![((1, 1), 0, 8), ((1, 2), 0, 8)], false),
        ] {
            assert_eq!(writes_overlap(&mut ranges), expected);
        }
        // Large/reversed vectors exercise the scalable sorted path without
        // brittle wall-clock assertions.
        let mut ranges: Vec<_> = (0..32_768)
            .rev()
            .map(|i| ((1, 1), i * 2, i * 2 + 1))
            .collect();
        assert!(!writes_overlap(&mut ranges));
        ranges.push(((1, 1), 100, 102));
        assert!(writes_overlap(&mut ranges));
    }
}

#[cfg(target_os = "linux")]
fn noreplace_error_code(
    error: &std::io::Error,
    source_is_directory: bool,
    source: &Path,
    destination: &Path,
) -> u32 {
    match error.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::ENOSYS) => VF_ERR_UNSUPPORTED,
        // Linux also uses EINVAL for a directory being moved beneath itself.
        // The syscall flag is a known-valid constant, so other EINVAL cases
        // identify a kernel/filesystem without RENAME_NOREPLACE support.
        Some(libc::EINVAL) if !(source_is_directory && destination.starts_with(source)) => {
            VF_ERR_UNSUPPORTED
        }
        _ => error
            .raw_os_error()
            .map(|errno| errno as u32)
            .unwrap_or(VF_ERR_RPC),
    }
}

/// Open state for a descriptor on the local filesystem.
struct LocalOpen {
    file: Arc<File>,
    // Device/inode identity cannot change while this descriptor pins the object.
    // Cache only identity, never size/timestamps or pathname-based metadata.
    identity: std::cell::Cell<Option<(u64, u64)>>,
    path: PathBuf,
    cur_offset: u64,
    append: bool,
}

impl LocalOpen {
    fn identity(&self) -> std::io::Result<(u64, u64)> {
        if let Some(identity) = self.identity.get() {
            return Ok(identity);
        }
        let metadata = self.file.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        self.identity.set(Some(identity));
        Ok(identity)
    }
}

/// Owned iterator retained while application callbacks run without the lock.
struct LocalDirectoryCursor {
    entries: std::fs::ReadDir,
    virtual_dir: PathBuf,
    // DirEntry::path() may refer through this held /proc/self/fd anchor.
    _anchor: AnchoredPath,
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

/// A local-filesystem [`VectorBackend`]. `"/"` is the `root` directory.
pub struct LocalBackend {
    io: Box<dyn io::DescriptorIo>,
    root: PathBuf,
    /// Canonical form of `root`, used for containment checks on platforms
    /// without Linux's dirfd-relative `openat2` resolution.
    #[cfg(any(not(target_os = "linux"), test))]
    root_canon: PathBuf,
    root_dir: File,
    cwd: PathBuf,
    next_fd: i32,
    open_files: HashMap<i32, LocalOpen>,
    #[cfg(feature = "test-faults")]
    fault_injector: Option<Arc<dyn FaultInjector>>,
}

impl LocalBackend {
    /// Hold the entire directory, resolving every component in one no-symlink
    /// openat2. Reading through its procfs descriptor closes validation races.
    fn strict_directory_path(&self, dir: &Path) -> VfResult<AnchoredPath> {
        #[cfg(target_os = "linux")]
        {
            #[repr(C)]
            struct OpenHow {
                flags: u64,
                mode: u64,
                resolve: u64,
            }
            let resolved = self.resolve(dir);
            let relative = resolved
                .strip_prefix(&self.root)
                .map_err(|_| VfError::client(0, ERR_ACCES))?;
            let relative = if relative.as_os_str().is_empty() {
                Path::new(".")
            } else {
                relative
            };
            let path = cstring_from_bytes(path_bytes(relative))
                .ok_or_else(|| VfError::client(0, ERR_INVAL))?;
            let how = OpenHow {
                flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
                mode: 0,
                resolve: 0x10 | 0x04, // RESOLVE_IN_ROOT | RESOLVE_NO_SYMLINKS
            };
            // SAFETY: the CString and repr(C) OpenHow remain live for synchronous
            // openat2; root_dir owns the borrowed descriptor.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    self.root_dir.as_raw_fd(),
                    path.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                ) as i32
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                return Err(match error.raw_os_error() {
                    Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP) => VfError::unsupported(0),
                    _ => VfError::client(0, Self::errno(&error)),
                });
            }
            Ok(AnchoredPath {
                path: PathBuf::from(format!("/proc/self/fd/{fd}/.")),
                // SAFETY: the preceding open succeeded and returned a new descriptor,
                // transferred exactly once to this owning File.
                anchor: Some(unsafe { File::from_raw_fd(fd) }),
                nofollow_on_open: false,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = dir;
            Err(VfError::unsupported(0))
        }
    }
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
            if let FileOperand::Fd(fd) = &a.file {
                let open = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
                let md = open
                    .file
                    .metadata()
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                self.fill_attrs(a, None, &md)
                    .map_err(|error| error.with_index(i))?;
                if a.masks.contains(AttrMask::NAMED_ATTR) {
                    use std::os::fd::AsRawFd;
                    a.has_named_attr = Self::has_xattr_fd(open.file.as_raw_fd())
                        .map_err(|error| error.with_index(i))?;
                    a.returned.insert(AttrMask::NAMED_ATTR);
                }
                continue;
            }
            let lexical = self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?;
            if follow {
                let p = self.real_path(&lexical).map_err(|e| e.with_index(i))?;
                let md = std::fs::metadata(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                self.fill_attrs(a, Some(&p), &md)
                    .map_err(|error| error.with_index(i))?;
            } else {
                let p = self.no_follow_path(&lexical).map_err(|e| e.with_index(i))?;
                let md = std::fs::symlink_metadata(&p)
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
                self.fill_attrs(a, Some(&p), &md)
                    .map_err(|error| error.with_index(i))?;
            }
        }
        Ok(())
    }

    /// Prepare a root (created if missing) with an explicit synchronous executor.
    pub fn new(root: PathBuf, engine: impl io::DescriptorIo + 'static) -> VfResult<Self> {
        std::fs::create_dir_all(&root).map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        let root_canon = std::fs::canonicalize(&root)
            .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        let root_dir =
            File::open(&root_canon).map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        Ok(LocalBackend {
            io: Box::new(engine),
            root: root_canon.clone(),
            #[cfg(any(not(target_os = "linux"), test))]
            root_canon,
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

    fn insert_open_file(&mut self, open: LocalOpen) -> VfResult<Fd> {
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
            self.no_follow_path_fallback(p)
        }
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn no_follow_path_fallback(&self, p: &Path) -> VfResult<AnchoredPath> {
        let relative = p
            .strip_prefix(&self.root)
            .map_err(|_| VfError::failure(0, ERR_ACCES))?;
        let parent = relative.parent().unwrap_or(Path::new(""));
        let parent_path = if parent.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(parent)
        };
        let canonical = std::fs::canonicalize(parent_path)
            .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        let canonical_relative = canonical
            .strip_prefix(&self.root_canon)
            .map_err(|_| VfError::failure(0, ERR_ACCES))?;

        // Walk the canonical in-root parent from the held root descriptor.
        // Each component is opened without following symlinks, so a later
        // rename or symlink swap cannot redirect the final path.
        let mut anchor = self
            .root_dir
            .try_clone()
            .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        for component in canonical_relative.components() {
            let std::path::Component::Normal(name) = component else {
                continue;
            };
            let name = cstring_from_bytes(name.as_bytes())
                .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
            // SAFETY: anchor owns the live directory descriptor and name is NUL-
            // terminated for this synchronous openat.
            let next = unsafe {
                libc::openat(
                    anchor.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if next < 0 {
                return Err(VfError::failure(
                    0,
                    Self::errno(&std::io::Error::last_os_error()),
                ));
            }
            // SAFETY: the preceding open succeeded and returned a new descriptor,
            // transferred exactly once to this owning File.
            anchor = unsafe { File::from_raw_fd(next) };
        }
        let mut path = PathBuf::from(format!("/dev/fd/{}", anchor.as_raw_fd()));
        if let Some(name) = relative.file_name() {
            path.push(name);
        } else {
            path.push(".");
        }
        Ok(AnchoredPath {
            path,
            anchor: Some(anchor),
            nofollow_on_open: true,
        })
    }

    /// Open a directory while following parent symlinks within the root, but
    /// atomically reject a symlink in the final component.
    fn open_directory_no_follow_final(&self, p: &Path) -> VfResult<AnchoredPath> {
        #[cfg(target_os = "linux")]
        let fd = {
            #[repr(C)]
            struct OpenHow {
                flags: u64,
                mode: u64,
                resolve: u64,
            }
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
                flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
                    as u64,
                mode: 0,
                resolve: 0x10 | 0x02, // RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS
            };
            // SAFETY: the CString, live root descriptor and initialized syscall
            // arguments remain valid until this synchronous directory open returns.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    self.root_dir.as_raw_fd(),
                    c_path.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                ) as i32
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                let errno = if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
                    && self.no_follow_path(p).ok().is_some_and(|path| {
                        std::fs::symlink_metadata(path)
                            .is_ok_and(|metadata| metadata.file_type().is_symlink())
                    }) {
                    ERR_ACCES
                } else {
                    Self::errno(&error)
                };
                return Err(VfError::failure(0, errno));
            }
            fd
        };
        #[cfg(not(target_os = "linux"))]
        let fd = {
            let parent_anchored = self.no_follow_path(p)?;
            let path = cstring_from_bytes(path_bytes(parent_anchored.as_ref()))
                .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
            // SAFETY: the CString, live root descriptor and initialized syscall
            // arguments remain valid until this synchronous directory open returns.
            let fd = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                let errno = if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
                    && std::fs::symlink_metadata(&parent_anchored)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    ERR_ACCES
                } else {
                    Self::errno(&error)
                };
                return Err(VfError::failure(0, errno));
            }
            fd
        };
        // SAFETY: the preceding open succeeded and returned a new descriptor,
        // transferred exactly once to this owning File.
        let anchor = unsafe { File::from_raw_fd(fd) };
        let path = if cfg!(target_os = "linux") {
            PathBuf::from(format!("/proc/self/fd/{}/.", anchor.as_raw_fd()))
        } else {
            PathBuf::from(format!("/dev/fd/{}/.", anchor.as_raw_fd()))
        };
        Ok(AnchoredPath {
            path,
            anchor: Some(anchor),
            nofollow_on_open: false,
        })
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
        // SAFETY: the CString and repr(C) OpenHow remain live for synchronous openat2;
        // root_dir owns the borrowed descriptor.
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
        // SAFETY: the preceding open succeeded and returned a new descriptor,
        // transferred exactly once to this owning File.
        let anchor = unsafe { File::from_raw_fd(fd) };
        let mut path = PathBuf::from(format!("/proc/self/fd/{fd}"));
        if let Some(name) = name {
            path.push(name);
        } else {
            // Force normal path traversal through the procfs descriptor;
            // symlink_attrs on the bare fd entry would describe procfs's
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
            // SAFETY: the CString and repr(C) OpenHow remain live for synchronous
            // openat2; root_dir owns the borrowed descriptor.
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
                    // SAFETY: the preceding open succeeded and returned a new
                    // descriptor, transferred exactly once to this owning File.
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
            if let Ok(canonical) = std::fs::canonicalize(p) {
                if canonical.starts_with(&self.root_canon) {
                    return Ok(AnchoredPath {
                        path: canonical,
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
        if path.nofollow_on_open {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
    }

    /// Change permissions through a pinned object without following the leaf.
    fn set_permissions_no_follow(
        path: &AnchoredPath,
        mode: u32,
        directory_only: bool,
        index: usize,
    ) -> VfResult<()> {
        #[cfg(target_os = "linux")]
        {
            // O_PATH allows chmod of files that grant no read access. With
            // O_NOFOLLOW it pins the leaf itself, including a concurrently
            // inserted symlink, which is rejected below.
            let name = cstring_from_bytes(path_bytes(path))
                .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
            let mut flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            if directory_only {
                flags |= libc::O_DIRECTORY;
            }
            // SAFETY: the owned path and NUL-terminated name remain live for this syscall.
            let fd = unsafe { libc::open(name.as_ptr(), flags) };
            if fd < 0 {
                return Err(VfError::failure(
                    index,
                    Self::errno(&std::io::Error::last_os_error()),
                ));
            }
            // SAFETY: open succeeded and transferred a new descriptor exactly once.
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file
                .metadata()
                .map_err(|error| VfError::failure(index, Self::errno(&error)))?;
            if metadata.file_type().is_symlink() {
                return Err(VfError::unsupported(index));
            }
            let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            std::fs::set_permissions(
                descriptor_path,
                std::fs::Permissions::from_mode(mode & 0o7777),
            )
            .map_err(|error| VfError::failure(index, Self::errno(&error)))
        }

        #[cfg(not(target_os = "linux"))]
        {
            if directory_only {
                // Use the held parent descriptor and reject symlink traversal.
                // fchmodat changes the entry without opening a mode-restricted
                // directory for reading.
                let parent = path
                    .anchor
                    .as_ref()
                    .ok_or_else(|| VfError::unsupported(index))?;
                let name = path
                    .file_name()
                    .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
                let name = cstring_from_bytes(path_bytes(Path::new(name)))
                    .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
                // SAFETY: the owned parent/path and NUL-terminated name remain live for the
                // synchronous syscall; metadata output storage is writable when used.
                let result = unsafe {
                    libc::fchmodat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        (mode & 0o7777) as libc::mode_t,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if result != 0 {
                    return Err(VfError::failure(
                        index,
                        Self::errno(&std::io::Error::last_os_error()),
                    ));
                }
                // Some systems accept AT_SYMLINK_NOFOLLOW but treat chmod on a
                // symlink as a successful no-op. Verify the anchored leaf itself
                // is still a directory before reporting that mkdir's mode applied.
                // SAFETY: this libc output structure contains C integer fields and is valid
                // when zero-initialized.
                let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: the owned parent/path and NUL-terminated name remain live for the
                // synchronous syscall; metadata output storage is writable when used.
                let result = unsafe {
                    libc::fstatat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        &mut metadata,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if result != 0 {
                    return Err(VfError::failure(
                        index,
                        Self::errno(&std::io::Error::last_os_error()),
                    ));
                }
                if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    return Err(VfError::failure(index, ERR_ACCES));
                }
                return Ok(());
            }

            use std::os::unix::fs::OpenOptionsExt;
            let mut options = OpenOptions::new();
            options.read(true);
            let mut flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
            if directory_only {
                flags |= libc::O_DIRECTORY;
            }
            options.custom_flags(flags);
            let file = options
                .open(path)
                .map_err(|error| VfError::failure(index, Self::errno(&error)))?;
            file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|error| VfError::failure(index, Self::errno(&error)))
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

    /// The real path a `FileOperand` refers to.
    fn tcfile_path(&self, f: &FileOperand) -> VfResult<PathBuf> {
        match f {
            FileOperand::Fd(fd) => {
                let open = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
                Ok(self.resolve(&open.path))
            }
            FileOperand::Path { .. } | FileOperand::Cwd | FileOperand::CwdPath(_) => {
                let path = self.vf_path(f)?;
                Ok(self.root.join(path))
            }
            FileOperand::Saved => Err(VfError::unsupported(0)),
            _ => Err(VfError::unsupported(0)),
        }
    }

    fn resolve_offset(&self, file: &FileOperand, off: VfOffset, len: u64) -> VfResult<u64> {
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

    fn advance_offset(&mut self, file: &FileOperand, new: u64) {
        if let Some(fd) = file.fd()
            && let Some(o) = self.open_files.get_mut(&fd)
        {
            o.cur_offset = new;
        }
    }

    fn xattr_result(result: libc::ssize_t) -> VfResult<bool> {
        #[cfg(test)]
        if let Some(errno) = XATTR_QUERY_ERROR.with(std::cell::Cell::take) {
            return Err(VfError::failure(0, errno as u32));
        }
        if result >= 0 {
            Ok(result > 0)
        } else {
            Err(VfError::failure(
                0,
                Self::errno(&std::io::Error::last_os_error()),
            ))
        }
    }

    /// Whether the anchored object has at least one extended attribute.
    fn has_xattr(path: &AnchoredPath) -> VfResult<bool> {
        #[cfg(test)]
        XATTR_QUERY_COUNT.with(|count| count.set(count.get() + 1));
        let cpath = cstring_from_bytes(path_bytes(path.as_ref()))
            .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        // SAFETY: the path/descriptor is live; a null buffer with size zero requests
        // the xattr list size without writing memory.
        let result = unsafe {
            if path.nofollow_on_open {
                libc::llistxattr(cpath.as_ptr(), std::ptr::null_mut(), 0)
            } else {
                libc::listxattr(cpath.as_ptr(), std::ptr::null_mut(), 0)
            }
        };
        Self::xattr_result(result)
    }

    fn has_xattr_fd(fd: std::os::fd::RawFd) -> VfResult<bool> {
        // SAFETY: the path/descriptor is live; a null buffer with size zero requests
        // the xattr list size without writing memory.
        let result = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
        Self::xattr_result(result)
    }

    fn fill_attrs(
        &self,
        a: &mut VfAttrs,
        path: Option<&AnchoredPath>,
        md: &std::fs::Metadata,
    ) -> VfResult<()> {
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
        a.has_named_attr = false;
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
        if a.masks.contains(AttrMask::FS_ID) {
            a.fs_id = u128::from(md.dev());
            a.returned.insert(AttrMask::FS_ID);
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
        if a.masks.contains(AttrMask::NAMED_ATTR)
            && let Some(path) = path
        {
            a.has_named_attr = Self::has_xattr(path)?;
            a.returned.insert(AttrMask::NAMED_ATTR);
        }
        Ok(())
    }

    fn ownership_ids(a: &VfAttrs, index: usize) -> VfResult<(Option<u32>, Option<u32>)> {
        let uid = a.masks.contains(AttrMask::UID).then_some(a.uid);
        let gid = a.masks.contains(AttrMask::GID).then_some(a.gid);
        if uid == Some(u32::MAX) || gid == Some(u32::MAX) {
            return Err(VfError::failure(index, ERR_INVAL));
        }
        Ok((uid, gid))
    }

    fn ownership_result(result: std::io::Result<()>, index: usize) -> VfResult<()> {
        result.map_err(|error| {
            let code = if error.kind() == std::io::ErrorKind::InvalidInput {
                ERR_INVAL
            } else {
                Self::errno(&error)
            };
            VfError::failure(index, code)
        })
    }

    fn chown_path(a: &VfAttrs, path: &AnchoredPath, follow: bool, index: usize) -> VfResult<()> {
        if !a.masks.intersects(AttrMask::UID | AttrMask::GID) {
            return Ok(());
        }
        // A following ownership change requires an existing, resolved target.
        // The missing-leaf fallback only pins its parent; following that leaf
        // would permit a concurrently inserted symlink to escape the root.
        if follow && path.nofollow_on_open {
            return Err(VfError::failure(index, ERR_NOENT));
        }
        let (uid, gid) = Self::ownership_ids(a, index)?;
        let result = if follow {
            std::os::unix::fs::chown(path.as_ref(), uid, gid)
        } else {
            std::os::unix::fs::lchown(path.as_ref(), uid, gid)
        };
        Self::ownership_result(result, index)
    }

    fn setattr_one(&mut self, a: &VfAttrs, i: usize) -> VfResult<()> {
        if let FileOperand::Fd(fd) = &a.file {
            let open = self
                .open_files
                .get(fd)
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            if a.masks.contains(AttrMask::SIZE) {
                open.file
                    .set_len(a.size)
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            }
            if a.masks.intersects(AttrMask::UID | AttrMask::GID) {
                let (uid, gid) = Self::ownership_ids(a, i)?;
                Self::ownership_result(std::os::unix::fs::fchown(open.file.as_ref(), uid, gid), i)?;
            }
            if a.masks.contains(AttrMask::MODE) {
                open.file
                    .set_permissions(std::fs::Permissions::from_mode(a.mode & 0o7777))
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            }
            if a.masks.intersects(AttrMask::ATIME | AttrMask::MTIME) {
                let mut times = FileTimes::new();
                if a.masks.contains(AttrMask::ATIME) {
                    times = times.set_accessed(Self::system_time(a.atime_sec, a.atime_nsec, i)?);
                }
                if a.masks.contains(AttrMask::MTIME) {
                    times = times.set_modified(Self::system_time(a.mtime_sec, a.mtime_nsec, i)?);
                }
                open.file
                    .set_times(times)
                    .map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            }
            return Ok(());
        }
        let p = self
            .real_path(&self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?)
            .map_err(|e| e.with_index(i))?;
        self.setattr_path(a, i, &p, true)
    }

    fn setattr_path(
        &mut self,
        a: &VfAttrs,
        i: usize,
        path: &AnchoredPath,
        follow: bool,
    ) -> VfResult<()> {
        if a.masks.contains(AttrMask::SIZE) {
            let mut options = OpenOptions::new();
            options.write(true);
            Self::protect_create_open(path, &mut options);
            let file = options
                .open(path)
                .map_err(|error| VfError::failure(i, Self::errno(&error)))?;
            file.set_len(a.size)
                .map_err(|error| VfError::failure(i, Self::errno(&error)))?;
        }
        if a.masks.intersects(AttrMask::UID | AttrMask::GID) {
            Self::chown_path(a, path, follow, i)?;
        }
        // Apply permissions last so a combined truncate+chmod cannot revoke
        // the write access needed by its own size update.
        if a.masks.contains(AttrMask::MODE) {
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeSetPermissions { index: i })?;
            if !follow || path.nofollow_on_open {
                Self::set_permissions_no_follow(path, a.mode, false, i)?;
            } else {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(a.mode & 0o7777))
                    .map_err(|error| VfError::failure(i, Self::errno(&error)))?;
            }
        }
        if a.masks.intersects(AttrMask::ATIME | AttrMask::MTIME) {
            let mut options = OpenOptions::new();
            options.read(true);
            Self::protect_create_open(path, &mut options);
            let file = options
                .open(path)
                .map_err(|error| VfError::failure(i, Self::errno(&error)))?;
            let mut times = FileTimes::new();
            if a.masks.contains(AttrMask::ATIME) {
                times = times.set_accessed(Self::system_time(a.atime_sec, a.atime_nsec, i)?);
            }
            if a.masks.contains(AttrMask::MTIME) {
                times = times.set_modified(Self::system_time(a.mtime_sec, a.mtime_nsec, i)?);
            }
            file.set_times(times)
                .map_err(|error| VfError::failure(i, Self::errno(&error)))?;
        }
        Ok(())
    }

    fn readv_one(&mut self, op: &ReadOp) -> VfResult<ReadResult> {
        let mut data = vec![0u8; op.length];
        let result = self.readv_one_into(op, &mut data)?;
        data.truncate(result.read);
        Ok(ReadResult {
            file: result.file,
            offset: result.offset,
            data,
            eof: result.eof,
        })
    }

    // Duplicate cursor requests need sequential offset resolution. Independent
    // borrowed and owned reads share the same descriptor/offset preflight.
    fn independent_reads(reads: &[ReadOp]) -> bool {
        let mut cursors = std::collections::HashSet::new();
        reads.iter().all(|op| {
            op.file.is_fd() && (!matches!(op.offset, VfOffset::Cur) || cursors.insert(op.file.fd()))
        })
    }

    fn prepare_read<'a>(
        open_files: &'a HashMap<i32, LocalOpen>,
        op: &ReadOp,
        index: usize,
    ) -> VfResult<(&'a Arc<File>, u64)> {
        let open = open_files
            .get(&op.file.fd().unwrap())
            .ok_or_else(|| VfError::failure(index, ERR_EBADF))?;
        let file = &open.file;
        let offset = match op.offset {
            VfOffset::At(offset) => offset,
            VfOffset::Cur => open.cur_offset,
            VfOffset::End => file
                .metadata()
                .map_err(|e| VfError::failure(index, Self::errno(&e)))?
                .len(),
            _ => return Err(VfError::failure(index, ERR_INVAL)),
        };
        checked_offset(offset, op.length as u64, index)?;
        Ok((file, offset))
    }

    fn readv_one_into(&mut self, op: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        if op.length != buffer.len() {
            return Err(VfError::client(0, ERR_INVAL));
        }
        let (file, off, descriptor) = match &op.file {
            FileOperand::Fd(fd) => {
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
            FileOperand::Path { .. } | FileOperand::Cwd | FileOperand::CwdPath(_) => {
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
            FileOperand::Saved => {
                return Err(VfError::unsupported(0));
            }
            _ => return Err(VfError::unsupported(0)),
        };
        let requested = u64::try_from(op.length).map_err(|_| Self::overflow(0))?;
        checked_offset(off, requested, 0)?;
        let n = file
            .read_at(buffer, off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        if descriptor && matches!(op.offset, VfOffset::Cur) {
            let next = checked_offset(off, n as u64, 0)?;
            self.advance_offset(&op.file, next);
        }
        Ok(ReadIntoResult {
            file: op.file.clone(),
            offset: off,
            read: n,
            eof: n < op.length,
        })
    }

    fn writev_one(&mut self, op: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteResult> {
        let (file, off, descriptor) = match op.file() {
            FileOperand::Fd(fd) => {
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
                    self.resolve_offset(op.file(), op.offset(), len)?
                };
                let f = o
                    .file
                    .try_clone()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                (f, off, true)
            }
            FileOperand::Path { .. } | FileOperand::Cwd | FileOperand::CwdPath(_) => {
                let p = self.real_path(&self.tcfile_path(op.file())?)?;
                let mut opts = OpenOptions::new();
                opts.write(true);
                if op.creates() {
                    opts.create(true);
                }
                if op.truncates() {
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
                let off = self.resolve_offset(op.file(), op.offset(), len)?;
                (f, off, false)
            }
            FileOperand::Saved => {
                return Err(VfError::unsupported(0));
            }
            _ => return Err(VfError::unsupported(0)),
        };
        let requested = u64::try_from(op.data().len()).map_err(|_| Self::overflow(0))?;
        checked_offset(off, requested, 0)?;
        file.write_all_at(op.data(), off)
            .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
        if descriptor && !matches!(op.offset(), VfOffset::At(_)) {
            let next = checked_offset(off, op.data().len() as u64, 0)?;
            self.advance_offset(op.file(), next);
        }
        Ok(WriteResult {
            file: op.file().clone(),
            offset: off,
            written: op.data().len(),
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
            let p = self.open_directory_no_follow_final(&self.resolve(&current))?;
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
                    file: FileOperand::from_os_path(&path),
                    masks,
                    ..VfAttrs::default()
                };
                let metadata = std::fs::symlink_metadata(entry.path())
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                let real = self.no_follow_path(&self.resolve(&path))?;
                self.fill_attrs(&mut attrs, Some(&real), &metadata)
                    .map_err(|error| error.with_index(out.len()))?;
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
        if p.length.is_none() && p.dst_offset > p.src_offset {
            let src_metadata = src
                .metadata()
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            let dst_metadata = dst
                .metadata()
                .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
            if src_metadata.dev() == dst_metadata.dev() && src_metadata.ino() == dst_metadata.ino()
            {
                // A forward to-EOF copy from the same inode keeps extending
                // its own source. Reject it before the first write.
                return Err(VfError::failure(0, ERR_INVAL));
            }
        }
        if let Some(requested) = p.length.filter(|&length| length > 0) {
            // A chunked forward copy can overwrite bytes that a later chunk
            // still needs when source and destination are aliases. Check only
            // forward-shifted overlapping ranges so ordinary copies do not
            // pay for extra metadata lookups.
            let requested_forward_overlap =
                p.dst_offset > p.src_offset && p.dst_offset - p.src_offset < requested;
            if requested_forward_overlap {
                let src_metadata = src
                    .metadata()
                    .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                let actual_length = requested.min(src_metadata.len().saturating_sub(p.src_offset));
                let actual_forward_overlap = p.dst_offset - p.src_offset < actual_length;
                if actual_forward_overlap {
                    let dst_metadata = dst
                        .metadata()
                        .map_err(|e| VfError::failure(0, Self::errno(&e)))?;
                    if src_metadata.dev() == dst_metadata.dev()
                        && src_metadata.ino() == dst_metadata.ino()
                    {
                        return Err(VfError::failure(0, ERR_INVAL));
                    }
                }
            }
        }
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
}

impl HandleBackend for LocalBackend {
    fn open_object_ref(
        &self,
        file: &FileOperand,
    ) -> VfResult<Option<vfsi_sync::backend::OpenObjectRef<'_>>> {
        let fd = file.fd().ok_or_else(|| VfError::client(0, ERR_INVAL))?;
        let (device, inode) = self
            .open_files
            .get(&fd)
            .ok_or_else(|| VfError::client(0, ERR_EBADF))?
            .identity()
            .map_err(|e| VfError::client(0, Self::errno(&e)))?;
        Ok(Some(vfsi_sync::backend::OpenObjectRef::Id(FsObjId::new(
            u128::from(device),
            inode,
        ))))
    }

    fn vfsync_impl(&mut self, files: &[FileOperand], mode: vfsi_core::api::SyncMode) -> VfRes {
        {
            let files: Vec<_> = files
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    let fd = file
                        .fd()
                        .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
                    self.open_files
                        .get(&fd)
                        .map(|open| &open.file)
                        .ok_or_else(|| VfError::failure(index, ERR_EBADF))
                })
                .collect::<VfResult<_>>()?;
            let results = self
                .io
                .sync(&files, matches!(mode, vfsi_core::api::SyncMode::Data));
            if results.len() != files.len() {
                return Err(VfError::client(0, VF_ERR_RPC));
            }
            for (index, result) in results.into_iter().enumerate() {
                result.map_err(|error| VfError::failure(index, Self::errno(&error)))?;
            }
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn vstatfs_impl(&mut self, files: &[FileOperand]) -> VfResult<Vec<FilesystemStats>> {
        use std::os::fd::AsRawFd;
        let mut results = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let anchored;
            let fd = if let FileOperand::Fd(fd) = file {
                self.open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(index, ERR_EBADF))?
                    .file
                    .as_raw_fd()
            } else {
                anchored = self
                    .real_path(&self.tcfile_path(file).map_err(|e| e.with_index(index))?)
                    .map_err(|e| e.with_index(index))?;
                if anchored.nofollow_on_open {
                    return Err(VfError::failure(index, ERR_NOENT));
                }
                anchored
                    .anchor
                    .as_ref()
                    .ok_or_else(|| VfError::unsupported(index))?
                    .as_raw_fd()
            };
            results.push(local_filesystem_stats(fd, index)?);
        }
        Ok(results)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::UNIX_SEMANTICS
    }

    fn abs_path(&self, path: &Path) -> PathBuf {
        let root_rel = if path.is_absolute() {
            path.strip_prefix("/").unwrap_or(path).to_path_buf()
        } else {
            self.cwd.join(path)
        };
        path_from_bytes(&normalize_bytes(path_bytes(&root_rel)))
    }

    fn open_path_impl(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<FileOperand> {
        use libc::O_CREAT;
        // `VfPathBase::Abs` treats the path as root-relative (like NFS);
        // `Cwd` resolves against the current directory.
        let resolved = match base {
            VfPathBase::Abs => self
                .root
                .join(path_from_bytes(&normalize_bytes(path_bytes(pathname)))),
            VfPathBase::Cwd => self.resolve(pathname),
        };
        let p = if flags & O_CREAT != 0 && flags & libc::O_EXCL != 0 {
            // Exclusive creation treats even a dangling final symlink as an
            // existing directory entry. Keep the leaf unresolved so
            // create_new sees it and the anchored open cannot follow it.
            self.no_follow_path(&resolved)?
        } else {
            self.real_path(&resolved)?
        };
        // The mode only applies when O_CREAT actually creates the file
        // (POSIX ignores it for existing files). Try create_new to detect
        // creation, then fall back to a plain open on EEXIST.
        let file = if flags & O_CREAT != 0 {
            let mut opts = Self::open_options(flags);
            opts.create_new(true);
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode & 0o7777);
            Self::protect_create_open(&p, &mut opts);
            match opts.open(&p) {
                Ok(f) => {
                    // The creation mode is now applied atomically by OPEN.
                    // Retain the fault boundary before registering the handle.
                    #[cfg(feature = "test-faults")]
                    self.inject_open_fault(OpenFaultPoint::BeforeSetPermissions { index: 0 })?;
                    f
                }
                Err(e)
                    if flags & libc::O_EXCL == 0
                        && e.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    let mut fallback = Self::open_options(flags);
                    use std::os::unix::fs::OpenOptionsExt;
                    fallback.mode(mode & 0o7777);
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
        let fd = self.insert_open_file(LocalOpen {
            file: Arc::new(file),
            identity: std::cell::Cell::new(None),
            path: pathname.to_path_buf(),
            cur_offset: 0,
            append: flags & libc::O_APPEND != 0,
        })?;
        Ok(FileOperand::from_fd(fd))
    }

    fn close_impl(&mut self, tcf: &FileOperand) -> VfResult<()> {
        if !tcf.is_fd() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        self.open_files
            .remove(&tcf.fd().unwrap())
            .map(|_| ())
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))
    }

    fn sync_data(&mut self, tcf: &FileOperand) -> VfResult<()> {
        let fd = tcf.fd().ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        self.open_files
            .get(&fd)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?
            .file
            .sync_data()
            .map_err(|error| VfError::failure(0, Self::errno(&error)))
    }

    fn sync_all(&mut self, tcf: &FileOperand) -> VfResult<()> {
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

    fn seek_raw_impl(&mut self, tcf: &FileOperand, offset: i64, whence: SeekFrom) -> VfResult<i64> {
        if !tcf.is_fd() {
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
    fn read_file_impl(&mut self, file: &FileOperand, max_bytes: usize) -> VfResult<Vec<u8>> {
        vfsi_sync::backend::helpers::native_read_file_impl_default(self, file, max_bytes)
    }
    fn open_impl(&mut self, request: &OpenOp) -> VfResult<FileOperand> {
        vfsi_sync::backend::helpers::native_open_impl_default(self, request)
    }
    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        vfsi_sync::backend::helpers::native_read_impl_default(self, request)
    }
    fn read_into_impl(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        vfsi_sync::backend::helpers::native_read_into_impl_default(self, request, buffer)
    }
    fn write_impl(&mut self, request: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteResult> {
        vfsi_sync::backend::helpers::native_write_impl_default(self, request)
    }
    fn seek_impl(&mut self, file: &FileOperand, position: std::io::SeekFrom) -> VfResult<u64> {
        vfsi_sync::backend::helpers::native_seek_impl_default(self, file, position)
    }
    fn metadata_impl(
        &mut self,
        target: Target<'_, FileOperand>,
        options: vfsi_core::api::AttrsOptions,
    ) -> VfResult<VfAttrs> {
        vfsi_sync::backend::helpers::native_metadata_impl_default(self, target, options)
    }
    fn set_attributes_impl(
        &mut self,
        update: &SetAttrsOp<Target<'_, FileOperand>>,
    ) -> VfResult<()> {
        vfsi_sync::backend::helpers::native_set_attributes_impl_default(self, update)
    }
    fn vsetattrs_impl(&mut self, updates: &[SetAttrsOp<Target<'_, FileOperand>>]) -> VfResult<()> {
        vfsi_sync::backend::helpers::vsetattrs_typed_default(self, updates)
    }
}

impl VectorBackend for LocalBackend {
    fn before_open_cleanup(&mut self, _index: usize, _file: &FileOperand) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeCleanup { index: _index })?;
        Ok(())
    }

    fn vopen_outcomes_impl(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<FileOperand>> {
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
            match self.open_raw_impl(path, *flags, *mode) {
                Ok(file) => {
                    #[cfg(feature = "test-faults")]
                    if let Err(error) =
                        self.inject_open_fault(OpenFaultPoint::AfterRegister { index })
                    {
                        let _ = self.close_impl(&file);
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

    fn vread_impl(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let owned = self.io.owned_read_window().is_some_and(|limit| {
            reads
                .iter()
                .try_fold(0usize, |total, op| total.checked_add(op.length))
                .is_some_and(|total| total <= limit)
        });
        if owned && Self::independent_reads(reads) {
            let prepared: Vec<_> = reads
                .iter()
                .enumerate()
                .map(|(index, op)| Self::prepare_read(&self.open_files, op, index))
                .collect::<VfResult<_>>()?;
            let operations = prepared
                .iter()
                .zip(reads)
                .map(|((file, offset), op)| io::OwnedRead {
                    file: Arc::clone(file),
                    offset: *offset,
                    buffer: vec![0; op.length],
                })
                .collect();
            let offsets: Vec<_> = prepared.iter().map(|(_, offset)| *offset).collect();
            drop(prepared);
            let results = self.io.read_owned(operations);
            if results.len() != reads.len() {
                return Err(VfError::client(0, VF_ERR_RPC));
            }
            let mut out = Vec::with_capacity(reads.len());
            let mut failure = None;
            for (index, ((op, offset), result)) in
                reads.iter().zip(offsets).zip(results).enumerate()
            {
                match result {
                    Ok(data) if data.len() <= op.length => {
                        if matches!(op.offset, VfOffset::Cur) {
                            self.advance_offset(&op.file, offset + data.len() as u64);
                        }
                        out.push(ReadResult {
                            file: op.file.clone(),
                            offset,
                            eof: data.len() < op.length,
                            data,
                        });
                    }
                    Ok(_) => {
                        failure.get_or_insert(VfError::client(index, VF_ERR_RPC));
                    }
                    Err(error) => {
                        failure.get_or_insert(if error.kind() == std::io::ErrorKind::InvalidData {
                            VfError::client(index, VF_ERR_RPC)
                        } else {
                            VfError::failure(index, Self::errno(&error))
                        });
                    }
                }
            }
            return failure.map_or(Ok(out), Err);
        }
        if reads.iter().all(|op| op.file.is_fd()) {
            let mut storage: Vec<_> = reads.iter().map(|op| vec![0; op.length]).collect();
            let mut buffers: Vec<_> = storage.iter_mut().map(Vec::as_mut_slice).collect();
            let results = self.vread_into_impl(reads, &mut buffers)?;
            return Ok(results
                .into_iter()
                .zip(storage)
                .map(|(result, mut data)| {
                    data.truncate(result.read);
                    ReadResult {
                        file: result.file,
                        offset: result.offset,
                        data,
                        eof: result.eof,
                    }
                })
                .collect());
        }
        let mut out = Vec::with_capacity(reads.len());
        for (i, op) in reads.iter().enumerate() {
            out.push(self.readv_one(op).map_err(|e| e.with_index(i))?);
        }
        Ok(out)
    }

    fn vread_into_impl(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        if reads.len() != buffers.len() {
            return Err(VfError::client(0, ERR_INVAL));
        }
        for (index, (op, buffer)) in reads.iter().zip(buffers.iter()).enumerate() {
            if op.length != buffer.len() {
                return Err(VfError::client(index, ERR_INVAL));
            }
        }
        if Self::independent_reads(reads) {
            let mut operations = Vec::with_capacity(reads.len());
            for (index, (op, buffer)) in reads.iter().zip(buffers.iter_mut()).enumerate() {
                let (file, offset) = Self::prepare_read(&self.open_files, op, index)?;
                operations.push(io::Read {
                    file,
                    offset,
                    buffer,
                });
            }
            let offsets: Vec<_> = operations.iter().map(|op| op.offset).collect();
            let results = self.io.read(&mut operations);
            drop(operations);
            if results.len() != reads.len() {
                return Err(VfError::client(0, VF_ERR_RPC));
            }
            let mut out = Vec::with_capacity(reads.len());
            let mut failure = None;
            for (index, ((op, offset), result)) in
                reads.iter().zip(offsets).zip(results).enumerate()
            {
                match result {
                    Ok(read) if read <= op.length => {
                        if matches!(op.offset, VfOffset::Cur) {
                            self.advance_offset(&op.file, offset + read as u64);
                        }
                        out.push(ReadIntoResult {
                            file: op.file.clone(),
                            offset,
                            read,
                            eof: read < op.length,
                        });
                    }
                    Ok(_) => {
                        failure.get_or_insert(VfError::client(index, VF_ERR_RPC));
                    }
                    Err(error) => {
                        failure.get_or_insert(VfError::failure(index, Self::errno(&error)));
                    }
                }
            }
            return failure.map_or(Ok(out), Err);
        }
        let mut out = Vec::with_capacity(reads.len());
        for (index, (request, buffer)) in reads.iter().zip(buffers.iter_mut()).enumerate() {
            out.push(
                self.readv_one_into(request, buffer)
                    .map_err(|e| e.with_index(index))?,
            );
        }
        Ok(out)
    }

    fn vwrite_impl(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> VfResult<Vec<WriteResult>> {
        if writes
            .iter()
            .all(|op| op.file().is_fd() && matches!(op.offset(), VfOffset::At(_)))
        {
            let mut operations = Vec::with_capacity(writes.len());
            let ordered = self.io.ordered();
            // Ordered executors never need an alias/overlap scan.
            let mut ranges = Vec::with_capacity(if ordered { 0 } else { writes.len() });
            let mut independent = true;
            for (index, op) in writes.iter().enumerate() {
                let fd = op.file().fd().unwrap();
                let open = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(index, ERR_EBADF))?;
                if open.append {
                    independent = false;
                    break;
                }
                let VfOffset::At(offset) = op.offset() else {
                    unreachable!()
                };
                let end = checked_offset(offset, op.data().len() as u64, index)?;
                if !ordered {
                    let identity = open
                        .identity()
                        .map_err(|e| VfError::failure(index, Self::errno(&e)))?;
                    ranges.push((identity, offset, end));
                }
                operations.push(io::Write {
                    file: &open.file,
                    offset,
                    data: op.data(),
                });
            }
            if independent && (ordered || !writes_overlap(&mut ranges)) {
                let results = self.io.write(&operations);
                if results.len() != writes.len() {
                    return Err(VfError::client(0, VF_ERR_RPC));
                }
                return writes
                    .iter()
                    .zip(operations)
                    .zip(results)
                    .enumerate()
                    .map(|(index, ((op, request), result))| {
                        let written =
                            result.map_err(|e| VfError::failure(index, Self::errno(&e)))?;
                        if written > op.data().len() {
                            return Err(VfError::client(index, VF_ERR_RPC));
                        }
                        Ok(WriteResult {
                            file: op.file().clone(),
                            offset: request.offset,
                            written,
                            stable: true,
                        })
                    })
                    .collect();
            }
        }
        let mut out = Vec::with_capacity(writes.len());
        for (i, op) in writes.iter().enumerate() {
            out.push(self.writev_one(*op).map_err(|e| e.with_index(i))?);
        }
        Ok(out)
    }

    fn vgetattrs_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        self.getattrsv_impl(attrs, true)
    }

    fn vgetattrs_nofollow_impl(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        self.getattrsv_impl(attrs, false)
    }

    fn vsetattrs_raw_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME)
            .union(AttrMask::UID)
            .union(AttrMask::GID);
        for (i, a) in attrs.iter().enumerate() {
            if !a.masks.difference(SETTABLE).is_empty() {
                return Err(VfError::unsupported(i));
            }
            self.setattr_one(a, i)?;
        }
        Ok(())
    }

    fn vsetattrs_raw_nofollow_impl(&mut self, attrs: &[VfAttrs]) -> VfRes {
        const SETTABLE: AttrMask = AttrMask::MODE
            .union(AttrMask::SIZE)
            .union(AttrMask::ATIME)
            .union(AttrMask::MTIME)
            .union(AttrMask::UID)
            .union(AttrMask::GID);
        for (i, a) in attrs.iter().enumerate() {
            if !a.masks.difference(SETTABLE).is_empty() {
                return Err(VfError::unsupported(i));
            }
            if a.file.is_fd() {
                self.setattr_one(a, i)?;
                continue;
            }
            let p = self
                .no_follow_path(&self.tcfile_path(&a.file).map_err(|e| e.with_index(i))?)
                .map_err(|e| e.with_index(i))?;
            if a.masks.intersects(AttrMask::UID | AttrMask::GID)
                && a.masks.difference(AttrMask::UID | AttrMask::GID).is_empty()
            {
                Self::chown_path(a, &p, false, i)?;
                continue;
            }
            let md =
                std::fs::symlink_metadata(&p).map_err(|e| VfError::failure(i, Self::errno(&e)))?;
            if md.file_type().is_symlink() {
                // No portable lchmod/ltruncate: refuse rather than follow.
                return Err(VfError::unsupported(i));
            }
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::AfterNoFollowSetattrTypeCheck { index: i })?;
            self.setattr_path(a, i, &p, false)?;
        }
        Ok(())
    }

    fn listdir_impl(
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

    fn listdir_page_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        _max_entries: usize,
        follow_symlinks: bool,
    ) -> VfResult<(Vec<VfAttrs>, Option<DirPageCursor>)> {
        if page_size == 0 {
            return Err(VfError::client(0, ERR_INVAL));
        }
        let mut state = match cursor {
            Some(cursor) => cursor.into_state::<LocalDirectoryCursor>()?,
            None => {
                let p = if follow_symlinks {
                    self.open_directory_no_follow_final(&self.resolve(dir))?
                } else {
                    self.strict_directory_path(dir)?
                };
                let entries = std::fs::read_dir(&p)
                    .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
                LocalDirectoryCursor {
                    entries,
                    virtual_dir: dir.to_path_buf(),
                    _anchor: p,
                }
            }
        };
        let mut page = Vec::with_capacity(page_size);
        while page.len() < page_size {
            let Some(entry) = state.entries.next() else {
                return Ok((page, None));
            };
            let entry = entry.map_err(|error| VfError::failure(0, Self::errno(&error)))?;
            let path = state.virtual_dir.join(entry.file_name());
            let mut attrs = VfAttrs {
                file: FileOperand::from_os_path(&path),
                masks,
                ..VfAttrs::default()
            };
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
            let real = self.no_follow_path(&self.resolve(&path))?;
            self.fill_attrs(&mut attrs, Some(&real), &metadata)
                .map_err(|error| error.with_index(page.len()))?;
            page.push(attrs);
        }
        Ok((page, Some(DirPageCursor::new(state))))
    }

    fn visit_dir_impl(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_entries: usize,
        cb: &mut dyn FnMut(&VfAttrs) -> bool,
    ) -> VfRes {
        let p = self.open_directory_no_follow_final(&self.resolve(dir))?;
        let entries =
            std::fs::read_dir(&p).map_err(|error| VfError::failure(0, Self::errno(&error)))?;
        for (count, entry) in entries.enumerate() {
            if max_entries != 0 && count >= max_entries {
                return Ok(());
            }
            let entry = entry.map_err(|error| VfError::failure(0, Self::errno(&error)))?;
            let path = dir.join(entry.file_name());
            let mut attrs = VfAttrs {
                file: FileOperand::from_os_path(&path),
                masks,
                ..VfAttrs::default()
            };
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| VfError::failure(0, Self::errno(&error)))?;
            let real = self.no_follow_path(&self.resolve(&path))?;
            self.fill_attrs(&mut attrs, Some(&real), &metadata)
                .map_err(|error| error.with_index(count))?;
            if !cb(&attrs) {
                return Ok(());
            }
        }
        Ok(())
    }

    fn vrename_impl(&mut self, pairs: &[(FileOperand, FileOperand)]) -> VfRes {
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

    fn vrename_with_options_impl(
        &mut self,
        pairs: &[(FileOperand, FileOperand)],
        options: vfsi_core::api::RenameOptions,
    ) -> VfRes {
        if pairs.is_empty() {
            return Ok(());
        }
        if options == vfsi_core::api::RenameOptions::Replace {
            return self.vrename_impl(pairs);
        }
        #[cfg(target_os = "linux")]
        {
            for (index, (src, dst)) in pairs.iter().enumerate() {
                let source_path = self.vf_path(src).map_err(|e| e.with_index(index))?;
                let destination_path = self.vf_path(dst).map_err(|e| e.with_index(index))?;
                let source = self
                    .no_follow_path(&self.root.join(&source_path))
                    .map_err(|e| e.with_index(index))?;
                let destination = self
                    .no_follow_path(&self.root.join(&destination_path))
                    .map_err(|e| e.with_index(index))?;
                let source_name = cstring_from_bytes(path_bytes(&source))
                    .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
                let destination_name = cstring_from_bytes(path_bytes(&destination))
                    .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
                if options == vfsi_core::api::RenameOptions::Exchange
                    && source_path == destination_path
                {
                    continue;
                }
                let flags = match options {
                    vfsi_core::api::RenameOptions::NoReplace => libc::RENAME_NOREPLACE,
                    vfsi_core::api::RenameOptions::Exchange => libc::RENAME_EXCHANGE,
                    vfsi_core::api::RenameOptions::Replace => unreachable!(),
                };
                // SAFETY: both NUL-terminated path buffers and their directory anchors
                // stay live; flags are supported renameat2 constants.
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        libc::AT_FDCWD,
                        source_name.as_ptr(),
                        libc::AT_FDCWD,
                        destination_name.as_ptr(),
                        flags,
                    )
                };
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    let source_is_directory = std::fs::symlink_metadata(&source)
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false);
                    let destination_is_directory = std::fs::symlink_metadata(&destination)
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false);
                    let errno = if options == vfsi_core::api::RenameOptions::NoReplace {
                        noreplace_error_code(
                            &error,
                            source_is_directory,
                            &source_path,
                            &destination_path,
                        )
                    } else {
                        match error.raw_os_error() {
                            Some(libc::EOPNOTSUPP | libc::ENOSYS) => VF_ERR_UNSUPPORTED,
                            Some(libc::EINVAL)
                                if !(source_is_directory
                                    && destination_path.starts_with(&source_path)
                                    || destination_is_directory
                                        && source_path.starts_with(&destination_path)) =>
                            {
                                VF_ERR_UNSUPPORTED
                            }
                            _ => error
                                .raw_os_error()
                                .map(|errno| errno as u32)
                                .unwrap_or(VF_ERR_RPC),
                        }
                    };
                    return Err(VfError::failure(index, errno));
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pairs;
            Err(VfError::failure(0, ERR_UNSUPPORTED))
        }
    }

    fn vremove_impl(&mut self, files: &[FileOperand]) -> VfRes {
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

    fn vmkdir_impl(&mut self, dirs: &[VfAttrs]) -> VfRes {
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
                Self::set_permissions_no_follow(&p, a.mode, true, i)?;
            }
        }
        Ok(())
    }

    fn vsymlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
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

    fn vreadlink_impl(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
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

    fn vhardlink_impl(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
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

    fn vcopy_data_impl(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (i, p) in pairs.iter().enumerate() {
            self.copy_extent(p).map_err(|e| e.with_index(i))?;
        }
        Ok(())
    }

    fn vcopy_impl(&mut self, pairs: &[ExtentPair], options: CopyOption) -> VfRes {
        if options.follow_source_symlinks() {
            return self.vcopy_data_impl(pairs);
        }
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

    fn copy_tree_impl(
        &mut self,
        src_dir: &Path,
        dst: &Path,
        symlinks: bool,
        _use_server_side_copy: bool,
    ) -> VfRes {
        vfsi_sync::backend::helpers::copy_tree_with(
            self,
            src_dir,
            dst,
            symlinks,
            AttrMask::MODE | AttrMask::SIZE | AttrMask::FILEID,
            |backend, pair| backend.vcopy_data_impl(std::slice::from_ref(pair)),
            |error| error.with_index(0),
        )
    }
    fn before_remove_type(&mut self, _index: usize) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeRemoveType { index: _index })?;
        Ok(())
    }

    fn vwrite_adb_impl(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let mut counts = Vec::with_capacity(patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            let layout = vfsi_core::internal::adb_layout(p, i)?;
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
}

#[cfg(test)]
#[allow(clippy::undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn)]
mod contract_tests;

#[cfg(target_os = "linux")]
fn local_filesystem_stats(fd: std::os::fd::RawFd, index: usize) -> VfResult<FilesystemStats> {
    // SAFETY: this libc output structure contains C integer fields and is valid when
    // zero-initialized.
    let mut raw: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: fd is borrowed from a live File and raw provides writable statvfs output
    // storage.
    if unsafe { libc::fstatvfs(fd, &mut raw) } != 0 {
        return Err(VfError::failure(
            index,
            LocalBackend::errno(&std::io::Error::last_os_error()),
        ));
    }
    let bytes = |blocks: u64| {
        blocks
            .checked_mul(raw.f_frsize)
            .map(Some)
            .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))
    };
    let conf = |name| -> VfResult<Option<u64>> {
        // SAFETY: libc supplies the calling thread's live errno cell; no reference
        // escapes this operation.
        unsafe {
            *libc::__errno_location() = 0;
        }
        // SAFETY: fd is borrowed from a live File and name is a supported _PC_*
        // selector.
        let value = unsafe { libc::fpathconf(fd, name) };
        if value >= 0 {
            return Ok(Some(value as u64));
        }
        // SAFETY: libc supplies the calling thread's live errno cell; no reference
        // escapes this operation.
        let errno = unsafe { *libc::__errno_location() };
        if errno == 0 || errno == libc::EINVAL {
            Ok(None)
        } else {
            Err(VfError::failure(index, errno as u32))
        }
    };
    Ok(FilesystemStats {
        total_bytes: bytes(raw.f_blocks)?,
        free_bytes: bytes(raw.f_bfree)?,
        available_bytes: bytes(raw.f_bavail)?,
        total_files: Some(raw.f_files),
        free_files: Some(raw.f_ffree),
        available_files: Some(raw.f_favail),
        block_size: Some(raw.f_bsize),
        fragment_size: Some(raw.f_frsize),
        max_name_len: conf(libc::_PC_NAME_MAX)?,
        max_path_len: conf(libc::_PC_PATH_MAX)?,
        max_links: conf(libc::_PC_LINK_MAX)?,
        file_size_bits: conf(libc::_PC_FILESIZEBITS)?
            .map(|value| {
                u32::try_from(value).map_err(|_| VfError::failure(index, libc::EOVERFLOW as u32))
            })
            .transpose()?,
        read_only: Some(raw.f_flag & libc::ST_RDONLY != 0),
        no_set_id: Some(raw.f_flag & libc::ST_NOSUID != 0),
        ..FilesystemStats::default()
    })
}
