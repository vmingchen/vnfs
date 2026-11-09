//! C namespace mutations and file copying.
use super::*;

/// Create a directory (`create_parents != 0` behaves like `mkdir -p`).
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_mkdir(
    fs: *mut vfsi_fs,
    path: *const c_char,
    mode: u32,
    create_parents: c_int,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), Some(path)) = (unsafe { fs.as_ref() }, unsafe { cstr_path(path) }) else {
            return libc::EINVAL;
        };
        let result: Result<(), VfError> = (|| {
            let path = vpath_for(fs, &path)?;
            let mut f = lock_or_io(&fs.fs)?;
            if create_parents != 0 {
                f.ensure_dir_impl(&path, mode)
            } else {
                f.mkdir_raw_impl(&path, mode)
            }
        })();
        match result {
            Ok(()) => 0,
            Err(e) => vf_code(&e),
        }
    })
}

/// Remove a path (directories must be empty).
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_remove(fs: *mut vfsi_fs, path: *const c_char) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), Some(path)) = (unsafe { fs.as_ref() }, unsafe { cstr_path(path) }) else {
            return libc::EINVAL;
        };
        let result: Result<(), VfError> = (|| {
            let path = vpath_for(fs, &path)?;
            lock_or_io(&fs.fs)?.unlink_impl(&path)
        })();
        match result {
            Ok(()) => 0,
            Err(e) => vf_code(&e),
        }
    })
}

/// Rename `oldpath` to `newpath`.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_rename(
    fs: *mut vfsi_fs,
    oldpath: *const c_char,
    newpath: *const c_char,
) -> c_int {
    ffi_guard!(libc::EIO, {
        let (Some(fs), Some(old), Some(new)) =
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
            // mutable outputs are exclusive.
            (unsafe { fs.as_ref() }, unsafe { cstr_path(oldpath) }, unsafe { cstr_path(newpath) })
        else {
            return libc::EINVAL;
        };
        let result: Result<(), VfError> = (|| {
            let old = vpath_for(fs, &old)?;
            let new = vpath_for(fs, &new)?;
            lock_or_io(&fs.fs)?.rename_impl(&old, &new)
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
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return libc::EINVAL;
        };
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        let (Some(src), Some(dst)) = (unsafe { cstr_path(src) }, unsafe { cstr_path(dst) }) else {
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
        match lock_or_io(&fs.fs).and_then(|mut backend| {
            vfsi_sync::internal::collect_vec_result(
                backend.vcopy_impl(&[pair], vfsi_sync::CopyOptions::new()),
                1,
            )
            .map(|_| ())
        }) {
            Ok(()) => 0,
            Err(error) => vf_code(&error),
        }
    })
}

/// Create a directory array in one backend vector call.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_mkdirv(
    fs: *mut vfsi_fs,
    ops: *const vfsi_mkdir_op,
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
        let mut dirs = Vec::with_capacity(count);
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
            dirs.push(VfAttrs {
                file: FileOperand::from_os_path(&path),
                masks: AttrMask::MODE,
                mode: op.mode,
                ..VfAttrs::default()
            });
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        unit_results("mkdir", results, backend.vmkdir_impl(&dirs), |_| {})
    })
}

/// Remove a path array in one backend vector call.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_removev(
    fs: *mut vfsi_fs,
    paths: *const *const c_char,
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
        if paths.is_null() {
            fail_batch!(results, vfsi_result::invalid(0, "null path array"), false);
        }
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let paths = unsafe { std::slice::from_raw_parts(paths, count) };
        let mut files = Vec::with_capacity(count);
        for (index, path) in paths.iter().enumerate() {
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(path) = (unsafe { cstr_path(*path) }) else {
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
            files.push(FileOperand::from_os_path(&path));
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        unit_results("remove", results, backend.vremove_impl(&files), |_| {})
    })
}

/// Rename a path-pair array in one backend vector call.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_renamev(
    fs: *mut vfsi_fs,
    ops: *const vfsi_rename_op,
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
        let mut pairs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let (Some(oldpath), Some(newpath)) = (unsafe { cstr_path(op.oldpath) }, unsafe {
                cstr_path(op.newpath)
            }) else {
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
                FileOperand::from_os_path(&oldpath),
                FileOperand::from_os_path(&newpath),
            ));
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        unit_results("rename", results, backend.vrename_impl(&pairs), |_| {})
    })
}

/// Copy an extent-pair array in one backend vector call.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_copyv(
    fs: *mut vfsi_fs,
    ops: *const vfsi_copy_op,
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
        let mut pairs = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            let Some(to_eof) = parse_c_bool(op.to_eof) else {
                fail_batch!(
                    results,
                    vfsi_result::invalid(index, "to_eof must be 0 or 1"),
                    false
                );
            };
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let (Some(src), Some(dst)) =
                (unsafe { cstr_path(op.src) }, unsafe { cstr_path(op.dst) })
            else {
                fail_batch!(results, vfsi_result::invalid(index, "null path"), false);
            };
            let (Some(src), Some(dst)) = (path_for(fs, &src), path_for(fs, &dst)) else {
                fail_batch!(
                    results,
                    vfsi_result::from_error(VfError::errno(index, ERR_NOENT)),
                    false
                );
            };
            pairs.push(ExtentPair::from_os_paths(
                &src,
                op.src_offset,
                &dst,
                op.dst_offset,
                (!to_eof).then_some(op.length),
            ));
        }
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        unit_results(
            "copy",
            results,
            backend.vcopy_impl(&pairs, vfsi_sync::CopyOptions::new()),
            |_| {},
        )
    })
}
