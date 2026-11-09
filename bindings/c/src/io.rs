//! C scalar, vector, and streaming I/O.
use super::*;

fn validate_c_read_results(
    reads: &[ReadOp],
    results: &[vfsi_sync::ReadResult],
) -> Result<(), VfError> {
    if results.len() != reads.len() {
        return Err(invalid_backend_result_count(
            "vread_impl",
            reads.len(),
            results.len(),
        ));
    }
    for (index, (read, result)) in reads.iter().zip(results).enumerate() {
        if result.file != read.file {
            return Err(invalid_backend_reply(format!(
                "vread_impl result {index} has a different file handle",
            )));
        }
        if result.data.len() > read.length {
            return Err(invalid_backend_reply(format!(
                "vread_impl result {index} returned {} bytes for a {}-byte read",
                result.data.len(),
                read.length,
            )));
        }
        if let vfsi_sync::VfOffset::At(expected) = read.offset {
            if result.offset != expected {
                return Err(invalid_backend_reply(format!(
                    "vread_impl result {index} started at offset {}, expected {expected}",
                    result.offset,
                )));
            }
        }
        if read.length != 0 && result.data.is_empty() && !result.eof {
            return Err(invalid_backend_reply(format!(
                "vread_impl result {index} made no progress without EOF",
            )));
        }
    }
    Ok(())
}

fn validate_c_write_results(
    writes: &[WriteOp<&FileOperand, &[u8]>],
    results: &[vfsi_sync::WriteResult],
) -> Result<(), VfError> {
    if results.len() != writes.len() {
        return Err(invalid_backend_result_count(
            "vwrite_impl",
            writes.len(),
            results.len(),
        ));
    }
    for (index, (write, result)) in writes.iter().zip(results).enumerate() {
        if result.file != *write.file() {
            return Err(invalid_backend_reply(format!(
                "vwrite_impl result {index} has a different file handle",
            )));
        }
        if result.bytes_written > write.data().len() {
            return Err(invalid_backend_reply(format!(
                "vwrite_impl result {index} reported {} bytes written for a {}-byte write",
                result.bytes_written,
                write.data().len(),
            )));
        }
        if let vfsi_sync::VfOffset::At(expected) = write.offset() {
            if result.offset != expected {
                return Err(invalid_backend_reply(format!(
                    "vwrite_impl result {index} started at offset {}, expected {expected}",
                    result.offset,
                )));
            }
        }
    }
    Ok(())
}

fn pread_impl(fs: &vfsi_fs, fd: i32, buffer: &mut [u8], offset: u64) -> Result<usize, VfError> {
    let file = lock_or_io(&fs.files)?
        .get(&fd)
        .map(|file| file.file.clone())
        .ok_or_else(|| VfError::errno(0, ERR_EBADF))?;
    let request = ReadOp::at(file, offset, buffer.len());
    let result = lock_or_io(&fs.fs)?.read_into_impl(&request, buffer)?;
    Ok(result.bytes_read)
}

/// Read up to `len` bytes at `offset` into `buf`; writes the actual count to
/// `*got` when non-NULL.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
/// The scalar read buffer must contain initialized bytes before this call,
/// because it is borrowed directly as a mutable Rust byte slice.
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
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), false) = (unsafe { fs.as_ref() }, buf.is_null()) else {
            return libc::EINVAL;
        };
        // SAFETY: the scalar C read contract grants len initialized, writable
        // bytes, exclusively borrowed for this call. This keeps the existing
        // direct-to-caller read without a temporary allocation or second copy.
        let buffer = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), len) };
        match pread_impl(fs, fd, buffer, offset) {
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

fn pwrite_impl(fs: &vfsi_fs, fd: i32, data: &[u8], offset: u64) -> Result<usize, VfError> {
    let file = lock_or_io(&fs.files)?
        .get(&fd)
        .cloned()
        .ok_or_else(|| VfError::errno(0, ERR_EBADF))?;
    let offset = if file.append {
        vfsi_sync::VfOffset::End
    } else {
        vfsi_sync::VfOffset::At(offset)
    };
    let request = WriteOp::new(&file.file, offset, data);
    let result = lock_or_io(&fs.fs)?.write_impl(request)?;
    validate_c_write_results(
        std::slice::from_ref(&request),
        std::slice::from_ref(&result),
    )?;
    Ok(result.bytes_written)
}

/// Write `len` bytes at `offset`; writes the actual count to `*wrote`.
/// Handles opened with `O_APPEND` write at EOF regardless of `offset`.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), false) = (unsafe { fs.as_ref() }, buf.is_null()) else {
            return libc::EINVAL;
        };
        // SAFETY: the non-null C input buffer is readable for len bytes and
        // remains unchanged for this call. The Rust helper borrows it directly.
        let data = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len) };
        match pwrite_impl(fs, fd, data, offset) {
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

/// Positioned vector read using caller-owned buffers.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_preadv(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_pread_op,
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
        let item_results = match unsafe { result_array(item_results, count) } {
            Ok(results) => results,
            Err(error) => return error,
        };
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(ops) = (unsafe { ops.as_mut() }) else {
            fail_batch!(
                item_results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        };
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let ops = unsafe { std::slice::from_raw_parts_mut(ops, count) };
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
                    vfsi_result::from_error(VfError::errno(index, ERR_EBADF)),
                    false
                );
            };
            reads.push(ReadOp::at(file.file, op.offset, op.len));
        }
        drop(table);
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), false)
            }
        };
        let results = match backend.vread_impl(&reads).and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        }) {
            Ok(results) => results,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), true)
            }
        };
        if let Err(error) = validate_c_read_results(&reads, &results) {
            fail_batch!(item_results, vfsi_result::from_error(error), true);
        }
        for (op, result) in ops.iter_mut().zip(results) {
            if !result.data.is_empty() {
                if result.data.len() > op.len {
                    fail_batch!(
                        item_results,
                        vfsi_result::from_error(VfError::transport(
                            None,
                            "read result exceeded its output buffer"
                        )),
                        true
                    );
                }
                // SAFETY: the caller grants op.len writable bytes and non-null
                // was checked. The owned result is initialized and disjoint
                // from caller storage. Raw copying also permits uninitialized
                // C destination storage without creating a Rust slice over it.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        result.data.as_ptr(),
                        op.buf.cast::<u8>(),
                        result.data.len(),
                    )
                };
            }
            op.got = result.data.len();
        }
        complete_results(item_results);
        vfsi_result::success(count)
    })
}

/// Positioned vector write using caller-owned buffers.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_pwritev(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_pwrite_op,
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
        let item_results = match unsafe { result_array(item_results, count) } {
            Ok(results) => results,
            Err(error) => return error,
        };
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(ops) = (unsafe { ops.as_mut() }) else {
            fail_batch!(
                item_results,
                vfsi_result::invalid(0, "null operation array"),
                false
            );
        };
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let ops = unsafe { std::slice::from_raw_parts_mut(ops, count) };
        let table = match lock_or_io(&fs.files) {
            Ok(table) => table,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), false)
            }
        };
        let mut files = Vec::with_capacity(count);
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
                    vfsi_result::from_error(VfError::errno(index, ERR_EBADF)),
                    false
                );
            };
            files.push(file);
        }
        drop(table);
        // All handles and buffers are validated before dispatch. The C caller
        // owns payload storage until this synchronous call returns; no copy is
        // needed, and a zero-length buffer need not have a non-null pointer.
        let writes: Vec<WriteOp<&FileOperand, &[u8]>> = ops
            .iter()
            .zip(&files)
            .map(|(op, file)| {
                let data = if op.len == 0 {
                    &[][..]
                } else {
                    // SAFETY: the C contract supplies count aligned, live array elements;
                    // mutable arrays are exclusively borrowed for this call.
                    unsafe { std::slice::from_raw_parts(op.buf.cast::<u8>(), op.len) }
                };
                // Backends honor O_APPEND even for positioned writes. Express
                // that policy in the request so validation accepts its actual
                // EOF position without relaxing ordinary offset checks.
                let offset = if file.append {
                    vfsi_sync::VfOffset::End
                } else {
                    vfsi_sync::VfOffset::At(op.offset)
                };
                WriteOp::new(&file.file, offset, data)
            })
            .collect();
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(backend) => backend,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), false)
            }
        };
        let results = match backend.vwrite_impl(&writes).and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        }) {
            Ok(results) => results,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_error(error), true)
            }
        };
        if let Err(error) = validate_c_write_results(&writes, &results) {
            fail_batch!(item_results, vfsi_result::from_error(error), true);
        }
        for (op, result) in ops.iter_mut().zip(results) {
            op.wrote = result.bytes_written;
        }
        complete_results(item_results);
        vfsi_result::success(count)
    })
}

/// Stream several paths in bounded vectorized chunks. Returning `false` from
/// `cb` cancels successfully. Callbacks provide backpressure between bounded
/// read waves and run outside the backend lock; they may query this filesystem.
/// The handle and input arrays must remain live and unchanged during callbacks.
/// On success, `index` counts files whose EOF was delivered to the callback,
/// including an EOF callback that cancels the stream.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
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
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
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
        // SAFETY: the C contract supplies count aligned, live array elements; mutable
        // arrays are exclusively borrowed for this call.
        let paths = unsafe { std::slice::from_raw_parts(paths, count) };
        let mut cpaths = Vec::with_capacity(count);
        let mut files = Vec::with_capacity(count);
        for (index, path) in paths.iter().enumerate() {
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(kernel_path) = (unsafe { cstr_path(*path) }) else {
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
            files.push(FileOperand::from_os_path(&vpath));
        }
        let mut completed = 0;
        let result = vfsi_sync::backend::helpers::stream_read_chunks(
            &files,
            |_| chunk_size,
            memory_limit,
            |reads| {
                lock_or_io(&fs.fs)?.vread_impl(reads).and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
            },
            |index, result| {
                completed += usize::from(result.eof);
                // SAFETY: callback borrows this owned result only during the
                // invocation; the backend read wave has already released its lock.
                unsafe {
                    cb(
                        cpaths[index].as_ptr(),
                        index,
                        result.offset,
                        result.data.as_ptr(),
                        result.data.len(),
                        result.eof,
                        userdata,
                    )
                }
            },
        );
        match result {
            Ok(()) => vfsi_result::success(completed),
            Err(error) => vfsi_result::from_error(error),
        }
    })
}

/// Read the full contents of several files in one vectorized batch, calling
/// `cb` for each file after its data has been fetched.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_read_paths(
    fs: *mut vfsi_fs,
    paths: *const *const c_char,
    count: usize,
    cb: vfsi_read_paths_cb,
    userdata: *mut c_void,
) -> c_int {
    // SAFETY: this forwards the same live pointers, lengths and callback obligations
    // required by the exported C ABI.
    unsafe {
        vfsi_read_paths_with_limit(
            fs,
            paths,
            count,
            vfsi_sync::DEFAULT_READ_ALLV_MAX_TOTAL_BYTES,
            cb,
            userdata,
        )
    }
}

/// Whole-file vector with an explicit aggregate payload budget. Oversized
/// results are an error, never successful truncation. Zero is a zero budget.
///
/// # Safety
/// All pointers, buffers and callbacks must satisfy the module-level C
/// pointer contract for their documented lengths and the full call duration.
#[no_mangle]
pub unsafe extern "C" fn vfsi_read_paths_with_limit(
    fs: *mut vfsi_fs,
    paths: *const *const c_char,
    count: usize,
    max_bytes: usize,
    cb: vfsi_read_paths_cb,
    userdata: *mut c_void,
) -> c_int {
    ffi_guard!(libc::EIO, {
        // SAFETY: non-null inputs point to live, aligned ABI objects for this call;
        // mutable outputs are exclusive.
        let (Some(fs), false, false) = (unsafe { fs.as_ref() }, paths.is_null(), cb.is_none())
        else {
            return libc::EINVAL;
        };
        let Some(cb) = cb else {
            return libc::EINVAL;
        };
        let mut kernel_paths = Vec::with_capacity(count);
        for i in 0..count {
            // SAFETY: `paths` points to `count` NUL-terminated strings.
            let p = unsafe { *paths.add(i) };
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(path) = (unsafe { cstr_path(p) }) else {
                return libc::EINVAL;
            };
            kernel_paths.push(path);
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let vpaths: Result<Vec<PathBuf>, VfError> =
                kernel_paths.iter().map(|p| vpath_for(fs, p)).collect();
            let vpaths = vpaths?;
            let files: Vec<FileOperand> = vpaths
                .iter()
                .map(|p| FileOperand::from_os_path(p))
                .collect();
            let datas = lock_or_io(&fs.fs)?
                .vread_all_with_options_impl(
                    &files,
                    vfsi_sync::ReadAllOptions::new().with_max_total_bytes(max_bytes),
                )
                .and_then(|outcomes| {
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), files.len())
                })?;
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
