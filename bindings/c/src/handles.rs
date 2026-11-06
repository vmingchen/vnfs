//! C file-handle registration, opening, and closing.
use super::*;

fn validate_c_open_results(opened: &[FileOperand], expected: usize) -> Result<(), VfError> {
    if opened.len() != expected {
        return Err(invalid_backend_result_count(
            "vopen_raw_impl",
            expected,
            opened.len(),
        ));
    }
    for (index, file) in opened.iter().enumerate() {
        let Some(fd) = file.fd() else {
            return Err(invalid_backend_reply(format!(
                "vopen_raw_impl result {index} is not an open file handle",
            )));
        };
        if fd < 0 {
            return Err(invalid_backend_reply(format!(
                "vopen_raw_impl result {index} has a negative file handle",
            )));
        }
    }
    if opened.len() > 1 {
        let mut seen = std::collections::HashSet::with_capacity(opened.len());
        for (index, file) in opened.iter().enumerate() {
            let fd = file
                .fd()
                .expect("open results were checked to be file handles");
            if !seen.insert(fd) {
                return Err(invalid_backend_reply(format!(
                    "vopen_raw_impl returned the same open handle more than once (result {index})",
                )));
            }
        }
    }
    Ok(())
}

fn unique_open_handles_for_cleanup(opened: &[FileOperand]) -> Vec<FileOperand> {
    let mut seen = std::collections::HashSet::with_capacity(opened.len());
    opened
        .iter()
        .filter(|file| file.fd().is_some_and(|fd| fd >= 0 && seen.insert(fd)))
        .cloned()
        .collect()
}

pub(super) fn confirmed_close_prefix(error: &VfError, count: usize) -> usize {
    if error.is_transport() {
        0
    } else {
        error.index().unwrap_or(0).min(count)
    }
}

fn insert_c_file(
    fs: &vfsi_fs,
    files: &mut std::collections::HashMap<i32, CFile>,
    file: CFile,
) -> Result<i32, VfError> {
    for _ in 0..i32::MAX {
        let fd = loop {
            let current = fs.next_fd.load(Ordering::Relaxed);
            let next = current.checked_add(1).unwrap_or(1);
            if fs
                .next_fd
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break current;
            }
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = files.entry(fd) {
            entry.insert(file);
            return Ok(fd);
        }
    }
    Err(VfError::errno(0, libc::EMFILE as u32))
}

fn open_impl(
    fs: &vfsi_fs,
    path: &std::path::Path,
    flags: c_int,
    mode: u32,
) -> Result<i32, VfError> {
    let vpath = vpath_for(fs, path)?;
    let file = lock_or_io(&fs.fs)?.open_raw_impl(&vpath, flags, mode)?;
    let mut files = lock_or_io(&fs.files)?;
    insert_c_file(
        fs,
        &mut files,
        CFile {
            file,
            append: flags & libc::O_APPEND != 0,
        },
    )
}

/// Open a file and return a vfsi descriptor (`>= 0`), or a negative errno.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_open(
    fs: *mut vfsi_fs,
    path: *const c_char,
    flags: c_int,
    mode: u32,
) -> c_int {
    ffi_guard!(-libc::EIO, {
        // SAFETY: non-null C strings are NUL-terminated and readable throughout this call,
        // as required by the exported ABI.
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), Some(path)) = (unsafe { fs.as_ref() }, unsafe { cstr_path(path) }) else {
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
        .ok_or_else(|| VfError::errno(0, ERR_EBADF))?;
    lock_or_io(&fs.fs)?.close_impl(&file.file)
}

/// Close a descriptor opened by [`vfsi_open`].
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_close(fs: *mut vfsi_fs, fd: c_int) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return libc::EINVAL;
        };
        match close_impl(fs, fd) {
            Ok(()) => 0,
            Err(e) => vf_code(&e),
        }
    })
}

/// Open `count` files in one backend vector call. ABI-v2 functions remain
/// available; this and the other `*v` entry points use the uniform ABI-v3
/// overall and per-element result contract.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_openv(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_open_op,
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
        let mut paths = Vec::with_capacity(count);
        let mut flags = Vec::with_capacity(count);
        let mut modes = Vec::with_capacity(count);
        for (index, op) in ops.iter().enumerate() {
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(path) = (unsafe { cstr_path(op.path) }) else {
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
            if let Err(error) = backend.vopen_preflight_impl(&refs, &flags, &modes) {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, false);
                return failure;
            }
            match backend.vopen_raw_impl(&refs, &flags, &modes) {
                Ok(opened) => opened,
                Err(error) => {
                    let failure = vfsi_result::from_error(error);
                    fail_results(results, failure, true);
                    return failure;
                }
            }
        };
        if let Err(error) = validate_c_open_results(&opened, count) {
            // Do not register ambiguous or non-handle results. Close each
            // distinct handle at most once during best-effort cleanup.
            let cleanup = unique_open_handles_for_cleanup(&opened);
            if !cleanup.is_empty() {
                if let Ok(mut backend) = fs.fs.lock() {
                    for file in &cleanup {
                        let _ = backend.close_impl(file);
                    }
                }
            }
            fail_batch!(results, vfsi_result::from_error(error), true);
        }
        let mut registered = Vec::with_capacity(count);
        let registration = (|| -> Result<(), VfError> {
            let mut files = lock_or_io(&fs.files)?;
            for (index, (op, file)) in ops.iter_mut().zip(&opened).enumerate() {
                let fd = insert_c_file(
                    fs,
                    &mut files,
                    CFile {
                        file: file.clone(),
                        append: op.flags & libc::O_APPEND != 0,
                    },
                )
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
                let _ = backend.vclose_impl(&opened);
            }
            fail_batch!(results, vfsi_result::from_error(error), true);
        }
        complete_results(results);
        vfsi_result::success(count)
    })
}

/// Close a descriptor array in one backend vector call.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_closev(
    fs: *mut vfsi_fs,
    fds: *const c_int,
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
        if fds.is_null() {
            fail_batch!(
                results,
                vfsi_result::invalid(0, "null descriptor array"),
                false
            );
        }
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let fds = unsafe { std::slice::from_raw_parts(fds, count) };
        let files = {
            let table = match lock_or_io(&fs.files) {
                Ok(table) => table,
                Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
            };
            let mut files = Vec::with_capacity(count);
            for (index, fd) in fds.iter().enumerate() {
                let Some(file) = table.get(fd).cloned() else {
                    fail_batch!(
                        results,
                        vfsi_result::from_error(VfError::errno(index, ERR_EBADF)),
                        false
                    );
                };
                files.push(file.file);
            }
            files
        };
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
        };
        match backend.vclose_impl(&files) {
            Ok(()) => {
                if let Ok(mut table) = fs.files.lock() {
                    for fd in fds {
                        table.remove(fd);
                    }
                }
                complete_results(results);
                vfsi_result::success(count)
            }
            Err(error) => {
                let closed_prefix = confirmed_close_prefix(&error, fds.len());
                if closed_prefix > 0 {
                    if let Ok(mut table) = fs.files.lock() {
                        for fd in fds.iter().take(closed_prefix) {
                            table.remove(fd);
                        }
                    }
                }
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, true);
                failure
            }
        }
    })
}
