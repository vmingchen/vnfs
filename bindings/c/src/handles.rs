//! C file-handle registration, opening, and closing.
use super::*;

fn validate_c_open_results(opened: &[FileOperand], expected: usize) -> Result<(), std::io::Error> {
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

fn insert_c_file(
    fs: &vfsi_fs,
    files: &mut std::collections::HashMap<i32, CFile>,
    file: CFile,
) -> Result<i32, (std::io::Error, CFile)> {
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
    Err((std::io::Error::from_raw_os_error(libc::EMFILE), file))
}

fn open_impl(
    fs: &vfsi_fs,
    path: &std::path::Path,
    flags: c_int,
    mode: u32,
) -> Result<i32, std::io::Error> {
    let vpath = vpath_for(fs, path)?;
    let file = CFile {
        file: lock_or_io(&fs.fs)?.open_raw_impl(&vpath, flags, mode)?,
        append: flags & libc::O_APPEND != 0,
    };
    let (error, unregistered) = match lock_or_io(&fs.files) {
        Ok(mut files) => match insert_c_file(fs, &mut files, file) {
            Ok(fd) => return Ok(fd),
            Err(failure) => failure,
        },
        Err(error) => (error, file),
    };
    if let Ok(mut backend) = lock_or_io(&fs.fs) {
        let _ = backend.close_impl(&unregistered.file);
    }
    Err(error)
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

fn close_impl(fs: &vfsi_fs, fd: i32) -> Result<(), std::io::Error> {
    let file = lock_or_io(&fs.files)?
        .remove(&fd)
        .ok_or_else(|| std::io::Error::from_raw_os_error((ERR_EBADF) as i32))?;
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
                Err(error) => {
                    fail_batch!(results, vfsi_result::from_cause(&error, Some(index)), false)
                }
            }
            flags.push(op.flags);
            modes.push(op.mode);
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let opened = {
            let mut backend = match lock_or_io(&fs.fs) {
                Ok(backend) => backend,
                Err(error) => fail_batch!(results, vfsi_result::from_error(error), false),
            };
            if let Err(error) = backend.vopen_preflight_impl(&refs, &flags, &modes) {
                let failure = vfsi_result::from_error(error);
                fail_results(results, failure, false);
                return failure;
            }
            match catch_submitted(results, move || {
                vfsi_sync::backend::helpers::open_raw_batch(&mut **backend, &refs, &flags, &modes)
            }) {
                Ok(Ok(opened)) => opened,
                Err(failure) => return failure,
                Ok(Err(error)) => {
                    let failure = vfsi_result::from_indexed(error);
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
        let registration = (|| -> Result<(), vfsi_core::IndexedError> {
            let mut files = lock_or_io(&fs.files).map_err(vfsi_core::IndexedError::unattributed)?;
            for (index, (op, file)) in ops.iter_mut().zip(&opened).enumerate() {
                let fd = insert_c_file(
                    fs,
                    &mut files,
                    CFile {
                        file: file.clone(),
                        append: op.flags & libc::O_APPEND != 0,
                    },
                )
                .map_err(|(error, _)| vfsi_core::IndexedError::at(index, error))?;
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
            fail_batch!(results, vfsi_result::from_indexed(error), true);
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
                        vfsi_result::from_cause(
                            &std::io::Error::from_raw_os_error(ERR_EBADF as i32),
                            Some(index),
                        ),
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
        let (_backend, outcomes) = match catch_submitted(results, move || {
            let outcomes = backend.vclose_impl(&files);
            (backend, outcomes)
        }) {
            Ok(completed) => completed,
            Err(failure) => return failure,
        };
        let mut table = fs.files.lock().ok();
        unit_results("close", results, outcomes, |index| {
            if let Some(table) = &mut table {
                table.remove(&fds[index]);
            }
        })
    })
}
