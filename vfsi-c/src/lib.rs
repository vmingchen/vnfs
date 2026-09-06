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
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard};

use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::nfs::NfsVecFs;
use vnfs::smb::SmbVecFs;
use vnfs::vecfs::{
    AttrMask, ExtentPair, ReadOp, VfAttrs, VfError, VfFile, WriteOp, ERR_EBADF, ERR_NOENT,
    VF_ERR_UNSUPPORTED,
};

/// ABI version implemented by this library.
pub const VFSI_ABI_VERSION: u32 = 3;
/// No error occurred.
pub const VFSI_ERROR_NONE: u32 = 0;
/// A filesystem/backend status was returned.
pub const VFSI_ERROR_FILESYSTEM: u32 = 1;
/// The transport failed without a filesystem status.
pub const VFSI_ERROR_TRANSPORT: u32 = 2;
/// The selected backend does not implement the requested operation.
pub const VFSI_ERROR_UNSUPPORTED: u32 = 3;
/// The C request itself was malformed.
pub const VFSI_ERROR_INVALID_ARGUMENT: u32 = 4;
/// The operation was not submitted because an earlier request was invalid.
pub const VFSI_ERROR_NOT_ATTEMPTED: u32 = 5;
/// The backend batch failed and this element's final state cannot be proven.
pub const VFSI_ERROR_INDETERMINATE: u32 = 6;
/// Fixed capacity of [`vfsi_result::message`], including its trailing NUL.
pub const VFSI_RESULT_MESSAGE_SIZE: usize = 160;
pub const VFSI_ATTR_MODE: u32 = 1 << 0;
pub const VFSI_ATTR_SIZE: u32 = 1 << 1;
pub const VFSI_ATTR_NLINK: u32 = 1 << 2;
pub const VFSI_ATTR_FILEID: u32 = 1 << 3;
pub const VFSI_ATTR_BLOCKS: u32 = 1 << 4;
pub const VFSI_ATTR_UID: u32 = 1 << 5;
pub const VFSI_ATTR_GID: u32 = 1 << 6;
pub const VFSI_ATTR_RDEV: u32 = 1 << 7;
pub const VFSI_ATTR_ATIME: u32 = 1 << 8;
pub const VFSI_ATTR_MTIME: u32 = 1 << 9;
pub const VFSI_ATTR_CTIME: u32 = 1 << 10;
/// The backend will currently attempt server-side COPY.
pub const VFSI_CAP_SERVER_COPY: u64 = 1 << 0;
/// The backend reports and honors Unix metadata such as modes and ownership.
pub const VFSI_CAP_POSIX_METADATA: u64 = 1 << 1;
/// The backend supports symbolic links.
pub const VFSI_CAP_SYMLINKS: u64 = 1 << 2;
/// The backend supports hard links.
pub const VFSI_CAP_HARDLINKS: u64 = 1 << 3;
/// The backend accepts arbitrary non-UTF-8 Unix path bytes.
pub const VFSI_CAP_NON_UTF8_PATHS: u64 = 1 << 4;
/// The backend implements no-follow metadata operations.
pub const VFSI_CAP_LSTAT: u64 = 1 << 5;

const _: () = assert!(VFSI_CAP_POSIX_METADATA == vnfs::VF_CAP_POSIX_METADATA);
const _: () = assert!(VFSI_CAP_SYMLINKS == vnfs::VF_CAP_SYMLINKS);
const _: () = assert!(VFSI_CAP_HARDLINKS == vnfs::VF_CAP_HARDLINKS);
const _: () = assert!(VFSI_CAP_NON_UTF8_PATHS == vnfs::VF_CAP_NON_UTF8_PATHS);
const _: () = assert!(VFSI_CAP_LSTAT == vnfs::VF_CAP_LSTAT);

macro_rules! ffi_guard {
    ($fallback:expr, $body:block) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(_) => $fallback,
        }
    }};
}

macro_rules! fail_batch {
    ($results:expr, $failure:expr, $submitted:expr) => {{
        let failure = $failure;
        fail_results($results, failure, $submitted);
        return failure;
    }};
}

/// Opaque filesystem handle owned by C.
pub struct vfsi_fs {
    fs: Mutex<Box<dyn vnfs::VecFs>>,
    files: Mutex<std::collections::HashMap<i32, VfFile>>,
    next_fd: AtomicI32,
    /// Kernel mountpoint used by application-visible paths.
    mountpoint: PathBuf,
    /// Backend path corresponding to `mountpoint`.
    backend_root: PathBuf,
}

/// Attributes returned by [`vfsi_stat`] and passed to listdir callbacks.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_attrs {
    /// Size of this structure, for forward-compatible extension.
    pub struct_size: u32,
    /// ABI version used to populate this structure.
    pub abi_version: u32,
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

/// Uniform ABI-v3 result for scalar and vector operations.
///
/// `index` is the completed count on success and the failing operation index
/// on error. Vector calls also populate a caller-owned result per element;
/// after a submitted concurrent batch fails, every non-failing element is
/// marked indeterminate because it may already have completed. `err_no`
/// retains the backend status while `category` is portable across protocols.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_result {
    pub struct_size: u32,
    pub abi_version: u32,
    pub index: usize,
    pub category: u32,
    pub err_no: u32,
    pub message: [c_char; VFSI_RESULT_MESSAGE_SIZE],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_open_op {
    pub path: *const c_char,
    pub flags: c_int,
    pub mode: u32,
    pub fd: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_stat_op {
    pub path: *const c_char,
    pub attrs: vfsi_attrs,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_setattr_op {
    pub path: *const c_char,
    pub mask: u32,
    pub mode: u32,
    pub size: u64,
    pub atime_sec: i64,
    pub atime_nsec: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_pread_op {
    pub fd: c_int,
    pub buf: *mut c_void,
    pub len: usize,
    pub offset: u64,
    pub got: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_pwrite_op {
    pub fd: c_int,
    pub buf: *const c_void,
    pub len: usize,
    pub offset: u64,
    pub wrote: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_mkdir_op {
    pub path: *const c_char,
    pub mode: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_rename_op {
    pub oldpath: *const c_char,
    pub newpath: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_copy_op {
    pub src: *const c_char,
    pub src_offset: u64,
    pub dst: *const c_char,
    pub dst_offset: u64,
    pub length: u64,
    pub to_eof: bool,
}

impl vfsi_attrs {
    fn from_vf(a: &VfAttrs) -> vfsi_attrs {
        vfsi_attrs {
            struct_size: std::mem::size_of::<vfsi_attrs>() as u32,
            abi_version: VFSI_ABI_VERSION,
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

impl vfsi_result {
    fn base(index: usize, category: u32, err_no: u32, message: &str) -> Self {
        let mut result = Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            abi_version: VFSI_ABI_VERSION,
            index,
            category,
            err_no,
            message: [0; VFSI_RESULT_MESSAGE_SIZE],
        };
        let bytes = message.as_bytes();
        let len = bytes.len().min(VFSI_RESULT_MESSAGE_SIZE - 1);
        for (dst, src) in result.message[..len].iter_mut().zip(&bytes[..len]) {
            *dst = *src as c_char;
        }
        result
    }

    fn success(completed: usize) -> Self {
        Self::base(completed, VFSI_ERROR_NONE, 0, "")
    }

    fn item_success(index: usize) -> Self {
        Self::base(index, VFSI_ERROR_NONE, 0, "")
    }

    fn invalid(index: usize, message: &str) -> Self {
        Self::base(
            index,
            VFSI_ERROR_INVALID_ARGUMENT,
            libc::EINVAL as u32,
            message,
        )
    }

    fn panic() -> Self {
        Self::base(
            0,
            VFSI_ERROR_TRANSPORT,
            libc::EIO as u32,
            "panic across FFI",
        )
    }

    fn from_error(error: VfError) -> Self {
        match error {
            VfError::Op { index, err_no } => Self::base(
                index,
                if err_no == VF_ERR_UNSUPPORTED {
                    VFSI_ERROR_UNSUPPORTED
                } else {
                    VFSI_ERROR_FILESYSTEM
                },
                err_no,
                "",
            ),
            VfError::Transport { index, message } => Self::base(
                index.unwrap_or(0),
                VFSI_ERROR_TRANSPORT,
                vnfs::VF_ERR_RPC,
                &message,
            ),
            _ => Self::base(0, VFSI_ERROR_TRANSPORT, libc::EIO as u32, "unknown error"),
        }
    }
}

// Keeping the error inline avoids allocating while crossing the C ABI; the
// fixed-size result is copied directly into the caller's return value.
#[allow(clippy::result_large_err)]
unsafe fn result_array<'a>(
    results: *mut vfsi_result,
    count: usize,
) -> Result<&'a mut [vfsi_result], vfsi_result> {
    if results.is_null() {
        return Err(vfsi_result::invalid(0, "null result array"));
    }
    let results = std::slice::from_raw_parts_mut(results, count);
    for (index, result) in results.iter_mut().enumerate() {
        *result = vfsi_result::base(index, VFSI_ERROR_NOT_ATTEMPTED, 0, "not attempted");
    }
    Ok(results)
}

fn complete_results(results: &mut [vfsi_result]) {
    for (index, result) in results.iter_mut().enumerate() {
        *result = vfsi_result::item_success(index);
    }
}

fn fail_results(results: &mut [vfsi_result], failure: vfsi_result, submitted: bool) {
    if submitted {
        for (index, result) in results.iter_mut().enumerate() {
            *result = vfsi_result::base(
                index,
                VFSI_ERROR_INDETERMINATE,
                0,
                "batch failed; completion is indeterminate",
            );
        }
    }
    if let Some(result) = results.get_mut(failure.index) {
        *result = failure;
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

pub type vfsi_read_stream_cb = Option<
    unsafe extern "C" fn(
        path: *const c_char,
        index: usize,
        offset: u64,
        data: *const u8,
        len: usize,
        eof: bool,
        userdata: *mut c_void,
    ) -> bool,
>;

/// Return the ABI version implemented by the loaded library.
#[no_mangle]
pub extern "C" fn vfsi_abi_version() -> u32 {
    ffi_guard!(0, { VFSI_ABI_VERSION })
}

/// Return the negotiated NFS minor version, or zero for a non-NFS/invalid
/// handle.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_minorversion(fs: *const vfsi_fs) -> u32 {
    ffi_guard!(0, {
        let Some(fs) = fs.as_ref() else {
            return 0;
        };
        fs.fs
            .lock()
            .ok()
            .and_then(|backend| backend.nfs_minorversion())
            .unwrap_or(0)
    })
}

/// Return the negotiated SMB dialect revision (`0x0202` through `0x0311`),
/// or zero for a non-SMB/invalid handle.
#[no_mangle]
pub unsafe extern "C" fn vfsi_smb_dialect(fs: *const vfsi_fs) -> u16 {
    ffi_guard!(0, {
        let Some(fs) = fs.as_ref() else {
            return 0;
        };
        fs.fs
            .lock()
            .ok()
            .and_then(|backend| backend.smb_dialect())
            .unwrap_or(0)
    })
}

/// Return the current `VFSI_CAP_*` capability bitset.
#[no_mangle]
pub unsafe extern "C" fn vfsi_capabilities(fs: *const vfsi_fs) -> u64 {
    ffi_guard!(0, {
        let Some(fs) = fs.as_ref() else {
            return 0;
        };
        fs.fs
            .lock()
            .map(|backend| backend.capabilities())
            .unwrap_or(0)
    })
}

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

fn lock_or_io<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, VfError> {
    mutex
        .lock()
        .map_err(|_| VfError::failure(0, libc::EIO as u32))
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

fn cstr_utf8(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: C callers must pass a NUL-terminated string which remains valid
    // for the duration of the call.
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn path_for(fs: &vfsi_fs, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(&fs.mountpoint).ok()?;
    let mut mapped = fs.backend_root.clone();
    for component in rel.components() {
        match component {
            Component::Normal(part) => mapped.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(mapped)
}

fn make_fs(fs: Box<dyn vnfs::VecFs>, mountpoint: PathBuf, backend_root: PathBuf) -> *mut vfsi_fs {
    Box::into_raw(Box::new(vfsi_fs {
        fs: Mutex::new(fs),
        files: Mutex::new(std::collections::HashMap::new()),
        next_fd: AtomicI32::new(1),
        mountpoint,
        backend_root,
    }))
}

fn insert_c_file(
    fs: &vfsi_fs,
    files: &mut std::collections::HashMap<i32, VfFile>,
    file: VfFile,
) -> Result<i32, VfError> {
    for _ in 0..i32::MAX {
        let fd = fs
            .next_fd
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.checked_add(1).unwrap_or(1))
            })
            .expect("descriptor update always supplies a value");
        if let std::collections::hash_map::Entry::Vacant(entry) = files.entry(fd) {
            entry.insert(file);
            return Ok(fd);
        }
    }
    Err(VfError::failure(0, libc::EMFILE as u32))
}

fn vpath_for(fs: &vfsi_fs, path: &std::path::Path) -> Result<PathBuf, VfError> {
    path_for(fs, path).ok_or_else(|| VfError::failure(0, ERR_NOENT))
}

/// Create a local-directory vfsi backend rooted at `root`.
#[no_mangle]
pub unsafe extern "C" fn vfsi_dummy_open(root: *const c_char, out: *mut *mut vfsi_fs) -> c_int {
    ffi_guard!(libc::EIO, {
        if root.is_null() || out.is_null() {
            return libc::EINVAL;
        }
        let Some(root) = cstr_path(root) else {
            return libc::EINVAL;
        };
        let fs = Box::new(DummyVecFs::new(root)) as Box<dyn vnfs::VecFs>;
        *out = make_fs(fs, PathBuf::from("/"), PathBuf::from("/"));
        0
    })
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
    ffi_guard!(libc::EIO, {
        if root.is_null() || mountpoint.is_null() || out.is_null() {
            return libc::EINVAL;
        }
        let (Some(root), Some(mountpoint)) = (cstr_path(root), cstr_path(mountpoint)) else {
            return libc::EINVAL;
        };
        let fs = Box::new(DummyVecFs::new(root)) as Box<dyn vnfs::VecFs>;
        *out = make_fs(fs, mountpoint, PathBuf::from("/"));
        0
    })
}

/// Connect to an NFSv4.1 server at `host` and open the export root.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open(host: *const c_char, out: *mut *mut vfsi_fs) -> c_int {
    ffi_guard!(libc::EIO, {
        if host.is_null() || out.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: C callers must pass a NUL-terminated host string.
        let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
            return libc::EINVAL;
        };
        match NfsVecFs::connect(host)
            .map(|f| Box::new(f) as Box<dyn vnfs::VecFs>)
            .map_err(|e| vf_code(&e))
        {
            Ok(fs) => {
                *out = make_fs(fs, PathBuf::from("/"), PathBuf::from("/"));
                0
            }
            Err(code) => code,
        }
    })
}

/// Connect using an explicit supported NFS minor version (1 or 2).
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open_minor(
    host: *const c_char,
    minorversion: u32,
    out: *mut *mut vfsi_fs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        if host.is_null() || out.is_null() || !matches!(minorversion, 1 | 2) {
            return libc::EINVAL;
        }
        let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
            return libc::EINVAL;
        };
        match NfsVecFs::connect_minor(host, minorversion)
            .map(|f| Box::new(f) as Box<dyn vnfs::VecFs>)
            .map_err(|e| vf_code(&e))
        {
            Ok(fs) => {
                *out = make_fs(fs, PathBuf::from("/"), PathBuf::from("/"));
                0
            }
            Err(code) => code,
        }
    })
}

/// Connect to an NFSv4.1 server and treat `mountpoint` (a local kernel mount
/// path) as the vfsi root. Callers can then pass ordinary kernel paths.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open_mount(
    host: *const c_char,
    mountpoint: *const c_char,
    out: *mut *mut vfsi_fs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        unsafe { vfsi_nfs_open_mount_export(host, c"/".as_ptr(), mountpoint, out) }
    })
}

/// Connect to an NFSv4.1 server, mapping a local kernel `mountpoint` to the
/// server-side `export_root` beneath the NFSv4 pseudo-root.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open_mount_export(
    host: *const c_char,
    export_root: *const c_char,
    mountpoint: *const c_char,
    out: *mut *mut vfsi_fs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        if host.is_null() || export_root.is_null() || mountpoint.is_null() || out.is_null() {
            return libc::EINVAL;
        }
        let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
            return libc::EINVAL;
        };
        let (Some(export_root), Some(mountpoint)) = (cstr_path(export_root), cstr_path(mountpoint))
        else {
            return libc::EINVAL;
        };
        if !export_root.is_absolute() || !mountpoint.is_absolute() {
            return libc::EINVAL;
        }
        if export_root
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return libc::EINVAL;
        }
        match NfsVecFs::connect(host)
            .map(|f| Box::new(f) as Box<dyn vnfs::VecFs>)
            .map_err(|e| vf_code(&e))
        {
            Ok(fs) => {
                *out = make_fs(fs, mountpoint, export_root);
                0
            }
            Err(code) => code,
        }
    })
}

/// Connect to an SMB2/3 share. `server` may omit port 445; empty username and
/// password strings request guest access. Paths are rooted at the share root.
#[no_mangle]
pub unsafe extern "C" fn vfsi_smb_open(
    server: *const c_char,
    share: *const c_char,
    username: *const c_char,
    password: *const c_char,
    domain: *const c_char,
    out: *mut *mut vfsi_fs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        if out.is_null() {
            return libc::EINVAL;
        }
        let (Some(server), Some(share), Some(username), Some(password), Some(domain)) = (
            cstr_utf8(server),
            cstr_utf8(share),
            cstr_utf8(username),
            cstr_utf8(password),
            cstr_utf8(domain),
        ) else {
            return libc::EINVAL;
        };
        match SmbVecFs::connect(&server, &share, &username, &password, &domain) {
            Ok(backend) => {
                *out = make_fs(
                    Box::new(backend) as Box<dyn vnfs::VecFs>,
                    PathBuf::from("/"),
                    PathBuf::from("/"),
                );
                0
            }
            Err(error) => vf_code(&error),
        }
    })
}

/// Connect to an SMB2/3 share and map a kernel-visible `mountpoint` onto
/// `share_root` within that share. Both paths must be absolute and may not
/// contain parent-directory components.
#[no_mangle]
pub unsafe extern "C" fn vfsi_smb_open_mount(
    server: *const c_char,
    share: *const c_char,
    username: *const c_char,
    password: *const c_char,
    domain: *const c_char,
    share_root: *const c_char,
    mountpoint: *const c_char,
    out: *mut *mut vfsi_fs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        if out.is_null() {
            return libc::EINVAL;
        }
        let (Some(server), Some(share), Some(username), Some(password), Some(domain)) = (
            cstr_utf8(server),
            cstr_utf8(share),
            cstr_utf8(username),
            cstr_utf8(password),
            cstr_utf8(domain),
        ) else {
            return libc::EINVAL;
        };
        let (Some(share_root), Some(mountpoint)) = (cstr_path(share_root), cstr_path(mountpoint))
        else {
            return libc::EINVAL;
        };
        if !share_root.is_absolute()
            || !mountpoint.is_absolute()
            || share_root
                .components()
                .chain(mountpoint.components())
                .any(|component| matches!(component, Component::ParentDir))
        {
            return libc::EINVAL;
        }
        match SmbVecFs::connect(&server, &share, &username, &password, &domain) {
            Ok(backend) => {
                *out = make_fs(
                    Box::new(backend) as Box<dyn vnfs::VecFs>,
                    mountpoint,
                    share_root,
                );
                0
            }
            Err(error) => vf_code(&error),
        }
    })
}

/// Destroy a filesystem handle returned by one of the `vfsi_*_open*`
/// functions.
#[no_mangle]
pub unsafe extern "C" fn vfsi_free(fs: *mut vfsi_fs) {
    ffi_guard!((), {
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
    })
}

fn stat_impl(fs: &vfsi_fs, path: &std::path::Path) -> Result<VfAttrs, VfError> {
    let vpath = vpath_for(fs, path)?;
    lock_or_io(&fs.fs)?.stat(&vpath)
}

/// Stat `path`, following a final symlink.
#[no_mangle]
pub unsafe extern "C" fn vfsi_stat(
    fs: *mut vfsi_fs,
    path: *const c_char,
    out: *mut vfsi_attrs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        let (Some(fs), Some(path), Some(out)) = (fs.as_ref(), cstr_path(path), out.as_mut()) else {
            return libc::EINVAL;
        };
        match stat_impl(fs, &path) {
            Ok(a) => {
                *out = vfsi_attrs::from_vf(&a);
                0
            }
            Err(e) => vf_code(&e),
        }
    })
}

fn open_impl(
    fs: &vfsi_fs,
    path: &std::path::Path,
    flags: c_int,
    mode: u32,
) -> Result<i32, VfError> {
    let vpath = vpath_for(fs, path)?;
    let file = lock_or_io(&fs.fs)?.open(&vpath, flags, mode)?;
    let mut files = lock_or_io(&fs.files)?;
    insert_c_file(fs, &mut files, file)
}

/// Open a file and return a vfsi descriptor (`>= 0`), or a negative errno.
#[no_mangle]
pub unsafe extern "C" fn vfsi_open(
    fs: *mut vfsi_fs,
    path: *const c_char,
    flags: c_int,
    mode: u32,
) -> c_int {
    ffi_guard!(-libc::EIO, {
        let (Some(fs), Some(path)) = (fs.as_ref(), cstr_path(path)) else {
            return -libc::EINVAL;
        };
        match open_impl(fs, &path, flags, mode) {
            Ok(fd) => fd,
            Err(e) => -vf_code(&e),
        }
    })
}

fn close_impl(fs: &vfsi_fs, fd: i32) -> Result<(), VfError> {
    let file = lock_or_io(&fs.files)?
        .remove(&fd)
        .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
    lock_or_io(&fs.fs)?.close(&file)
}

/// Close a descriptor opened by [`vfsi_open`].
#[no_mangle]
pub unsafe extern "C" fn vfsi_close(fs: *mut vfsi_fs, fd: c_int) -> c_int {
    ffi_guard!(libc::EIO, {
        let Some(fs) = fs.as_ref() else {
            return libc::EINVAL;
        };
        match close_impl(fs, fd) {
            Ok(()) => 0,
            Err(e) => vf_code(&e),
        }
    })
}

fn pread_impl(
    fs: &vfsi_fs,
    fd: i32,
    buf: *mut c_void,
    len: usize,
    offset: u64,
) -> Result<usize, VfError> {
    let file = lock_or_io(&fs.files)?
        .get(&fd)
        .cloned()
        .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
    let data = lock_or_io(&fs.fs)?.read(&file, offset, len)?;
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
    ffi_guard!(libc::EIO, {
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
    })
}

fn pwrite_impl(
    fs: &vfsi_fs,
    fd: i32,
    buf: *const c_void,
    len: usize,
    offset: u64,
) -> Result<usize, VfError> {
    let file = lock_or_io(&fs.files)?
        .get(&fd)
        .cloned()
        .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
    // SAFETY: C caller must provide a valid buffer of `len` bytes.
    let data = unsafe { std::slice::from_raw_parts(buf as *const u8, len) };
    lock_or_io(&fs.fs)?.write(&file, offset, data)
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
    ffi_guard!(libc::EIO, {
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
    })
}

/// Create a directory (`create_parents != 0` behaves like `mkdir -p`).
#[no_mangle]
pub unsafe extern "C" fn vfsi_mkdir(
    fs: *mut vfsi_fs,
    path: *const c_char,
    mode: u32,
    create_parents: c_int,
) -> c_int {
    ffi_guard!(libc::EIO, {
        let (Some(fs), Some(path)) = (fs.as_ref(), cstr_path(path)) else {
            return libc::EINVAL;
        };
        let result: Result<(), VfError> = (|| {
            let path = vpath_for(fs, &path)?;
            let mut f = lock_or_io(&fs.fs)?;
            if create_parents != 0 {
                f.ensure_dir(&path, mode)
            } else {
                f.mkdir(&path, mode)
            }
        })();
        match result {
            Ok(()) => 0,
            Err(e) => vf_code(&e),
        }
    })
}

/// Remove a path (directories must be empty).
#[no_mangle]
pub unsafe extern "C" fn vfsi_remove(fs: *mut vfsi_fs, path: *const c_char) -> c_int {
    ffi_guard!(libc::EIO, {
        let (Some(fs), Some(path)) = (fs.as_ref(), cstr_path(path)) else {
            return libc::EINVAL;
        };
        let result: Result<(), VfError> = (|| {
            let path = vpath_for(fs, &path)?;
            lock_or_io(&fs.fs)?.unlink(&path)
        })();
        match result {
            Ok(()) => 0,
            Err(e) => vf_code(&e),
        }
    })
}

/// Rename `oldpath` to `newpath`.
#[no_mangle]
pub unsafe extern "C" fn vfsi_rename(
    fs: *mut vfsi_fs,
    oldpath: *const c_char,
    newpath: *const c_char,
) -> c_int {
    ffi_guard!(libc::EIO, {
        let (Some(fs), Some(old), Some(new)) =
            (fs.as_ref(), cstr_path(oldpath), cstr_path(newpath))
        else {
            return libc::EINVAL;
        };
        let result: Result<(), VfError> = (|| {
            let old = vpath_for(fs, &old)?;
            let new = vpath_for(fs, &new)?;
            lock_or_io(&fs.fs)?.renamev(&[(VfFile::from_os_path(&old), VfFile::from_os_path(&new))])
        })();
        match result {
            Ok(()) => 0,
            Err(e) => vf_code(&e),
        }
    })
}

/// Copy one extent. When `to_eof` is true, `length` is ignored and copying
/// continues to the source EOF. The backend uses NFSv4.2 COPY or SMB
/// server-side copy when available and falls back to client-side I/O.
#[no_mangle]
pub unsafe extern "C" fn vfsi_copy(
    fs: *mut vfsi_fs,
    src: *const c_char,
    src_offset: u64,
    dst: *const c_char,
    dst_offset: u64,
    length: u64,
    to_eof: bool,
) -> c_int {
    ffi_guard!(libc::EIO, {
        let Some(fs) = fs.as_ref() else {
            return libc::EINVAL;
        };
        let (Some(src), Some(dst)) = (cstr_path(src), cstr_path(dst)) else {
            return libc::EINVAL;
        };
        let (Some(src), Some(dst)) = (path_for(fs, &src), path_for(fs, &dst)) else {
            return libc::ENOENT;
        };
        let pair = ExtentPair::from_os_paths(
            &src,
            src_offset,
            &dst,
            dst_offset,
            (!to_eof).then_some(length),
        );
        match lock_or_io(&fs.fs).and_then(|mut backend| backend.copyv(&[pair])) {
            Ok(()) => 0,
            Err(error) => vf_code(&error),
        }
    })
}

/// Open `count` files in one backend vector call. ABI-v2 functions remain
/// available; this and the other `*v` entry points use the uniform ABI-v3
/// overall and per-element result contract.
#[no_mangle]
pub unsafe extern "C" fn vfsi_openv(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_open_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        let Some(ops) = ops.as_mut() else {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        };
        let ops = std::slice::from_raw_parts_mut(ops, count);
        let mut paths = Vec::with_capacity(count);
        let mut flags = Vec::with_capacity(count);
        let mut modes = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            let Some(path) = cstr_path(op.path) else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            match vpath_for(fs, &path) {
                Ok(path) => paths.push(path),
                Err(error) => fail_batch!(
                    results,
                    vfsi_result::from_error(error.with_index(index)),
                    false
                ),
            }
            flags.push(op.flags);
            modes.push(op.mode);
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let opened = {
            let mut backend = match lock_or_io(&fs.fs) {
                Ok(backend) => backend,
                Err(error) => fail_batch!(results, vfsi_result::from_error(error), true),
            };
            match backend.openv(&refs, &flags, &modes) {
                Ok(opened) => opened,
                Err(error) => return vfsi_result::from_error(error),
            }
        };
        let mut registered = Vec::with_capacity(count);
        let registration = (|| -> Result<(), VfError> {
            let mut files = lock_or_io(&fs.files)?;
            for (index, (op, file)) in ops.iter_mut().zip(&opened).enumerate() {
                let fd = insert_c_file(fs, &mut files, file.clone())
                    .map_err(|error| error.with_index(index))?;
                op.fd = fd;
                registered.push(fd);
            }
            Ok(())
        })();
        if let Err(error) = registration {
            if let Ok(mut files) = fs.files.lock() {
                for fd in registered {
                    files.remove(&fd);
                }
            }
            if let Ok(mut backend) = fs.fs.lock() {
                let _ = backend.closev(&opened);
            }
            fail_batch!(results, vfsi_result::from_error(error), true);
        }
        complete_results(results);
        vfsi_result::success(count)
    })
}

/// Close a descriptor array in one backend vector call.
#[no_mangle]
pub unsafe extern "C" fn vfsi_closev(
    fs: *mut vfsi_fs,
    fds: *const c_int,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        if fds.is_null() {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null descriptor array"),
                false
            );
        }
        let fds = std::slice::from_raw_parts(fds, count);
        let files = {
            let mut table = match lock_or_io(&fs.files) {
                Ok(table) => table,
                Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
            };
            let mut files = Vec::with_capacity(count);
            for (index, fd) in fds.iter().enumerate() {
                let Some(file) = table.get(fd).cloned() else {
                    fail_batch!(
                        results,
                        vfsi_result::from_error(VfError::failure(index, ERR_EBADF)),
                        false
                    );
                };
                files.push(file);
            }
            for fd in fds {
                table.remove(fd);
            }
            files
        };
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        match backend.closev(&files) {
            Ok(()) => {
                complete_results(results);
                vfsi_result::success(count)
            }
            Err(error) => {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, true);
                failure
            }
        }
    })
}

/// Stat a path array in one backend vector call.
#[no_mangle]
pub unsafe extern "C" fn vfsi_statv(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_stat_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        let Some(ops) = ops.as_mut() else {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        };
        let ops = std::slice::from_raw_parts_mut(ops, count);
        let mut attrs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            let Some(path) = cstr_path(op.path) else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            let path = match vpath_for(fs, &path) {
                Ok(path) => path,
                Err(error) => fail_batch!(
                    results,
                    vfsi_result::from_error(error.with_index(index)),
                    false
                ),
            };
            attrs.push(VfAttrs {
                file: VfFile::from_os_path(&path),
                masks: mask(),
                ..VfAttrs::default()
            });
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        if let Err(error) = backend.getattrsv(&mut attrs) {
            fail_batch!(results, vfsi_result::from_error(error), true);
        }
        for (op, attrs) in ops.iter_mut().zip(&attrs) {
            op.attrs = vfsi_attrs::from_vf(attrs);
        }
        complete_results(results);
        vfsi_result::success(count)
    })
}

/// Set selected attributes for a path array in one backend vector call.
#[no_mangle]
pub unsafe extern "C" fn vfsi_setattrv(
    fs: *mut vfsi_fs,
    ops: *const vfsi_setattr_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        if ops.is_null() {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        }
        let ops = std::slice::from_raw_parts(ops, count);
        let mut attrs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            let Some(path) = cstr_path(op.path) else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            let path = match vpath_for(fs, &path) {
                Ok(path) => path,
                Err(error) => fail_batch!(
                    results,
                    vfsi_result::from_error(error.with_index(index)),
                    false
                ),
            };
            let Some(masks) = AttrMask::from_bits(op.mask) else {
                fail_batch!(
                    results,
                    vfsi_result::invalid(index, "unknown attribute mask bit"),
                    false
                );
            };
            attrs.push(VfAttrs {
                file: VfFile::from_os_path(&path),
                masks,
                mode: op.mode,
                size: op.size,
                atime_sec: op.atime_sec,
                atime_nsec: op.atime_nsec,
                mtime_sec: op.mtime_sec,
                mtime_nsec: op.mtime_nsec,
                ..VfAttrs::default()
            });
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        match backend.setattrsv(&attrs) {
            Ok(()) => {
                complete_results(results);
                vfsi_result::success(count)
            }
            Err(error) => {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, true);
                failure
            }
        }
    })
}

/// Positioned vector read using caller-owned buffers.
#[no_mangle]
pub unsafe extern "C" fn vfsi_preadv(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_pread_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let item_results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        let Some(ops) = ops.as_mut() else {
            fail_batch!(
                item_results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        };
        let ops = std::slice::from_raw_parts_mut(ops, count);
        let table = match lock_or_io(&fs.files) {
            Ok(table) => table,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), false)
            }
        };
        let mut reads = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            if op.len > 0 && op.buf.is_null() {
                fail_batch!(
                    item_results,
                    vfsi_result::invalid(index, "null read buffer"),
                    false
                );
            }
            let Some(file) = table.get(&op.fd).cloned() else {
                fail_batch!(
                    item_results,
                    vfsi_result::from_error(VfError::failure(index, ERR_EBADF)),
                    false
                );
            };
            reads.push(ReadOp::at(file, op.offset, op.len));
        }
        drop(table);
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), false)
            }
        };
        let results = match backend.readv(&reads) {
            Ok(results) => results,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), true)
            }
        };
        for (op, result) in ops.iter_mut().zip(results) {
            if !result.data.is_empty() {
                std::ptr::copy_nonoverlapping(
                    result.data.as_ptr(),
                    op.buf.cast::<u8>(),
                    result.data.len(),
                );
            }
            op.got = result.data.len();
        }
        complete_results(item_results);
        vfsi_result::success(count)
    })
}

/// Positioned vector write using caller-owned buffers.
#[no_mangle]
pub unsafe extern "C" fn vfsi_pwritev(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_pwrite_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let item_results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        let Some(ops) = ops.as_mut() else {
            fail_batch!(
                item_results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        };
        let ops = std::slice::from_raw_parts_mut(ops, count);
        let table = match lock_or_io(&fs.files) {
            Ok(table) => table,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), false)
            }
        };
        let mut writes = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            if op.len > 0 && op.buf.is_null() {
                fail_batch!(
                    item_results,
                    vfsi_result::invalid(index, "null write buffer"),
                    false
                );
            }
            let Some(file) = table.get(&op.fd).cloned() else {
                fail_batch!(
                    item_results,
                    vfsi_result::from_error(VfError::failure(index, ERR_EBADF)),
                    false
                );
            };
            let data = if op.len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(op.buf.cast::<u8>(), op.len).to_vec()
            };
            writes.push(WriteOp::at(file, op.offset, data));
        }
        drop(table);
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), false)
            }
        };
        let results = match backend.writev(&writes) {
            Ok(results) => results,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), true)
            }
        };
        for (op, result) in ops.iter_mut().zip(results) {
            op.wrote = result.written;
        }
        complete_results(item_results);
        vfsi_result::success(count)
    })
}

/// Create a directory array in one backend vector call.
#[no_mangle]
pub unsafe extern "C" fn vfsi_mkdirv(
    fs: *mut vfsi_fs,
    ops: *const vfsi_mkdir_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        if ops.is_null() {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        }
        let ops = std::slice::from_raw_parts(ops, count);
        let mut dirs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            let Some(path) = cstr_path(op.path) else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            let path = match vpath_for(fs, &path) {
                Ok(path) => path,
                Err(error) => fail_batch!(
                    results,
                    vfsi_result::from_error(error.with_index(index)),
                    false
                ),
            };
            dirs.push(VfAttrs {
                file: VfFile::from_os_path(&path),
                masks: AttrMask::MODE,
                mode: op.mode,
                ..VfAttrs::default()
            });
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        match backend.mkdirv(&dirs) {
            Ok(()) => {
                complete_results(results);
                vfsi_result::success(count)
            }
            Err(error) => {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, true);
                failure
            }
        }
    })
}

/// Remove a path array in one backend vector call.
#[no_mangle]
pub unsafe extern "C" fn vfsi_removev(
    fs: *mut vfsi_fs,
    paths: *const *const c_char,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        if paths.is_null() {
            fail_batch!(results, vfsi_result::invalid(0, "null path array"), false);
        }
        let paths = std::slice::from_raw_parts(paths, count);
        let mut files = Vec::with_capacity(count);
        for (index, path) in paths.iter().enumerate() {
            let Some(path) = cstr_path(*path) else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            let path = match vpath_for(fs, &path) {
                Ok(path) => path,
                Err(error) => fail_batch!(
                    results,
                    vfsi_result::from_error(error.with_index(index)),
                    false
                ),
            };
            files.push(VfFile::from_os_path(&path));
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        match backend.removev(&files) {
            Ok(()) => {
                complete_results(results);
                vfsi_result::success(count)
            }
            Err(error) => {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, true);
                failure
            }
        }
    })
}

/// Rename a path-pair array in one backend vector call.
#[no_mangle]
pub unsafe extern "C" fn vfsi_renamev(
    fs: *mut vfsi_fs,
    ops: *const vfsi_rename_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        if ops.is_null() {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        }
        let ops = std::slice::from_raw_parts(ops, count);
        let mut pairs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            let (Some(oldpath), Some(newpath)) = (cstr_path(op.oldpath), cstr_path(op.newpath))
            else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            let (oldpath, newpath) = match (vpath_for(fs, &oldpath), vpath_for(fs, &newpath)) {
                (Ok(oldpath), Ok(newpath)) => (oldpath, newpath),
                (Err(error), _) | (_, Err(error)) => {
                    fail_batch!(
                        results,
                        vfsi_result::from_error(error.with_index(index)),
                        false
                    );
                }
            };
            pairs.push((
                VfFile::from_os_path(&oldpath),
                VfFile::from_os_path(&newpath),
            ));
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        match backend.renamev(&pairs) {
            Ok(()) => {
                complete_results(results);
                vfsi_result::success(count)
            }
            Err(error) => {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, true);
                failure
            }
        }
    })
}

/// Copy an extent-pair array in one backend vector call.
#[no_mangle]
pub unsafe extern "C" fn vfsi_copyv(
    fs: *mut vfsi_fs,
    ops: *const vfsi_copy_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        let results = match result_array(item_results, count) {
            Ok(results) => results,
            Err(error) => return error,
        };
        if ops.is_null() {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        }
        let ops = std::slice::from_raw_parts(ops, count);
        let mut pairs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            let (Some(src), Some(dst)) = (cstr_path(op.src), cstr_path(op.dst)) else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            let (Some(src), Some(dst)) = (path_for(fs, &src), path_for(fs, &dst)) else {
                fail_batch!(
                    results,
                    vfsi_result::from_error(VfError::failure(index, ERR_NOENT)),
                    false
                );
            };
            pairs.push(ExtentPair::from_os_paths(
                &src,
                op.src_offset,
                &dst,
                op.dst_offset,
                (!op.to_eof).then_some(op.length),
            ));
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        match backend.copyv(&pairs) {
            Ok(()) => {
                complete_results(results);
                vfsi_result::success(count)
            }
            Err(error) => {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, true);
                failure
            }
        }
    })
}

/// Stream several paths in bounded vectorized chunks. Returning `false` from
/// `cb` cancels successfully. The callback provides backpressure and must not
/// reenter the same filesystem handle.
#[no_mangle]
pub unsafe extern "C" fn vfsi_read_streamv(
    fs: *mut vfsi_fs,
    paths: *const *const c_char,
    count: usize,
    chunk_size: usize,
    memory_limit: usize,
    cb: vfsi_read_stream_cb,
    userdata: *mut c_void,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        let Some(fs) = fs.as_ref() else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        let Some(cb) = cb else {
            return vfsi_result::invalid(0, "null callback");
        };
        if chunk_size == 0 || memory_limit == 0 {
            return vfsi_result::invalid(0, "chunk and memory limits must be non-zero");
        }
        if count == 0 {
            return vfsi_result::success(0);
        }
        if paths.is_null() {
            return vfsi_result::invalid(0, "null path array");
        }
        let paths = std::slice::from_raw_parts(paths, count);
        let mut cpaths = Vec::with_capacity(count);
        let mut files = Vec::with_capacity(count);
        for (index, path) in paths.iter().enumerate() {
            let Some(kernel_path) = cstr_path(*path) else {
                return vfsi_result::invalid(index, "null path");
            };
            let Some(cpath) = cstr_from_os(kernel_path.as_os_str().as_bytes()) else {
                return vfsi_result::invalid(index, "path contains NUL");
            };
            let vpath = match vpath_for(fs, &kernel_path) {
                Ok(path) => path,
                Err(error) => return vfsi_result::from_error(error.with_index(index)),
            };
            cpaths.push(cpath);
            files.push(VfFile::from_os_path(&vpath));
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => return vfsi_result::from_error(error),
        };
        let result = backend.read_streamv(
            &files,
            chunk_size,
            memory_limit,
            &mut |index, offset, data, eof| {
                cb(
                    cpaths[index].as_ptr(),
                    index,
                    offset,
                    data.as_ptr(),
                    data.len(),
                    eof,
                    userdata,
                )
            },
        );
        match result {
            Ok(()) => vfsi_result::success(count),
            Err(error) => vfsi_result::from_error(error),
        }
    })
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
    ffi_guard!(libc::EIO, {
        let (Some(fs), Some(dir)) = (fs.as_ref(), cstr_path(dir)) else {
            return libc::EINVAL;
        };
        let Some(cb) = cb else {
            return libc::EINVAL;
        };
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = vpath_for(fs, &dir)?;
            let entries = lock_or_io(&fs.fs)?.listdir(&dir, mask(), 0, false)?;
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
    })
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
    ffi_guard!(libc::EIO, {
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
            let mut rows = Vec::new();
            {
                let mut backend = lock_or_io(&fs.fs)?;
                backend.listdirv(&refs, mask(), max_entries, recursive, &mut |attrs, dir| {
                    rows.push((attrs.clone(), dir.to_path_buf()));
                    true
                })?;
            }
            for (attrs, dir) in rows {
                let kernel_dir = match vpath_index.get(&dir) {
                    Some(&i) => original_paths[i].clone(),
                    None => {
                        let rel = dir.strip_prefix("/").unwrap_or(&dir);
                        mountpoint.join(rel)
                    }
                };
                let dir = kernel_dir.as_os_str().as_bytes();
                let Some(dir) = cstr_from_os(dir) else {
                    break;
                };
                let name = attrs
                    .file
                    .path()
                    .and_then(|p| p.file_name())
                    .map(|n| n.as_bytes())
                    .and_then(cstr_from_os)
                    .unwrap_or_default();
                let a = vfsi_attrs::from_vf(&attrs);
                // SAFETY: `c_cb` was provided by C and is invoked from the same
                // thread while its arguments are alive. No filesystem mutex is held.
                if !unsafe { c_cb(dir.as_ptr(), name.as_ptr(), &a, userdata) } {
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
    })
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
    ffi_guard!(libc::EIO, {
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
            let datas = lock_or_io(&fs.fs)?.read_allv(&files)?;
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
    })
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
    fn smb_constructor_rejects_invalid_arguments_before_connecting() {
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                vfsi_smb_open(
                    std::ptr::null(),
                    c"share".as_ptr(),
                    c"".as_ptr(),
                    c"".as_ptr(),
                    c"".as_ptr(),
                    &mut fs,
                )
            },
            libc::EINVAL
        );
        let invalid_utf8 = [0xff_u8, 0];
        assert_eq!(
            unsafe {
                vfsi_smb_open(
                    invalid_utf8.as_ptr().cast(),
                    c"share".as_ptr(),
                    c"".as_ptr(),
                    c"".as_ptr(),
                    c"".as_ptr(),
                    &mut fs,
                )
            },
            libc::EINVAL
        );
        assert!(fs.is_null());
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

        let sub = CString::new("/v/d1/sub").unwrap();
        let nested = CString::new("/v/d1/sub/nested.bin").unwrap();
        assert_eq!(unsafe { vfsi_mkdir(fs, sub.as_ptr(), 0o755, 1) }, 0);
        let fd = unsafe {
            vfsi_open(
                fs,
                nested.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o644,
            )
        };
        assert!(fd > 0);
        assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);
        let one_dir = [d1.as_ptr()];
        seen.rows.clear();
        assert_eq!(
            unsafe {
                vfsi_listdirv(
                    fs,
                    one_dir.as_ptr(),
                    one_dir.len(),
                    0,
                    true,
                    Some(cb),
                    &mut seen as *mut _ as *mut c_void,
                )
            },
            0
        );
        assert!(seen
            .rows
            .contains(&("/v/d1/sub".to_string(), "nested.bin".to_string())));

        unsafe { vfsi_free(fs) };
    }

    #[test]
    fn listdirv_callback_can_reenter_filesystem() {
        let root = temp_root();
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
        let dir = CString::new("/reentrant").unwrap();
        let path = CString::new("/reentrant/file").unwrap();
        assert_eq!(unsafe { vfsi_mkdir(fs, dir.as_ptr(), 0o755, 1) }, 0);
        let fd = unsafe { vfsi_open(fs, path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
        assert!(fd > 0);
        assert_eq!(unsafe { vfsi_close(fs, fd) }, 0);

        struct Reentrant {
            fs: *mut vfsi_fs,
            path: CString,
            stat_result: c_int,
        }
        unsafe extern "C" fn cb(
            _: *const c_char,
            _: *const c_char,
            _: *const vfsi_attrs,
            user: *mut c_void,
        ) -> bool {
            let state = &mut *(user as *mut Reentrant);
            let mut attrs = std::mem::zeroed();
            state.stat_result = vfsi_stat(state.fs, state.path.as_ptr(), &mut attrs);
            false
        }

        let dirs = [dir.as_ptr()];
        let mut state = Reentrant {
            fs,
            path,
            stat_result: -1,
        };
        assert_eq!(
            unsafe {
                vfsi_listdirv(
                    fs,
                    dirs.as_ptr(),
                    dirs.len(),
                    0,
                    false,
                    Some(cb),
                    &mut state as *mut _ as *mut c_void,
                )
            },
            0
        );
        assert_eq!(state.stat_result, 0);
        unsafe { vfsi_free(fs) };
    }

    #[test]
    fn poisoned_backend_mutex_returns_eio() {
        let root = temp_root();
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
        let handle = unsafe { &*fs };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.fs.lock().unwrap();
            panic!("poison backend mutex");
        }));

        let path = CString::new("/").unwrap();
        let mut attrs = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { vfsi_stat(fs, path.as_ptr(), &mut attrs) },
            libc::EIO
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
    fn mount_mapping_includes_backend_root_and_rejects_escape() {
        let root = temp_root();
        let backend = Box::new(DummyVecFs::new(PathBuf::from(
            root.to_string_lossy().into_owned(),
        ))) as Box<dyn vnfs::VecFs>;
        let raw = make_fs(
            backend,
            PathBuf::from("/mnt/repos"),
            PathBuf::from("/exports/git"),
        );
        let fs = unsafe { &*raw };

        assert_eq!(
            path_for(fs, Path::new("/mnt/repos/project/objects")),
            Some(PathBuf::from("/exports/git/project/objects"))
        );
        assert_eq!(path_for(fs, Path::new("/mnt/repos/../escape")), None);

        unsafe { vfsi_free(raw) };
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

    #[test]
    fn abi_v3_vector_io_and_bounded_streaming() {
        let root = temp_root();
        let mut fs: *mut vfsi_fs = std::ptr::null_mut();
        assert_eq!(unsafe { vfsi_dummy_open(root.as_ptr(), &mut fs) }, 0);
        let mut item_results = [vfsi_result::panic(); 2];
        let mut one_result = [vfsi_result::panic(); 1];

        let dir_a = CString::new("/v3-a").unwrap();
        let dir_b = CString::new("/v3-b").unwrap();
        let mkdir_ops = [
            vfsi_mkdir_op {
                path: dir_a.as_ptr(),
                mode: 0o755,
            },
            vfsi_mkdir_op {
                path: dir_b.as_ptr(),
                mode: 0o755,
            },
        ];
        let result = unsafe {
            vfsi_mkdirv(
                fs,
                mkdir_ops.as_ptr(),
                mkdir_ops.len(),
                item_results.as_mut_ptr(),
            )
        };
        assert_eq!(result.category, VFSI_ERROR_NONE);
        assert_eq!(result.index, 2);
        assert!(item_results
            .iter()
            .all(|result| result.category == VFSI_ERROR_NONE));

        let path_a = CString::new("/v3-a/a.bin").unwrap();
        let path_b = CString::new("/v3-b/b.bin").unwrap();
        let mut open_ops = [
            vfsi_open_op {
                path: path_a.as_ptr(),
                flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                mode: 0o644,
                fd: -1,
            },
            vfsi_open_op {
                path: path_b.as_ptr(),
                flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                mode: 0o644,
                fd: -1,
            },
        ];
        let result = unsafe {
            vfsi_openv(
                fs,
                open_ops.as_mut_ptr(),
                open_ops.len(),
                item_results.as_mut_ptr(),
            )
        };
        assert_eq!(result.category, VFSI_ERROR_NONE);
        assert!(open_ops.iter().all(|op| op.fd > 0));

        let data_a = b"abcdef";
        let data_b = b"1234567";
        let mut writes = [
            vfsi_pwrite_op {
                fd: open_ops[0].fd,
                buf: data_a.as_ptr().cast(),
                len: data_a.len(),
                offset: 0,
                wrote: 0,
            },
            vfsi_pwrite_op {
                fd: open_ops[1].fd,
                buf: data_b.as_ptr().cast(),
                len: data_b.len(),
                offset: 0,
                wrote: 0,
            },
        ];
        assert_eq!(
            unsafe {
                vfsi_pwritev(
                    fs,
                    writes.as_mut_ptr(),
                    writes.len(),
                    item_results.as_mut_ptr(),
                )
            }
            .category,
            VFSI_ERROR_NONE
        );
        assert_eq!([writes[0].wrote, writes[1].wrote], [6, 7]);

        let mut buf_a = [0u8; 8];
        let mut buf_b = [0u8; 8];
        let mut reads = [
            vfsi_pread_op {
                fd: open_ops[0].fd,
                buf: buf_a.as_mut_ptr().cast(),
                len: buf_a.len(),
                offset: 0,
                got: 0,
            },
            vfsi_pread_op {
                fd: open_ops[1].fd,
                buf: buf_b.as_mut_ptr().cast(),
                len: buf_b.len(),
                offset: 0,
                got: 0,
            },
        ];
        assert_eq!(
            unsafe {
                vfsi_preadv(
                    fs,
                    reads.as_mut_ptr(),
                    reads.len(),
                    item_results.as_mut_ptr(),
                )
            }
            .category,
            VFSI_ERROR_NONE
        );
        assert_eq!(&buf_a[..reads[0].got], data_a);
        assert_eq!(&buf_b[..reads[1].got], data_b);

        let fds = [open_ops[0].fd, open_ops[1].fd];
        assert_eq!(
            unsafe { vfsi_closev(fs, fds.as_ptr(), fds.len(), item_results.as_mut_ptr(),) }
                .category,
            VFSI_ERROR_NONE
        );

        #[derive(Default)]
        struct Streamed {
            files: [Vec<u8>; 2],
            max_chunk: usize,
        }
        unsafe extern "C" fn stream_cb(
            _: *const c_char,
            index: usize,
            _: u64,
            data: *const u8,
            len: usize,
            _: bool,
            userdata: *mut c_void,
        ) -> bool {
            let state = &mut *userdata.cast::<Streamed>();
            state.max_chunk = state.max_chunk.max(len);
            state.files[index].extend_from_slice(std::slice::from_raw_parts(data, len));
            true
        }
        let paths = [path_a.as_ptr(), path_b.as_ptr()];
        let mut streamed = Streamed::default();
        let result = unsafe {
            vfsi_read_streamv(
                fs,
                paths.as_ptr(),
                paths.len(),
                3,
                4,
                Some(stream_cb),
                &mut streamed as *mut _ as *mut c_void,
            )
        };
        assert_eq!(result.category, VFSI_ERROR_NONE);
        assert!(streamed.max_chunk <= 3);
        assert_eq!(streamed.files[0], data_a);
        assert_eq!(streamed.files[1], data_b);

        let mut stats = [
            vfsi_stat_op {
                path: path_a.as_ptr(),
                attrs: unsafe { std::mem::zeroed() },
            },
            vfsi_stat_op {
                path: path_b.as_ptr(),
                attrs: unsafe { std::mem::zeroed() },
            },
        ];
        assert_eq!(
            unsafe {
                vfsi_statv(
                    fs,
                    stats.as_mut_ptr(),
                    stats.len(),
                    item_results.as_mut_ptr(),
                )
            }
            .category,
            VFSI_ERROR_NONE
        );
        assert_eq!([stats[0].attrs.size, stats[1].attrs.size], [6, 7]);

        let setattr = [vfsi_setattr_op {
            path: path_a.as_ptr(),
            mask: VFSI_ATTR_SIZE,
            mode: 0,
            size: 2,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
        }];
        assert_eq!(
            unsafe { vfsi_setattrv(fs, setattr.as_ptr(), setattr.len(), one_result.as_mut_ptr(),) }
                .category,
            VFSI_ERROR_NONE
        );

        let renamed_a = CString::new("/v3-a/renamed.bin").unwrap();
        let renamed_b = CString::new("/v3-b/renamed.bin").unwrap();
        let renames = [
            vfsi_rename_op {
                oldpath: path_a.as_ptr(),
                newpath: renamed_a.as_ptr(),
            },
            vfsi_rename_op {
                oldpath: path_b.as_ptr(),
                newpath: renamed_b.as_ptr(),
            },
        ];
        assert_eq!(
            unsafe {
                vfsi_renamev(
                    fs,
                    renames.as_ptr(),
                    renames.len(),
                    item_results.as_mut_ptr(),
                )
            }
            .category,
            VFSI_ERROR_NONE
        );
        let remove_paths = [renamed_a.as_ptr(), renamed_b.as_ptr()];
        assert_eq!(
            unsafe {
                vfsi_removev(
                    fs,
                    remove_paths.as_ptr(),
                    remove_paths.len(),
                    item_results.as_mut_ptr(),
                )
            }
            .category,
            VFSI_ERROR_NONE
        );
        assert_eq!(
            unsafe {
                vfsi_removev(
                    fs,
                    [dir_a.as_ptr(), dir_b.as_ptr()].as_ptr(),
                    2,
                    item_results.as_mut_ptr(),
                )
            }
            .category,
            VFSI_ERROR_NONE
        );

        let missing = CString::new("/missing").unwrap();
        let root_path = CString::new("/").unwrap();
        let mut failed_stats = [
            vfsi_stat_op {
                path: missing.as_ptr(),
                attrs: unsafe { std::mem::zeroed() },
            },
            vfsi_stat_op {
                path: root_path.as_ptr(),
                attrs: unsafe { std::mem::zeroed() },
            },
        ];
        let failed = unsafe {
            vfsi_statv(
                fs,
                failed_stats.as_mut_ptr(),
                failed_stats.len(),
                item_results.as_mut_ptr(),
            )
        };
        assert_eq!(failed.index, 0);
        assert_eq!(failed.category, VFSI_ERROR_FILESYSTEM);
        assert_eq!(item_results[0].category, VFSI_ERROR_FILESYSTEM);
        assert_eq!(item_results[1].category, VFSI_ERROR_INDETERMINATE);
        unsafe { vfsi_free(fs) };
    }
}
