//! C API for vfsi, the vectorized filesystem API.
//!
//! The C-facing symbols are intentionally thin: C code works with an opaque
//! `vfsi_fs` handle and byte-oriented `const char *` paths, while all
//! filesystem logic stays in the native backend crates.
//!
//! # C pointer contract
//! Non-null handles must originate from this API and stay live for the whole
//! call. Strings must be readable and NUL-terminated. Arrays and buffers must
//! be aligned, valid for their stated lengths, and fit within one allocation.
//! Writable arrays/buffers must be exclusively accessible and must not overlap
//! other borrowed inputs. Optional null pointers are accepted only where the
//! individual function documents them. Callbacks may use borrowed data only
//! during their invocation and must not free the handle or invalidate inputs.
#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString, OsString};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard};

use vfsi_local::LocalBackend;
use vfsi_nfs::nfs::NfsVecFs;
use vfsi_smb::SmbVecFs;
#[cfg(test)]
use vfsi_sync::VF_ERR_UNSUPPORTED;
use vfsi_sync::{
    AttrMask, ExtentPair, FileOperand, ReadOp, VfAttrs, VfError, WriteOp, ERR_EBADF, ERR_NOENT,
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
const C_INDEX_UNKNOWN: usize = usize::MAX;
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
/// Native atomic no-replace rename is implemented (filesystem support may vary).
pub const VFSI_CAP_RENAME_NOREPLACE: u64 = 1 << 6;
const _: () =
    assert!(VFSI_CAP_RENAME_NOREPLACE == vfsi_sync::Capabilities::RENAME_NOREPLACE.bits());
/// Native atomic name exchange is implemented (filesystem support may vary).
pub const VFSI_CAP_RENAME_EXCHANGE: u64 = 1 << 7;
const _: () = assert!(VFSI_CAP_RENAME_EXCHANGE == vfsi_sync::Capabilities::RENAME_EXCHANGE.bits());
/// Genuine retained directory handles are implemented.
pub const VFSI_CAP_DIRECTORY_HANDLES: u64 = 1 << 8;
const _: () =
    assert!(VFSI_CAP_DIRECTORY_HANDLES == vfsi_sync::Capabilities::DIRECTORY_HANDLES.bits());
/// Contents removal can be anchored to a retained directory handle.
pub const VFSI_CAP_ANCHORED_REMOVAL: u64 = 1 << 9;
const _: () =
    assert!(VFSI_CAP_ANCHORED_REMOVAL == vfsi_sync::Capabilities::ANCHORED_REMOVAL.bits());

const _: () = assert!(VFSI_CAP_POSIX_METADATA == vfsi_sync::VF_CAP_POSIX_METADATA);
const _: () = assert!(VFSI_CAP_SYMLINKS == vfsi_sync::VF_CAP_SYMLINKS);
const _: () = assert!(VFSI_CAP_HARDLINKS == vfsi_sync::VF_CAP_HARDLINKS);
const _: () = assert!(VFSI_CAP_NON_UTF8_PATHS == vfsi_sync::VF_CAP_NON_UTF8_PATHS);
const _: () = assert!(VFSI_CAP_LSTAT == vfsi_sync::VF_CAP_LSTAT);

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

mod identity;
pub use identity::*;
mod directory;
pub use directory::*;
mod filesystem;
mod listing;
pub use filesystem::{
    vfsi_abi_version, vfsi_capabilities, vfsi_dummy_open, vfsi_dummy_open_mount, vfsi_free,
    vfsi_nfs_from_mount, vfsi_nfs_minorversion, vfsi_nfs_mount_info, vfsi_nfs_open,
    vfsi_nfs_open_minor, vfsi_nfs_open_mount, vfsi_nfs_open_mount_export, vfsi_smb_dialect,
    vfsi_smb_open, vfsi_smb_open_mount,
};
mod handles;
pub use handles::{vfsi_close, vfsi_closev, vfsi_open, vfsi_openv};
mod metadata;
pub use metadata::{vfsi_setattrv, vfsi_stat, vfsi_statv};
mod io;
pub use io::{
    vfsi_pread, vfsi_preadv, vfsi_pwrite, vfsi_pwritev, vfsi_read_paths,
    vfsi_read_paths_with_limit, vfsi_read_streamv,
};
mod namespace;
#[cfg(test)]
use filesystem::make_fs;
#[cfg(test)]
use handles::confirmed_close_prefix;
#[cfg(test)]
use listing::collect_bounded_entries;
pub use listing::{
    vfsi_listdir, vfsi_listdir_with_limits, vfsi_listdirs, vfsi_listdirs_with_ids, vfsi_listdirv,
    vfsi_walk,
};
pub use namespace::{
    vfsi_copy, vfsi_copyv, vfsi_mkdir, vfsi_mkdirv, vfsi_remove, vfsi_removev, vfsi_rename,
    vfsi_renamev,
};

/// Opaque filesystem handle owned by C.
pub struct vfsi_fs {
    fs: Mutex<Box<dyn BindingBackend>>,
    files: Mutex<std::collections::HashMap<i32, CFile>>,
    next_fd: AtomicI32,
    /// Kernel mountpoint used by application-visible paths.
    mountpoint: PathBuf,
    /// Backend path corresponding to `mountpoint`.
    backend_root: PathBuf,
    /// Process-local comparison scope, stable across reconnects and mount views.
    namespace_id: u64,
    cursor_owner: std::sync::Arc<()>,
}

#[derive(Clone)]
struct CFile {
    file: FileOperand,
    append: bool,
}

impl From<FileOperand> for CFile {
    fn from(file: FileOperand) -> Self {
        Self {
            file,
            append: false,
        }
    }
}

/// Attributes returned by [`vfsi_stat`] and passed to listdir callbacks.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_attrs {
    /// Fixed ABI-v3 layout size; do not extend this structure within ABI v3.
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
/// `index` is the completed count on success, the failing operation index on
/// an attributable error, or `VFSI_INDEX_UNKNOWN`. Vector calls also populate
/// a caller-owned result per element;
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
    /// Only MODE, SIZE, ATIME and MTIME have value fields in this ABI.
    /// Other mask bits are rejected before any operation is submitted.
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
    pub to_eof: u8,
}

impl vfsi_attrs {
    fn from_vf(a: &VfAttrs) -> vfsi_attrs {
        vfsi_attrs {
            struct_size: std::mem::size_of::<vfsi_attrs>() as u32,
            abi_version: VFSI_ABI_VERSION,
            ftype: vfsi_sync::file_type_to_nfs(&a.ftype),
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
        Self::base(
            error.index().unwrap_or(C_INDEX_UNKNOWN),
            if error.is_transport() {
                VFSI_ERROR_TRANSPORT
            } else if error.kind() == std::io::ErrorKind::Unsupported {
                VFSI_ERROR_UNSUPPORTED
            } else {
                VFSI_ERROR_FILESYSTEM
            },
            error.raw_code(),
            &error.to_string(),
        )
    }
}

/// Initialize output-only C storage before borrowing it as Rust values.
///
/// # Safety
/// output must be non-null, aligned, and exclusively writable for count T
/// slots in one allocation (whose size fits isize). The returned borrow must
/// not outlive that storage. Old slot contents need not be initialized.
#[inline]
unsafe fn initialized_output_slice<'a, T: Copy>(
    output: *mut T,
    count: usize,
    mut initialize: impl FnMut(usize) -> T,
) -> &'a mut [T] {
    // SAFETY: the caller grants count aligned writable slots. Raw writes do
    // not read or drop old C output bytes. Every slot holds a valid T before
    // we create references; the caller bounds the returned borrow's lifetime.
    unsafe {
        for index in 0..count {
            output.add(index).write(initialize(index));
        }
        std::slice::from_raw_parts_mut(output, count)
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
    // SAFETY: the C contract grants count aligned writable output slots,
    // exclusively borrowed for this call; initialization reads no old bytes.
    let results = unsafe {
        initialized_output_slice(results, count, |index| {
            vfsi_result::base(index, VFSI_ERROR_NOT_ATTEMPTED, 0, "not attempted")
        })
    };
    Ok(results)
}

fn parse_c_bool(value: u8) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn complete_results(results: &mut [vfsi_result]) {
    for (index, result) in results.iter_mut().enumerate() {
        *result = vfsi_result::item_success(index);
    }
}

fn invalid_backend_reply(message: impl Into<std::borrow::Cow<'static, str>>) -> VfError {
    VfError::transport_with_kind(None, vfsi_sync::TransportKind::InvalidReply, message)
}

fn invalid_backend_result_count(operation: &str, expected: usize, actual: usize) -> VfError {
    invalid_backend_reply(format!(
        "{operation} returned {actual} results for {expected} requests",
    ))
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
    if failure.index != C_INDEX_UNKNOWN {
        if let Some(result) = results.get_mut(failure.index) {
            *result = failure;
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

/// Aggregate limits for complete shallow vector listings. Zero is a zero
/// budget; SIZE_MAX explicitly opts out. MODE is always requested.
#[repr(C)]
pub struct vfsi_listing_options {
    pub max_entries: usize,
    pub max_path_bytes: usize,
    pub attributes: u32,
}
/// Complete=1, stopped=2, not completed=0. Empty directories are explicitly
/// complete with zero delivered entries. Failure leaves every result incomplete.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct vfsi_listing_result {
    pub completion: u32,
    pub delivered: usize,
}
pub type vfsi_indexed_listdir_cb = Option<
    unsafe extern "C" fn(
        usize,
        *const c_char,
        *const c_char,
        *const vfsi_attrs,
        *mut c_void,
    ) -> bool,
>;

/// Version of the additive listing callback with scoped object identity.
pub const VFSI_LISTDIR_IDS_VERSION: u32 = 1;
pub type vfsi_indexed_listdir_ids_cb = Option<
    unsafe extern "C" fn(
        usize,
        *const c_char,
        *const c_char,
        *const vfsi_attrs,
        *const vfsi_object_id,
        *mut c_void,
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
    e.to_errno()
}

fn lock_or_io<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, VfError> {
    mutex
        .lock()
        .map_err(|_| VfError::errno(0, libc::EIO as u32))
}

/// # Safety
/// A non-null path must point to a live NUL-terminated C string.
unsafe fn cstr_path(path: *const c_char) -> Option<PathBuf> {
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

/// # Safety
/// A non-null value must point to a live NUL-terminated C string.
unsafe fn cstr_utf8(value: *const c_char) -> Option<String> {
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

fn vpath_for(fs: &vfsi_fs, path: &std::path::Path) -> Result<PathBuf, VfError> {
    path_for(fs, path).ok_or_else(|| VfError::errno(0, ERR_NOENT))
}

/// Version of the additive validated Linux NFS mount discovery callback.
pub const VFSI_MOUNT_INFO_VERSION: u32 = 1;
/// Borrowed server endpoint, export root and local mount point, respectively.
pub type vfsi_nfs_mount_cb =
    Option<unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, *mut c_void)>;

/// Resource policy for an incremental tree traversal. Zero is a zero limit,
/// not unlimited. Set SIZE_MAX explicitly to opt out of a limit.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_walk_options {
    pub max_entries: usize,
    pub max_path_bytes: usize,
    pub max_depth: usize,
    pub attributes: u32,
    pub sort_by_name: u8,
}
/// Event kinds: 0=enter directory, 1=entry, 2=leave directory.
/// Return 0=continue, 1=prune directory, 2=stop. Arguments are borrowed.
pub type vfsi_walk_cb =
    Option<unsafe extern "C" fn(*const c_char, u32, usize, *const vfsi_attrs, *mut c_void) -> u32>;

// Protocol inspection belongs to the bindings, outside the native operation contracts.
trait BindingBackend: vfsi_sync::backend::VectorBackend {
    fn identity_namespace(&self) -> Option<std::borrow::Cow<'_, str>> {
        None
    }
    #[cfg(test)]
    fn reconnect_for_test(&mut self) -> Result<(), VfError> {
        Err(VfError::unsupported(0))
    }
    fn nfs_minorversion(&self) -> Option<u32> {
        None
    }
    fn smb_dialect(&self) -> Option<u16> {
        None
    }
}
impl BindingBackend for LocalBackend {
    fn identity_namespace(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(std::borrow::Cow::Borrowed("posix"))
    }
}
impl BindingBackend for NfsVecFs {
    fn identity_namespace(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(format!("nfs:{}", NfsVecFs::identity_namespace(self)).into())
    }
    #[cfg(test)]
    fn reconnect_for_test(&mut self) -> Result<(), VfError> {
        self.reconnect()
    }
    fn nfs_minorversion(&self) -> Option<u32> {
        Some(self.minorversion())
    }
}
impl BindingBackend for SmbVecFs {
    fn smb_dialect(&self) -> Option<u16> {
        Some(self.dialect() as u16)
    }
}

#[cfg(test)]
#[allow(clippy::undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn)]
mod error_tests;

#[cfg(test)]
#[allow(clippy::undocumented_unsafe_blocks, unsafe_op_in_unsafe_fn)]
mod tests;
