//! Additive versioned object identity; ABI-v3 attributes remain frozen.
use super::*;
use std::collections::HashMap;
use std::sync::{atomic::AtomicU64, OnceLock};

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;

/// Layout version of vfsi_object_id (independent of the library ABI version).
pub const VFSI_OBJECT_ID_VERSION: u32 = 1;
/// Query the final path component itself rather than following a symlink.
pub const VFSI_OBJECT_ID_NOFOLLOW: u32 = 1;

/// Complete object identity, not an open-instance ID or a durable global ID.
/// Initialize struct_size=sizeof(vfsi_object_id), version=VFSI_OBJECT_ID_VERSION.
/// Compare namespace_id, both fs_id words and fileid; zero values in fs_id or
/// fileid are valid. namespace_id is an opaque nonzero token scoped to this
/// process and loaded library instance; never persist it across reloads.
/// Local mount views share one namespace; NFS views share a configured endpoint
/// regardless of export root/minor version. Different endpoint spellings are
/// conservatively distinct. IDs may be reused after deletion/server changes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct vfsi_object_id {
    pub struct_size: u32,
    pub version: u32,
    pub namespace_id: u64,
    /// Full 128-bit backend filesystem ID, high word first. Never truncate it.
    pub fs_id: [u64; 2],
    pub fileid: u64,
}

impl vfsi_object_id {
    fn empty() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>() as u32,
            version: VFSI_OBJECT_ID_VERSION,
            namespace_id: 0,
            fs_id: [0; 2],
            fileid: 0,
        }
    }
    pub(super) fn from_attrs(namespace_id: u64, attrs: &VfAttrs) -> Result<Self, VfError> {
        if !attrs.returned.contains(AttrMask::identity()) {
            return Err(VfError::unsupported(0));
        }
        Ok(Self {
            namespace_id,
            fs_id: [(attrs.fs_id >> 64) as u64, attrs.fs_id as u64],
            fileid: attrs.fileid,
            ..Self::empty()
        })
    }
}

/// Exactly one target: path != NULL and fd == -1, or path == NULL and fd >= 0.
/// flags is zero or VFSI_OBJECT_ID_NOFOLLOW (paths only). out points to an
/// initialized version/size header. attrs is optional output-only ABI-v3 storage;
/// when non-NULL, attributes and identity come from the same GETATTR response.
/// Output pointers must be distinct, exclusive and disjoint from all inputs.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct vfsi_object_id_op {
    pub path: *const c_char,
    pub fd: c_int,
    pub flags: u32,
    pub out: *mut vfsi_object_id,
    pub attrs: *mut vfsi_attrs,
}

pub(super) fn namespace_id(key: Option<&str>) -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    static NAMESPACES: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    let allocate = || loop {
        let current = NEXT.load(Ordering::Relaxed);
        let next = current
            .checked_add(1)
            .expect("object identity namespace exhausted");
        if NEXT
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break current;
        }
    };
    match key {
        Some(key) => *NAMESPACES
            .get_or_init(Default::default)
            .lock()
            .expect("identity namespace mutex poisoned")
            .entry(key.to_owned())
            .or_insert_with(allocate),
        // Unknown backend namespaces must never alias merely by numeric IDs.
        None => allocate(),
    }
}

// Read only the fixed eight-byte input header until its declared layout fits.
unsafe fn validate_output(out: *mut vfsi_object_id) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: the contract grants an aligned readable eight-byte header;
    // no reference to the larger object is created before checking its size.
    unsafe {
        std::ptr::addr_of!((*out).struct_size).read()
            >= std::mem::size_of::<vfsi_object_id>() as u32
            && std::ptr::addr_of!((*out).version).read() == VFSI_OBJECT_ID_VERSION
    }
}

/// Query path or retained-descriptor identities in backend vector batches.
/// Returns ABI-v3 results; output IDs are usable only for successful items.
/// Preflight validates every target/header before any backend request. Valid
/// output payloads are cleared before dispatch, including on backend failure.
/// Missing identity components report Unsupported, never fabricated zeros.
/// Follow/no-follow policy changes partition the batch into contiguous runs.
/// Planning is bounded by the default directory entry/path-byte limits.
///
/// # Safety
/// Follow the module C pointer contract. Each out supplies a readable 8-byte
/// header and exclusive storage for its declared size, at least the v1 layout.
/// Older/smaller headers are rejected without reading/writing past the header.
/// No pointers/outputs may alias. count zero permits null arrays.
#[no_mangle]
pub unsafe extern "C" fn vfsi_get_object_idv(
    fs: *mut vfsi_fs,
    ops: *const vfsi_object_id_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        // SAFETY: the caller keeps a non-null handle live throughout this call.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        if count > vfsi_sync::DEFAULT_DIRECTORY_MAX_ENTRIES {
            return vfsi_result::base(
                0,
                VFSI_ERROR_FILESYSTEM,
                libc::EFBIG as u32,
                "identity request count exceeds planning limit",
            );
        }
        // SAFETY: item_results grants count exclusive writable result slots.
        let results = match unsafe { result_array(item_results, count) } {
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
        // SAFETY: ops grants count initialized input slots, disjoint from outputs.
        let ops = unsafe { std::slice::from_raw_parts(ops, count) };
        let table = match lock_or_io(&fs.files) {
            Ok(table) => table,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        let mut attrs = Vec::with_capacity(count);
        let mut targets = Vec::with_capacity(count);
        let mut bytes = 0usize;
        for (index, op) in ops.iter().enumerate() {
            // SAFETY: each output grants an initialized readable size/version header.
            if !unsafe { validate_output(op.out) } || op.flags & !VFSI_OBJECT_ID_NOFOLLOW != 0 {
                fail_batch!(
                    results,
                    vfsi_result::invalid(index, "invalid identity output header or flags"),
                    false
                );
            }
            let file = if !op.path.is_null() && op.fd == -1 {
                // SAFETY: non-null paths are NUL-terminated and readable.
                let path = unsafe { CStr::from_ptr(op.path) };
                bytes = match bytes.checked_add(path.to_bytes().len()) {
                    Some(bytes) if bytes <= vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES => bytes,
                    _ => fail_batch!(
                        results,
                        vfsi_result::base(
                            index,
                            VFSI_ERROR_FILESYSTEM,
                            libc::EFBIG as u32,
                            "identity paths exceed planning limit"
                        ),
                        false
                    ),
                };
                let path = Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
                let path = match vpath_for(fs, path) {
                    Ok(path) => path,
                    Err(error) => fail_batch!(
                        results,
                        vfsi_result::from_error(error.with_index(index)),
                        false
                    ),
                };
                bytes = match path
                    .as_os_str()
                    .len()
                    .checked_mul(2)
                    .and_then(|n| bytes.checked_add(n))
                {
                    Some(bytes) if bytes <= vfsi_sync::DEFAULT_DIRECTORY_MAX_PATH_BYTES => bytes,
                    _ => fail_batch!(
                        results,
                        vfsi_result::base(
                            index,
                            VFSI_ERROR_FILESYSTEM,
                            libc::EFBIG as u32,
                            "mapped identity paths exceed planning limit"
                        ),
                        false
                    ),
                };
                FileOperand::from_os_path(&path)
            } else if op.path.is_null() && op.fd >= 0 && op.flags == 0 {
                match table.get(&op.fd) {
                    Some(file) => file.file.clone(),
                    None => fail_batch!(
                        results,
                        vfsi_result::from_error(VfError::errno(index, ERR_EBADF)),
                        false
                    ),
                }
            } else {
                fail_batch!(
                    results,
                    vfsi_result::invalid(index, "specify one path or retained descriptor"),
                    false
                );
            };
            targets.push(file.clone());
            attrs.push(VfAttrs {
                file,
                masks: AttrMask::identity()
                    | if op.attrs.is_null() {
                        AttrMask::empty()
                    } else {
                        mask()
                    },
                ..VfAttrs::default()
            });
        }
        drop(table);
        for op in ops {
            // SAFETY: preflight validated full exclusive v1 output storage.
            unsafe {
                op.out.write(vfsi_object_id::empty());
                if !op.attrs.is_null() {
                    op.attrs.write(vfsi_attrs::from_vf(&VfAttrs::default()));
                }
            }
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        let mut start = 0;
        while start < count {
            let mut end = start + 1;
            while end < count && ops[end].flags == ops[start].flags {
                end += 1;
            }
            let result = if ops[start].flags == VFSI_OBJECT_ID_NOFOLLOW {
                backend.vgetattrs_nofollow_impl(&mut attrs[start..end])
            } else {
                backend.vgetattrs_impl(&mut attrs[start..end])
            };
            if let Err(error) = vfsi_sync::internal::collect_vec_result(result, end - start) {
                fail_batch!(
                    results,
                    vfsi_result::from_error(error.map_index(|index| start + index)),
                    true
                );
            }
            start = end;
        }
        if attrs
            .iter()
            .zip(&targets)
            .any(|(attrs, target)| attrs.file != *target)
        {
            fail_batch!(
                results,
                vfsi_result::from_error(invalid_backend_reply("identity reply target mismatch")),
                true
            );
        }
        let mut first_failure = None;
        for (index, (op, attrs)) in ops.iter().zip(&attrs).enumerate() {
            let identity = vfsi_object_id::from_attrs(fs.namespace_id, attrs);
            if identity.is_err() || (!op.attrs.is_null() && !attrs.returned.contains(mask())) {
                let failure = vfsi_result::base(
                    index,
                    VFSI_ERROR_UNSUPPORTED,
                    libc::ENOTSUP as u32,
                    "backend omitted requested identity/stat attributes",
                );
                results[index] = failure;
                first_failure.get_or_insert(failure);
            } else {
                // SAFETY: all output storage was preflighted and is exclusive.
                unsafe {
                    op.out.write(identity.expect("checked identity fields"));
                    if !op.attrs.is_null() {
                        op.attrs.write(vfsi_attrs::from_vf(attrs));
                    }
                }
                results[index] = vfsi_result::item_success(index);
            }
        }
        first_failure.unwrap_or_else(|| vfsi_result::success(count))
    })
}

/// Query a path identity, with optional VFSI_OBJECT_ID_NOFOLLOW.
/// # Safety
/// Same output-header, storage and pointer contract as vfsi_get_object_idv.
#[no_mangle]
pub unsafe extern "C" fn vfsi_get_object_id(
    fs: *mut vfsi_fs,
    path: *const c_char,
    flags: u32,
    out: *mut vfsi_object_id,
) -> vfsi_result {
    let op = vfsi_object_id_op {
        path,
        fd: -1,
        flags,
        out,
        attrs: std::ptr::null_mut(),
    };
    let mut item = vfsi_result::panic();
    // SAFETY: the wrapper supplies one live operation/result and forwards the caller's pointer contract.
    unsafe { vfsi_get_object_idv(fs, &op, 1, &mut item) }
}

/// Query the object pinned by a retained C descriptor, independent of its name.
/// # Safety
/// Same output-header, storage and pointer contract as vfsi_get_object_idv.
#[no_mangle]
pub unsafe extern "C" fn vfsi_fget_object_id(
    fs: *mut vfsi_fs,
    fd: c_int,
    out: *mut vfsi_object_id,
) -> vfsi_result {
    let op = vfsi_object_id_op {
        path: std::ptr::null(),
        fd,
        flags: 0,
        out,
        attrs: std::ptr::null_mut(),
    };
    let mut item = vfsi_result::panic();
    // SAFETY: the wrapper supplies one live operation/result and forwards the caller's pointer contract.
    unsafe { vfsi_get_object_idv(fs, &op, 1, &mut item) }
}
