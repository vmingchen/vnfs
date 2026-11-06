//! Backend construction, lifecycle, and protocol inspection.
use super::*;

/// Return the ABI version implemented by the loaded library.
#[no_mangle]
pub extern "C" fn vfsi_abi_version() -> u32 {
    ffi_guard!(0, { VFSI_ABI_VERSION })
}

/// Return the negotiated NFS minor version, or zero for a non-NFS/invalid
/// handle.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_minorversion(fs: *const vfsi_fs) -> u32 {
    ffi_guard!(0, {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
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
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_smb_dialect(fs: *const vfsi_fs) -> u16 {
    ffi_guard!(0, {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
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
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_capabilities(fs: *const vfsi_fs) -> u64 {
    ffi_guard!(0, {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return 0;
        };
        fs.fs
            .lock()
            .map(|backend| backend.capabilities().bits())
            .unwrap_or(0)
    })
}

pub(super) fn make_fs(
    fs: Box<dyn BindingBackend>,
    mountpoint: PathBuf,
    backend_root: PathBuf,
) -> *mut vfsi_fs {
    let namespace_id = identity::namespace_id(fs.identity_namespace().as_deref());
    Box::into_raw(Box::new(vfsi_fs {
        fs: Mutex::new(fs),
        files: Mutex::new(std::collections::HashMap::new()),
        next_fd: AtomicI32::new(1),
        mountpoint,
        backend_root,
        namespace_id,
        cursor_owner: std::sync::Arc::new(()),
    }))
}

/// Create a local-directory vfsi backend rooted at `root`.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_dummy_open(root: *const c_char, out: *mut *mut vfsi_fs) -> c_int {
    ffi_guard!(libc::EIO, {
        if root.is_null() || out.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        let Some(root) = (unsafe { cstr_path(root) }) else {
            return libc::EINVAL;
        };
        let fs = match vfsi_posix::backend(root) {
            Ok(fs) => Box::new(fs) as Box<dyn BindingBackend>,
            Err(error) => return vf_code(&error),
        };
        // SAFETY: out was checked for null; the caller provides writable storage for this
        // returned handle.
        unsafe { *out = make_fs(fs, PathBuf::from("/"), PathBuf::from("/")) };
        0
    })
}

/// Create a local-directory vfsi backend rooted at `root`, treating
/// `mountpoint` as the kernel-visible root of the same directory. Callers can
/// pass ordinary kernel paths under `mountpoint`, which are mapped to `/`-rooted
/// vfsi paths before they reach the backend.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        let (Some(root), Some(mountpoint)) =
            (unsafe { cstr_path(root) }, unsafe { cstr_path(mountpoint) })
        else {
            return libc::EINVAL;
        };
        let fs = match vfsi_posix::backend(root) {
            Ok(fs) => Box::new(fs) as Box<dyn BindingBackend>,
            Err(error) => return vf_code(&error),
        };
        // SAFETY: out was checked for null; the caller provides writable storage for this
        // returned handle.
        unsafe { *out = make_fs(fs, mountpoint, PathBuf::from("/")) };
        0
    })
}

/// Connect to an NFSv4 server at `host` and open the export root.
/// Negotiate NFSv4.2, falling back to NFSv4.1.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open(host: *const c_char, out: *mut *mut vfsi_fs) -> c_int {
    ffi_guard!(libc::EIO, {
        if host.is_null() || out.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: C callers must pass a NUL-terminated host string.
        // SAFETY: host was checked non-null and the caller supplies a live
        // NUL-terminated string for the full operation.
        let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
            return libc::EINVAL;
        };
        match NfsVecFs::connect(host)
            .map(|f| Box::new(f) as Box<dyn BindingBackend>)
            .map_err(|e| vf_code(&e))
        {
            Ok(fs) => {
                // SAFETY: out was checked for null; the caller provides writable storage
                // for this returned handle.
                unsafe { *out = make_fs(fs, PathBuf::from("/"), PathBuf::from("/")) };
                0
            }
            Err(code) => code,
        }
    })
}

/// Connect using an explicit supported NFS minor version (1 or 2).
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
        // SAFETY: host was checked non-null and the caller supplies a live
        // NUL-terminated string for the full operation.
        let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
            return libc::EINVAL;
        };
        match NfsVecFs::connect_minor(host, minorversion)
            .map(|f| Box::new(f) as Box<dyn BindingBackend>)
            .map_err(|e| vf_code(&e))
        {
            Ok(fs) => {
                // SAFETY: out was checked for null; the caller provides writable storage
                // for this returned handle.
                unsafe { *out = make_fs(fs, PathBuf::from("/"), PathBuf::from("/")) };
                0
            }
            Err(code) => code,
        }
    })
}

/// Connect to an NFSv4.1/4.2 server and treat `mountpoint` (a local kernel mount
/// path) as the vfsi root. Callers can then pass ordinary kernel paths.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_open_mount(
    host: *const c_char,
    mountpoint: *const c_char,
    out: *mut *mut vfsi_fs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: this forwards the same caller-owned C pointers and lengths;
        // the additional export root is a static NUL-terminated C string.
        unsafe { vfsi_nfs_open_mount_export(host, c"/".as_ptr(), mountpoint, out) }
    })
}

/// Discover a Linux NFS mount and inherit its version, port and security.
/// `path` must be an absolute directory. Ordinary paths beneath it are mapped
/// to the corresponding remote directory. No AUTH_SYS downgrade or kernel
/// fallback is performed; direct access does not share the kernel cache.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_from_mount(path: *const c_char, out: *mut *mut vfsi_fs) -> c_int {
    ffi_guard!(libc::EIO, {
        if out.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: out was checked for null; the caller provides writable storage for this
        // returned handle.
        unsafe { *out = std::ptr::null_mut() };
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        let Some(path) = (unsafe { cstr_path(path) }) else {
            return libc::EINVAL;
        };
        match vfsi_nfs::nfs::NfsClientBuilder::from_mount(&path)
            .and_then(|builder| builder.connect())
        {
            Ok(backend) => {
                // SAFETY: out was checked for null; the caller provides writable storage
                // for this returned handle.
                unsafe { *out = make_fs(Box::new(backend), path, PathBuf::from("/")) };
                0
            }
            Err(error) => vf_code(&error),
        }
    })
}

/// Discover a supported Linux NFS mount without connecting. Uses the same
/// security/version/covering-mount checks as nfs_from_mount. Callback strings
/// preserve path bytes and are borrowed only during one successful invocation.
/// Errors invoke no callback. Other platforms return ENOTSUP.
///
/// # Safety
/// path must be a live NUL-terminated absolute directory path. The callback
/// and userdata must remain valid; copy any returned strings before returning.
#[no_mangle]
pub unsafe extern "C" fn vfsi_nfs_mount_info(
    path: *const c_char,
    cb: vfsi_nfs_mount_cb,
    userdata: *mut c_void,
) -> c_int {
    ffi_guard!(libc::EIO, {
        let Some(cb) = cb else { return libc::EINVAL };
        // SAFETY: forwards the caller-owned NUL-terminated input contract.
        let Some(path) = (unsafe { cstr_path(path) }) else {
            return libc::EINVAL;
        };
        #[cfg(target_os = "linux")]
        {
            let mount = match vfsi_nfs::mount::NfsMount::discover(&path) {
                Ok(mount) => mount,
                Err(error) => return vf_code(&error),
            };
            let (Ok(host), Ok(export), Ok(point)) = (
                CString::new(mount.host()),
                CString::new(mount.export_root().as_os_str().as_bytes()),
                CString::new(mount.mount_point().as_os_str().as_bytes()),
            ) else {
                return libc::EINVAL;
            };
            // SAFETY: all strings remain live through the supplied callback.
            unsafe { cb(host.as_ptr(), export.as_ptr(), point.as_ptr(), userdata) };
            0
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, cb, userdata);
            libc::ENOTSUP
        }
    })
}

/// Connect to an NFSv4.1/4.2 server, mapping a local kernel `mountpoint` to the
/// server-side `export_root` beneath the NFSv4 pseudo-root.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
        // SAFETY: host was checked non-null and the caller supplies a live
        // NUL-terminated string for the full operation.
        let Ok(host) = unsafe { CStr::from_ptr(host) }.to_str() else {
            return libc::EINVAL;
        };
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        let (Some(export_root), Some(mountpoint)) = (unsafe { cstr_path(export_root) }, unsafe {
            cstr_path(mountpoint)
        }) else {
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
            .map(|f| Box::new(f) as Box<dyn BindingBackend>)
            .map_err(|e| vf_code(&e))
        {
            Ok(fs) => {
                // SAFETY: out was checked for null; the caller provides writable storage
                // for this returned handle.
                unsafe { *out = make_fs(fs, mountpoint, export_root) };
                0
            }
            Err(code) => code,
        }
    })
}

/// Connect to an SMB2/3 share. `server` may omit port 445; empty username and
/// password strings request guest access. Paths are rooted at the share root.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(server) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(share) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(username) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(password) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(domain) },
        ) else {
            return libc::EINVAL;
        };
        match SmbVecFs::connect(&server, &share, &username, &password, &domain) {
            Ok(backend) => {
                // SAFETY: out was checked for null; the caller provides writable storage
                // for this returned handle.
                unsafe {
                    *out = make_fs(
                        Box::new(backend) as Box<dyn BindingBackend>,
                        PathBuf::from("/"),
                        PathBuf::from("/"),
                    )
                };
                0
            }
            Err(error) => vf_code(&error),
        }
    })
}

/// Connect to an SMB2/3 share and map a kernel-visible `mountpoint` onto
/// `share_root` within that share. Both paths must be absolute and may not
/// contain parent-directory components.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(server) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(share) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(username) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(password) },
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            unsafe { cstr_utf8(domain) },
        ) else {
            return libc::EINVAL;
        };
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        let (Some(share_root), Some(mountpoint)) = (unsafe { cstr_path(share_root) }, unsafe {
            cstr_path(mountpoint)
        }) else {
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
                // SAFETY: out was checked for null; the caller provides writable storage
                // for this returned handle.
                unsafe {
                    *out = make_fs(
                        Box::new(backend) as Box<dyn BindingBackend>,
                        mountpoint,
                        share_root,
                    )
                };
                0
            }
            Err(error) => vf_code(&error),
        }
    })
}

/// Destroy a filesystem handle returned by one of the `vfsi_*_open*`
/// functions.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_free(fs: *mut vfsi_fs) {
    ffi_guard!((), {
        if !fs.is_null() {
            if std::env::var("VNFS_STATS").as_deref() == Ok("1") {
                let (n, ops, bytes, max) = vfsi_nfs::compound::compound_stats();
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
                let (calls, us) = vfsi_nfs::compound::rpc_stats();
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
