//! C metadata queries, updates, and attribute projection.
use super::*;

pub(super) fn attrs_from_metadata(metadata: &vfsi_sync::Attrs) -> vfsi_attrs {
    fn time(value: Option<std::time::SystemTime>) -> (i64, u32) {
        value
            .and_then(|v| match v.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => i64::try_from(d.as_secs())
                    .ok()
                    .map(|s| (s, d.subsec_nanos())),
                Err(e) => {
                    let d = e.duration();
                    i64::try_from(d.as_secs()).ok().and_then(|s| {
                        if d.subsec_nanos() == 0 {
                            s.checked_neg().map(|s| (s, 0))
                        } else {
                            s.checked_neg()
                                .and_then(|s| s.checked_sub(1))
                                .map(|s| (s, 1_000_000_000 - d.subsec_nanos()))
                        }
                    })
                }
            })
            .unwrap_or_default()
    }
    let (atime_sec, atime_nsec) = time(metadata.accessed());
    let (mtime_sec, mtime_nsec) = time(metadata.modified());
    let (ctime_sec, ctime_nsec) = time(metadata.changed());
    vfsi_attrs {
        struct_size: std::mem::size_of::<vfsi_attrs>() as u32,
        abi_version: VFSI_ABI_VERSION,
        ftype: vfsi_sync::file_type_to_nfs(&metadata.file_type()),
        mode: metadata.mode().unwrap_or_default() & 0o7777,
        size: metadata.len().unwrap_or_default(),
        nlink: metadata.nlink().unwrap_or_default(),
        fileid: metadata.file_id().unwrap_or_default(),
        blocks: metadata.blocks().unwrap_or_default(),
        uid: metadata.uid().unwrap_or_default(),
        gid: metadata.gid().unwrap_or_default(),
        atime_sec,
        atime_nsec,
        mtime_sec,
        mtime_nsec,
        ctime_sec,
        ctime_nsec,
    }
}

fn stat_impl(fs: &vfsi_fs, path: &std::path::Path) -> Result<VfAttrs, VfError> {
    let vpath = vpath_for(fs, path)?;
    let mut attrs = VfAttrs {
        file: FileOperand::from_os_path(&vpath),
        masks: mask(),
        ..VfAttrs::default()
    };
    lock_or_io(&fs.fs)?.vgetattrs_impl(std::slice::from_mut(&mut attrs))?;
    if !attrs.returned.contains(mask()) {
        return Err(VfError::errno(0, libc::ENOTSUP as u32));
    }
    Ok(attrs)
}

/// Stat `path`, following a final symlink.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_stat(
    fs: *mut vfsi_fs,
    path: *const c_char,
    out: *mut vfsi_attrs,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), Some(path), false) = (
            unsafe { fs.as_ref() },
            unsafe { cstr_path(path) },
            out.is_null(),
        ) else {
            return libc::EINVAL;
        };
        match stat_impl(fs, &path) {
            Ok(a) => {
                // SAFETY: out is non-null, aligned and exclusively writable.
                // Raw initialization permits uninitialized C output storage.
                unsafe { out.write(vfsi_attrs::from_vf(&a)) };
                0
            }
            Err(e) => vf_code(&e),
        }
    })
}

/// Stat a path array in one backend vector call.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_statv(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_stat_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let results = match unsafe { result_array(item_results, count) } {
            Ok(results) => results,
            Err(error) => return error,
        };
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(ops) = (unsafe { ops.as_mut() }) else {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        };
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let ops = unsafe { std::slice::from_raw_parts_mut(ops, count) };
        let mut attrs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(path) = (unsafe { cstr_path(op.path) }) else {
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
                file: FileOperand::from_os_path(&path),
                masks: mask(),
                ..VfAttrs::default()
            });
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        if let Err(error) = backend.vgetattrs_impl(&mut attrs) {
            fail_batch!(results, vfsi_result::from_error(error), true);
        }
        let mut first_failure = None;
        for (index, (op, attrs)) in ops.iter_mut().zip(&attrs).enumerate() {
            if attrs.returned.contains(mask()) {
                op.attrs = vfsi_attrs::from_vf(attrs);
                results[index] = vfsi_result::item_success(index);
            } else {
                // Clear stale caller data when this item cannot satisfy the C
                // struct's all-fields contract.
                op.attrs = vfsi_attrs::from_vf(&VfAttrs::default());
                let failure = vfsi_result::base(
                    index,
                    VFSI_ERROR_UNSUPPORTED,
                    libc::ENOTSUP as u32,
                    "backend omitted requested stat attributes",
                );
                results[index] = failure;
                first_failure.get_or_insert(failure);
            }
        }
        first_failure.unwrap_or_else(|| vfsi_result::success(count))
    })
}

/// Set selected attributes for a path array in one backend vector call.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_setattrv(
    fs: *mut vfsi_fs,
    ops: *const vfsi_setattr_op,
    count: usize,
    item_results: *mut vfsi_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
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
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let ops = unsafe { std::slice::from_raw_parts(ops, count) };
        let mut attrs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(path) = (unsafe { cstr_path(op.path) }) else {
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
            let Some(masks) = AttrMask::from_bits(op.mask).filter(|masks| {
                masks
                    .difference(AttrMask::MODE | AttrMask::SIZE | AttrMask::ATIME | AttrMask::MTIME)
                    .is_empty()
            }) else {
                fail_batch!(
                    results,
                    vfsi_result::invalid(
                        index,
                        "attribute mask has no value field in vfsi_setattr_op"
                    ),
                    false
                );
            };
            attrs.push(VfAttrs {
                file: FileOperand::from_os_path(&path),
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
        match backend.vsetattrs_raw_impl(&attrs) {
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
