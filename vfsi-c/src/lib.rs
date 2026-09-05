//! C API for vfsi, the vectorized filesystem API.
//!
//! The C-facing symbols are intentionally thin: C code works with an opaque
//! `vfsi_fs` handle and byte-oriented `const char *` paths, while all
//! filesystem logic stays in the Rust `vnfs` implementation.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, OsString};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::nfs::NfsVecFs;
use vnfs::vecfs::{AttrMask, VfAttrs, VfError, VfFile, ERR_EBADF, ERR_NOENT};

/// Opaque filesystem handle owned by C.
pub struct vfsi_fs {
    fs: Mutex<Box<dyn vnfs::VecFs>>,
    files: Mutex<std::collections::HashMap<i32, VfFile>>,
    next_fd: AtomicI32,
    /// Kernel mountpoint mapped to the vfsi/NFS export root.
    mountpoint: PathBuf,
}

/// Attributes returned by [`vfsi_stat`] and passed to listdir callbacks.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_attrs {
    pub ftype: u32,
    pub mode: u32,
    pub size: u64,
    pub nlink: u32,
    pub fileid: u64,
    pub uid: u32,
    pub gid: u32,
    pub blocks: u64,
    pub atime_sec: i64,
    pub atime_nsec: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub ctime_sec: i64,
    pub ctime_nsec: u32,
}

impl vfsi_attrs {
    fn from_vf(a: &VfAttrs) -> vfsi_attrs {
        vfsi_attrs {
            ftype: a.ftype.as_nfs(),
            mode: a.mode,
            size: a.size,
            nlink: a.nlink,
            fileid: a.fileid,
            uid: a.uid,
            gid: a.gid,
            blocks: a.blocks,
            atime_sec: a.atime_sec,
            atime_nsec: a.atime_nsec,
            mtime_sec: a.mtime_sec,
            mtime_nsec: a.mtime_nsec,
            ctime_sec: a.ctime_sec,
            ctime_nsec: a.ctime_nsec,
        }
    }
}

pub type vfsi_listdir_cb = Option<
    unsafe extern "C" fn(
        name: *const c_char,
        attrs: *const vfsi_attrs,
        userdata: *mut c_void,
    ) -> bool,
>;

pub type vfsi_listdirv_cb = Option<
    unsafe extern "C" fn(
        dir: *const c_char,
        name: *const c_char,
        attrs: *const vfsi_attrs,
        userdata: *mut c_void,
    ) -> bool,
>;

pub type vfsi_read_paths_cb = Option<
    unsafe extern "C" fn(
        path: *const c_char,
        data: *const u8,
        len: usize,
        userdata: *mut c_void,
    ) -> bool,
>;

fn mask() -> AttrMask {
    AttrMask::MODE
        | AttrMask::SIZE
        | AttrMask::NLINK
        | AttrMask::FILEID
        | AttrMask::BLOCKS
        | AttrMask::UID
        | AttrMask::GID
        | AttrMask::ATIME
        | AttrMask::MTIME
        | AttrMask::CTIME
}

fn vf_code(e: &VfError) -> c_int {
    match e {
        VfError::Op { err_no, .. } => *err_no as c_int,
        VfError::Transport { .. } => libc::EIO,
        _ => libc::EIO,
    }
}

fn cstr_path(path: *const c_char) -> Option<PathBuf> {
    if path.is_null() {
        return None;
    }
    // SAFETY: C callers must pass a NUL-terminated path string.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    Some(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

fn cstr_from_os(bytes: &[u8]) -> Option<CString> {
    CString::new(bytes).ok()
}

fn path_for(fs: &vfsi_fs, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(&fs.mountpoint).ok()?;
    if rel.as_os_str().is_empty() {
        Some(PathBuf::from("/"))
    } else {
        Some(Path::new("/").join(rel))
    }
}

fn vpath_for(fs: &vfsi_fs, path: &std::path::Path) -> Result<PathBuf, VfError> {
    path_for(fs, path).ok_or_else(|| VfError::failure(0, ERR_NOENT))
}

/// Create a local-directory vfsi backend rooted at `root`.
#[no_mangle]
pub unsafe extern "C" fn vfsi_dummy_open(root: *const c_char, out: *mut *mut vfsi_fs) -> c_int {
    if root.is_null() || out.is_null() {
        return libc::EINVAL;
    }
    let Some(root) = cstr_path(root) else {
        return libc::EINVAL;
    };
    let backend = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Ok::<_, std::io::Error>(Box::new(DummyVecFs::new(root)) as Box<dyn vnfs::VecFs>)
    }));
    match backend {
        Ok(Ok(fs)) => {
            *out = Box::into_raw(Box::new(vfsi_fs {
                fs: Mutex::new(fs),
                files: Mutex::new(std::collections::HashMap::new()),
                next_fd: AtomicI32::new(1),
                mountpoint: PathBuf::from("/"),
            }));
            0
        }
        Ok(Err(e)) => e.raw_os_error().unwrap_or(libc::EIO),
        Err(_) => libc::EIO,
    }
}

/// Create a local-directory vfsi backend rooted at `root`, treating
/// `mountpoint` as the kernel-visible root of the same directory. Callers can
/// pass ordinary kernel paths under `mountpoint`, which are mapped to `/`-rooted
/// vfsi paths before they reach the backend.
#[no_mangle]
pub unsafe extern "C" fn vfsi_dummy_open_mount(
    root: *const c_char,
    mountpoint: *const c_char,
    out: *mut *mut vfsi_fs,
) -> c_int {
    if root.is_null() || mountpoint.is_null() || out.is_null() {
        return libc::EINVAL;
    }
    let (Some(root), Some(mountpoint)) = (cstr_path(root), cstr_path(mountpoint)) else {
        return libc::EINVAL;
    };
    let backend = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Ok::<_, std::io::Error>(Box::new(DummyVecFs::new(root)) as Box<dyn vnfs::VecFs>)
    }));
    match backend {
        Ok(Ok(fs)) => {
            *out = Box::into_raw(Box::new(vfsi_fs {
                fs: Mutex::new(fs),
                files: Mutex::new(std::collections::HashMap::new()),
                next_fd: AtomicI32::new(1),
                mountpoint,
            }));
            0
        }
        Ok(Err(e)) => e.raw_os_error().unwrap_or(libc::EIO),
        Err(_) => libc::EIO,
    }
}

/// Connect to an NFSv4.1 server at `host` and open the export root.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open(host: *const c_char, out: *mut *mut vfsi_fs) -> c_int {
    if host.is_null() || out.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: C callers must pass a NUL-terminated host string.
    let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
        return libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        NfsVecFs::connect(host)
            .map(|f| Box::new(f) as Box<dyn vnfs::VecFs>)
            .map_err(|e| vf_code(&e))
    }));
    match res {
        Ok(Ok(fs)) => {
            *out = Box::into_raw(Box::new(vfsi_fs {
                fs: Mutex::new(fs),
                files: Mutex::new(std::collections::HashMap::new()),
                next_fd: AtomicI32::new(1),
                mountpoint: PathBuf::from("/"),
            }));
            0
        }
        Ok(Err(code)) => code,
        Err(_) => libc::EIO,
    }
}

/// Connect to an NFSv4.1 server and treat `mountpoint` (a local kernel mount
/// path) as the vfsi root. Callers can then pass ordinary kernel paths.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open_mount(
    host: *const c_char,
    mountpoint: *const c_char,
    out: *mut *mut vfsi_fs,
) -> c_int {
    if host.is_null() || mountpoint.is_null() || out.is_null() {
        return libc::EINVAL;
    }
    let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
        return libc::EINVAL;
    };
    let Some(mountpoint) = cstr_path(mountpoint) else {
        return libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        NfsVecFs::connect(host)
            .map(|f| Box::new(f) as Box<dyn vnfs::VecFs>)
            .map_err(|e| vf_code(&e))
    }));
    match res {
        Ok(Ok(fs)) => {
            *out = Box::into_raw(Box::new(vfsi_fs {
                fs: Mutex::new(fs),
                files: Mutex::new(std::collections::HashMap::new()),
                next_fd: AtomicI32::new(1),
                mountpoint,
            }));
            0
        }
        Ok(Err(code)) => code,
        Err(_) => libc::EIO,
    }
}

/// Destroy a filesystem handle returned by one of the `vfsi_*_open*`
/// functions.
#[no_mangle]
pub unsafe extern "C" fn vfsi_free(fs: *mut vfsi_fs) {
    if !fs.is_null() {
        if std::env::var("VNFS_STATS").as_deref() == Ok("1") {
            let (n, ops, bytes, max) = vnfs::compound::compound_stats();
            if n > 0 {
                eprintln!(
                    "[vfsi] compounds={} avg_ops={:.2} max_ops={} avg_bytes={:.0} total_bytes={}",
                    n,
                    ops as f64 / n as f64,
                    max,
                    bytes as f64 / n as f64,
                    bytes
                );
            }
            let (calls, us) = vnfs::compound::rpc_stats();
            if calls > 0 {
                eprintln!(
                    "[vfsi] rpc_calls={} avg_rpc_ms={:.2} total_rpc_ms={:.1}",
                    calls,
                    us as f64 / calls as f64 / 1000.0,
                    us as f64 / 1000.0
                );
            }
        }
        // SAFETY: frees an object previously created by this crate.
        unsafe { drop(Box::from_raw(fs)) };
    }
}

fn stat_impl(fs: &vfsi_fs, path: &std::path::Path) -> Result<VfAttrs, VfError> {
    let vpath = vpath_for(fs, path)?;
    fs.fs.lock().expect("vfsi lock poisoned").stat(&vpath)
}

/// Stat `path`, following a final symlink.
#[no_mangle]
pub unsafe extern "C" fn vfsi_stat(
    fs: *mut vfsi_fs,
    path: *const c_char,
    out: *mut vfsi_attrs,
) -> c_int {
    let (Some(fs), Some(path), Some(out)) = (fs.as_ref(), cstr_path(path), out.as_mut()) else {
        return libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stat_impl(fs, &path)));
    match res {
        Ok(Ok(a)) => {
            *out = vfsi_attrs::from_vf(&a);
            0
        }
        Ok(Err(e)) => vf_code(&e),
        Err(_) => libc::EIO,
    }
}

fn open_impl(
    fs: &vfsi_fs,
    path: &std::path::Path,
    flags: c_int,
    mode: u32,
) -> Result<i32, VfError> {
    let vpath = vpath_for(fs, path)?;
    let file = fs
        .fs
        .lock()
        .expect("vfsi lock poisoned")
        .open(&vpath, flags, mode)?;
    let fd = fs.next_fd.fetch_add(1, Ordering::Relaxed);
    fs.files
        .lock()
        .expect("vfsi files lock poisoned")
        .insert(fd, file);
    Ok(fd)
}

/// Open a file and return a vfsi descriptor (`>= 0`), or a negative errno.
#[no_mangle]
pub unsafe extern "C" fn vfsi_open(
    fs: *mut vfsi_fs,
    path: *const c_char,
    flags: c_int,
    mode: u32,
) -> c_int {
    let (Some(fs), Some(path)) = (fs.as_ref(), cstr_path(path)) else {
        return -libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        open_impl(fs, &path, flags, mode)
    }));
    match res {
        Ok(Ok(fd)) => fd,
        Ok(Err(e)) => -vf_code(&e),
        Err(_) => -libc::EIO,
    }
}

fn close_impl(fs: &vfsi_fs, fd: i32) -> Result<(), VfError> {
    let file = fs
        .files
        .lock()
        .expect("vfsi files lock poisoned")
        .remove(&fd)
        .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
    fs.fs.lock().expect("vfsi lock poisoned").close(&file)
}

/// Close a descriptor opened by [`vfsi_open`].
#[no_mangle]
pub unsafe extern "C" fn vfsi_close(fs: *mut vfsi_fs, fd: c_int) -> c_int {
    let Some(fs) = fs.as_ref() else {
        return libc::EINVAL;
    };
    match close_impl(fs, fd) {
        Ok(()) => 0,
        Err(e) => vf_code(&e),
    }
}

fn pread_impl(
    fs: &vfsi_fs,
    fd: i32,
    buf: *mut c_void,
    len: usize,
    offset: u64,
) -> Result<usize, VfError> {
    let file = fs
        .files
        .lock()
        .expect("vfsi files lock poisoned")
        .get(&fd)
        .cloned()
        .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
    let data = fs
        .fs
        .lock()
        .expect("vfsi lock poisoned")
        .read(&file, offset, len)?;
    // SAFETY: C caller must provide a buffer of at least `len` bytes.
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len()) };
    Ok(data.len())
}

/// Read up to `len` bytes at `offset` into `buf`; writes the actual count to
/// `*got` when non-NULL.
#[no_mangle]
pub unsafe extern "C" fn vfsi_pread(
    fs: *mut vfsi_fs,
    fd: c_int,
    buf: *mut c_void,
    len: usize,
    offset: u64,
    got: *mut usize,
) -> c_int {
    let (Some(fs), false, Some(buf)) = (fs.as_ref(), buf.is_null(), buf.as_mut()) else {
        return libc::EINVAL;
    };
    let _ = buf;
    match pread_impl(fs, fd, buf, len, offset) {
        Ok(n) => {
            if !got.is_null() {
                // SAFETY: C caller may pass NULL, otherwise a writable usize.
                unsafe { *got = n };
            }
            0
        }
        Err(e) => vf_code(&e),
    }
}

fn pwrite_impl(
    fs: &vfsi_fs,
    fd: i32,
    buf: *const c_void,
    len: usize,
    offset: u64,
) -> Result<usize, VfError> {
    let file = fs
        .files
        .lock()
        .expect("vfsi files lock poisoned")
        .get(&fd)
        .cloned()
        .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
    // SAFETY: C caller must provide a valid buffer of `len` bytes.
    let data = unsafe { std::slice::from_raw_parts(buf as *const u8, len) };
    fs.fs
        .lock()
        .expect("vfsi lock poisoned")
        .write(&file, offset, data)
}

/// Write `len` bytes at `offset`; writes the actual count to `*wrote`.
#[no_mangle]
pub unsafe extern "C" fn vfsi_pwrite(
    fs: *mut vfsi_fs,
    fd: c_int,
    buf: *const c_void,
    len: usize,
    offset: u64,
    wrote: *mut usize,
) -> c_int {
    let (Some(fs), false, Some(buf)) = (fs.as_ref(), buf.is_null(), buf.as_ref()) else {
        return libc::EINVAL;
    };
    let _ = buf;
    match pwrite_impl(fs, fd, buf, len, offset) {
        Ok(n) => {
            if !wrote.is_null() {
                // SAFETY: C caller may pass NULL, otherwise a writable usize.
                unsafe { *wrote = n };
            }
            0
        }
        Err(e) => vf_code(&e),
    }
}

/// Create a directory (`create_parents != 0` behaves like `mkdir -p`).
#[no_mangle]
pub unsafe extern "C" fn vfsi_mkdir(
    fs: *mut vfsi_fs,
    path: *const c_char,
    mode: u32,
    create_parents: c_int,
) -> c_int {
    let (Some(fs), Some(path)) = (fs.as_ref(), cstr_path(path)) else {
        return libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path = vpath_for(fs, &path)?;
        let mut f = fs.fs.lock().expect("vfsi lock poisoned");
        if create_parents != 0 {
            f.ensure_dir(&path, mode)
        } else {
            f.mkdir(&path, mode)
        }
    }));
    match res {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => vf_code(&e),
        Err(_) => libc::EIO,
    }
}

/// Remove a path (directories must be empty).
#[no_mangle]
pub unsafe extern "C" fn vfsi_remove(fs: *mut vfsi_fs, path: *const c_char) -> c_int {
    let (Some(fs), Some(path)) = (fs.as_ref(), cstr_path(path)) else {
        return libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path = vpath_for(fs, &path)?;
        fs.fs.lock().expect("vfsi lock poisoned").unlink(&path)
    }));
    match res {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => vf_code(&e),
        Err(_) => libc::EIO,
    }
}

/// Rename `oldpath` to `newpath`.
#[no_mangle]
pub unsafe extern "C" fn vfsi_rename(
    fs: *mut vfsi_fs,
    oldpath: *const c_char,
    newpath: *const c_char,
) -> c_int {
    let (Some(fs), Some(old), Some(new)) = (fs.as_ref(), cstr_path(oldpath), cstr_path(newpath))
    else {
        return libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let old = vpath_for(fs, &old)?;
        let new = vpath_for(fs, &new)?;
        fs.fs
            .lock()
            .expect("vfsi lock poisoned")
            .renamev(&[(VfFile::from_os_path(&old), VfFile::from_os_path(&new))])
    }));
    match res {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => vf_code(&e),
        Err(_) => libc::EIO,
    }
}

/// List `dir` and call `cb` for each entry. Returning `false` from `cb`
/// stops the listing.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdir(
    fs: *mut vfsi_fs,
    dir: *const c_char,
    cb: vfsi_listdir_cb,
    userdata: *mut c_void,
) -> c_int {
    let (Some(fs), Some(dir)) = (fs.as_ref(), cstr_path(dir)) else {
        return libc::EINVAL;
    };
    let Some(cb) = cb else {
        return libc::EINVAL;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dir = vpath_for(fs, &dir)?;
        let entries = fs
            .fs
            .lock()
            .expect("vfsi lock poisoned")
            .listdir(&dir, mask(), 0, false)?;
        for a in entries {
            let name = a
                .file
                .path()
                .and_then(|p| p.file_name())
                .map(|n| n.as_bytes())
                .and_then(cstr_from_os)
                .unwrap_or_default();
            let attrs = vfsi_attrs::from_vf(&a);
            // SAFETY: callback was provided by C and is called from the same
            // thread while `attrs`/`name` are alive.
            if !unsafe { cb(name.as_ptr(), &attrs, userdata) } {
                break;
            }
        }
        Ok::<_, VfError>(())
    }));
    match res {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => vf_code(&e),
        Err(_) => libc::EIO,
    }
}

/// List several directories in one vectorized batch, calling `cb` for each
/// entry with the directory the entry came from.
#[no_mangle]
pub unsafe extern "C" fn vfsi_listdirv(
    fs: *mut vfsi_fs,
    dirs: *const *const c_char,
    count: usize,
    max_entries: usize,
    recursive: bool,
    cb: vfsi_listdirv_cb,
    userdata: *mut c_void,
) -> c_int {
    let (Some(fs), false, false) = (fs.as_ref(), dirs.is_null(), cb.is_none()) else {
        return libc::EINVAL;
    };
    let Some(cb) = cb else {
        return libc::EINVAL;
    };
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: `dirs` points to `count` NUL-terminated strings.
        let p = unsafe { *dirs.add(i) };
        let Some(path) = cstr_path(p) else {
            return libc::EINVAL;
        };
        paths.push(path);
    }
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let original_paths = paths.clone();
        let vpaths: Result<Vec<PathBuf>, VfError> =
            paths.iter().map(|p| vpath_for(fs, p)).collect();
        let vpaths = vpaths?;
        let refs: Vec<&Path> = vpaths.iter().map(PathBuf::as_path).collect();
        let vpath_index: std::collections::HashMap<PathBuf, usize> = vpaths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();
        let mountpoint = fs.mountpoint.clone();
        let c_cb = cb;
        fs.fs.lock().expect("vfsi lock poisoned").listdirv(
            &refs,
            mask(),
            max_entries,
            recursive,
            &mut |attrs, dir| {
                let kernel_dir = match vpath_index.get(dir) {
                    Some(&i) => original_paths[i].clone(),
                    None => {
                        let rel = dir.strip_prefix("/").unwrap_or(dir);
                        mountpoint.join(rel)
                    }
                };
                let dir = kernel_dir.as_os_str().as_bytes();
                let Some(dir) = cstr_from_os(dir) else {
                    return false;
                };
                let name = attrs
                    .file
                    .path()
                    .and_then(|p| p.file_name())
                    .map(|n| n.as_bytes())
                    .and_then(cstr_from_os)
                    .unwrap_or_default();
                let a = vfsi_attrs::from_vf(attrs);
                // SAFETY: `c_cb` was provided by C and is invoked from the
                // same thread while its arguments are alive.
                unsafe { c_cb(dir.as_ptr(), name.as_ptr(), &a, userdata) }
            },
        )
    }));
    match res {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => vf_code(&e),
        Err(_) => libc::EIO,
    }
}

/// Read the full contents of several files in one vectorized batch, calling
/// `cb` for each file after its data has been fetched.
#[no_mangle]
pub unsafe extern "C" fn vfsi_read_paths(
    fs: *mut vfsi_fs,
    paths: *const *const c_char,
    count: usize,
    cb: vfsi_read_paths_cb,
    userdata: *mut c_void,
) -> c_int {
    let (Some(fs), false, false) = (fs.as_ref(), paths.is_null(), cb.is_none()) else {
        return libc::EINVAL;
    };
    let Some(cb) = cb else {
        return libc::EINVAL;
    };
    let mut kernel_paths = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: `paths` points to `count` NUL-terminated strings.
        let p = unsafe { *paths.add(i) };
        let Some(path) = cstr_path(p) else {
            return libc::EINVAL;
        };
        kernel_paths.push(path);
    }
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let vpaths: Result<Vec<PathBuf>, VfError> =
            kernel_paths.iter().map(|p| vpath_for(fs, p)).collect();
        let vpaths = vpaths?;
        let files: Vec<VfFile> = vpaths.iter().map(|p| VfFile::from_os_path(p)).collect();
        let datas = fs
            .fs
            .lock()
            .expect("vfsi lock poisoned")
            .read_allv(&files)?;
        let c_cb = cb;
        for (i, data) in datas.iter().enumerate() {
            let Some(path) = cstr_from_os(kernel_paths[i].as_os_str().as_bytes()) else {
                continue;
            };
            // SAFETY: `c_cb` was provided by C and is invoked from the same
            // thread while `path`/`data` are alive.
            if !unsafe { c_cb(path.as_ptr(), data.as_ptr(), data.len(), userdata) } {
                break;
            }
        }
        Ok::<_, VfError>(())
    }));
    match res {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => vf_code(&e),
        Err(_) => libc::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn temp_root() -> CString {
        CString::new(
            std::env::temp_dir()
                .join(format!("vfsi_c_{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap()
    }

    #[test]
    fn dummy_backend_read_write_list() {
        let root = temp_root();
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
        assert!(!fs.is_null());

        let dir = CString::new("/d").unwrap();
        assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);
        let path = CString::new("/d/f.bin").unwrap();
        let fd = unsafe {
            vfsi_open(
                fs,
                path.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o644,
            )
        };
        assert!(fd > 0);
        let data = b"hello".as_ptr() as *const c_void;
        assert_eq!(
            unsafe { vfsi_pwrite(fs, fd, data, 5, 0, std::ptr::null_mut()) },
            0
        );
        assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);

        let mut buf = [0u8; 8];
        let fd = unsafe { vfsi_open(fs, path.as_ptr(), libc::O_RDONLY, 0) };
        assert!(fd > 0);
        let mut got = 0usize;
        assert_eq!(
            unsafe { vfsi_pread(fs, fd, buf.as_mut_ptr() as *mut c_void, 8, 0, &mut got) },
            0
        );
        assert_eq!(got, 5);
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);

        let mut seen: Vec<String> = Vec::new();
        unsafe extern "C" fn callback(
            name: *const c_char,
            _: *const vfsi_attrs,
            user: *mut c_void,
        ) -> bool {
            let v = &mut *(user as *mut Vec<String>);
            v.push(CStr::from_ptr(name).to_string_lossy().into_owned());
            true
        }
        assert_eq!(
            unsafe {
                vfsi_listdir(
                    fs,
                    dir.as_ptr(),
                    Some(callback),
                    &mut seen as *mut _ as *mut c_void,
                )
            },
            0
        );
        assert_eq!(seen, vec!["f.bin"]);

        unsafe { vfsi_free(fs) };
    }

    #[test]
    fn dummy_backend_listdirv_maps_dirs_to_entries() {
        let root = temp_root();
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
        assert!(!fs.is_null());

        let d1 = CString::new("/v/d1").unwrap();
        let d2 = CString::new("/v/d2").unwrap();
        assert_eq!(unsafe { vfsi_mkdir(fs, d1.as_ptr(), 0o755, 1) }, 0);
        assert_eq!(unsafe { vfsi_mkdir(fs, d2.as_ptr(), 0o755, 1) }, 0);
        let p1 = CString::new("/v/d1/one.bin").unwrap();
        let p2 = CString::new("/v/d2/two.bin").unwrap();
        for p in [&p1, &p2] {
            let fd = unsafe {
                vfsi_open(
                    fs,
                    p.as_ptr(),
                    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                    0o644,
                )
            };
            assert!(fd > 0);
            assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
        }

        #[derive(Default)]
        struct Seen {
            rows: Vec<(String, String)>,
        }
        unsafe extern "C" fn cb(
            dir: *const c_char,
            name: *const c_char,
            _: *const vfsi_attrs,
            user: *mut c_void,
        ) -> bool {
            let seen = &mut *(user as *mut Seen);
            seen.rows.push((
                CStr::from_ptr(dir).to_string_lossy().into_owned(),
                CStr::from_ptr(name).to_string_lossy().into_owned(),
            ));
            true
        }

        let dirs = [d1.as_ptr(), d2.as_ptr()];
        let mut seen = Seen::default();
        assert_eq!(
            unsafe {
                vfsi_listdirv(
                    fs,
                    dirs.as_ptr(),
                    dirs.len(),
                    0,
                    false,
                    Some(cb),
                    &mut seen as *mut _ as *mut c_void,
                )
            },
            0
        );
        assert_eq!(
            seen.rows,
            vec![
                ("/v/d1".to_string(), "one.bin".to_string()),
                ("/v/d2".to_string(), "two.bin".to_string()),
            ]
        );

        unsafe { vfsi_free(fs) };
    }

    #[test]
    fn dummy_open_mount_accepts_kernel_paths() {
        let root = temp_root();
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(
            unsafe { vfsi_dummy_open_mount(root.as_ptr(), root.as_ptr(), &mut fs) },
            0
        );
        assert!(!fs.is_null());

        let mut path = root.as_bytes().to_vec();
        path.extend_from_slice(b"/mount-dir/file.bin");
        let path = CString::new(path).unwrap();
        let mut dir = root.as_bytes().to_vec();
        dir.extend_from_slice(b"/mount-dir");
        let dir = CString::new(dir).unwrap();

        assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);
        let fd = unsafe {
            vfsi_open(
                fs,
                path.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o644,
            )
        };
        assert!(fd > 0);
        assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
        assert_eq!(unsafe { vfsi_remove(fs, path.as_ptr()) }, 0);
        assert_eq!(unsafe { vfsi_remove(fs, dir.as_ptr()) }, 0);

        unsafe { vfsi_free(fs) };
    }

    #[test]
    fn read_paths_batches_full_file_contents() {
        let root = temp_root();
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(
            unsafe { vfsi_dummy_open_mount(root.as_ptr(), root.as_ptr(), &mut fs) },
            0
        );
        assert!(!fs.is_null());

        let mut dir = root.as_bytes().to_vec();
        dir.extend_from_slice(b"/read-dir");
        let dir = CString::new(dir).unwrap();
        assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);

        let path_a = {
            let mut p = dir.as_bytes().to_vec();
            p.extend_from_slice(b"/a.bin");
            CString::new(p).unwrap()
        };
        let path_b = {
            let mut p = dir.as_bytes().to_vec();
            p.extend_from_slice(b"/b.bin");
            CString::new(p).unwrap()
        };
        for (path, contents) in [(&path_a, &b"first"[..]), (&path_b, &b"second-longer"[..])] {
            let fd = unsafe {
                vfsi_open(
                    fs,
                    path.as_ptr(),
                    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                    0o644,
                )
            };
            assert!(fd > 0);
            assert_eq!(
                unsafe {
                    vfsi_pwrite(
                        fs,
                        fd,
                        contents.as_ptr() as *const c_void,
                        contents.len(),
                        0,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
        }

        #[derive(Default)]
        struct Seen {
            rows: Vec<(String, Vec<u8>)>,
        }
        unsafe extern "C" fn cb(
            path: *const c_char,
            data: *const u8,
            len: usize,
            user: *mut c_void,
        ) -> bool {
            let seen = &mut *(user as *mut Seen);
            let bytes = unsafe { std::slice::from_raw_parts(data, len) };
            seen.rows.push((
                CStr::from_ptr(path).to_string_lossy().into_owned(),
                bytes.to_vec(),
            ));
            true
        }

        let paths = [path_a.as_ptr(), path_b.as_ptr()];
        let mut seen = Seen::default();
        assert_eq!(
            unsafe {
                vfsi_read_paths(
                    fs,
                    paths.as_ptr(),
                    paths.len(),
                    Some(cb),
                    &mut seen as *mut _ as *mut c_void,
                )
            },
            0
        );
        assert_eq!(seen.rows[0].1, b"first");
        assert_eq!(seen.rows[1].1, b"second-longer");

        unsafe { vfsi_free(fs) };
    }
}
