//! C scalar, vector, and streaming I/O.
use super::*;

pub(super) fn validate_c_read_results(
    reads: &[ReadOp],
    results: &[vfsi_sync::ReadOutput],
) -> Result<(), std::io::Error> {
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

pub(super) fn validate_c_write_results(
    writes: &[WriteOp<&FileOperand, &[u8]>],
    results: &[vfsi_sync::WriteOutput],
) -> Result<(), std::io::Error> {
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

fn pread_impl(
    fs: &vfsi_fs,
    fd: i32,
    buffer: &mut [u8],
    offset: u64,
) -> Result<usize, std::io::Error> {
    let file = lock_or_io(&fs.files)?
        .get(&fd)
        .map(|file| file.file.clone())
        .ok_or_else(|| std::io::Error::from_raw_os_error((ERR_EBADF) as i32))?;
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

fn pwrite_impl(fs: &vfsi_fs, fd: i32, data: &[u8], offset: u64) -> Result<usize, std::io::Error> {
    let file = lock_or_io(&fs.files)?
        .get(&fd)
        .cloned()
        .ok_or_else(|| std::io::Error::from_raw_os_error((ERR_EBADF) as i32))?;
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
                    vfsi_result::from_cause(
                        &std::io::Error::from_raw_os_error((ERR_EBADF) as i32),
                        Some(index)
                    ),
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
        let results = match vfsi_sync::internal::collect_vec_result(
            backend.vread_impl(&reads),
            reads.len(),
        ) {
            Ok(results) => results,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_indexed(error), true)
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
                        vfsi_result::from_error(vfsi_core::internal::errors::transport(
                            vfsi_core::TransportKind::Other,
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
                    vfsi_result::from_cause(
                        &std::io::Error::from_raw_os_error((ERR_EBADF) as i32),
                        Some(index)
                    ),
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
        let results = match vfsi_sync::internal::collect_vec_result(
            backend.vwrite_impl(&writes),
            writes.len(),
        ) {
            Ok(results) => results,
            Err(error) => {
                fail_batch!(item_results, vfsi_result::from_indexed(error), true)
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

/// # Safety
/// A non-null path must point to a live NUL-terminated C string.
unsafe fn copy_callback_path(path: *const c_char) -> Option<CString> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the caller supplies a readable, terminated string.
    Some(unsafe { CStr::from_ptr(path) }.to_owned())
}

fn callback_file_operand(fs: &vfsi_fs, path: &CStr) -> Result<FileOperand, std::io::Error> {
    let path = Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
    vpath_for(fs, path).map(|path| FileOperand::from_os_path(&path))
}

struct CallbackStreamFiles<'a> {
    fs: &'a vfsi_fs,
    opened: Vec<Option<FileOperand>>,
    close_error: Option<vfsi_core::IndexedError>,
}

impl CallbackStreamFiles<'_> {
    fn read(
        &mut self,
        indices: &[usize],
        reads: &mut [ReadOp],
    ) -> Result<Vec<vfsi_sync::ReadOutput>, vfsi_core::IndexedError> {
        use vfsi_core::{internal::collect_vec_result, IndexedError};
        use vfsi_sync::{OpenFlags, OpenOp};

        let fs = self.fs;
        let mut backend = lock_or_io(&fs.fs).map_err(IndexedError::unattributed)?;
        let pending: Vec<_> = indices
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, index)| self.opened[index].is_none())
            .collect();
        let mut failure = None;
        if !pending.is_empty() {
            let requests: Vec<_> = pending
                .iter()
                .map(|&(slot, _)| {
                    let path = reads[slot].file.path().expect("stream path input");
                    OpenOp::for_positioned_io(path, OpenFlags::READ)
                })
                .collect();
            let outcomes = backend
                .vopen_impl(&requests)
                .map_err(IndexedError::unattributed)?;
            let mut owners: std::collections::HashSet<_> = self
                .opened
                .iter()
                .flatten()
                .filter_map(FileOperand::fd)
                .collect();
            if outcomes.len() > pending.len() {
                for file in outcomes.into_iter().filter_map(Result::ok) {
                    if file.fd().is_some_and(|fd| fd >= 0 && owners.insert(fd)) {
                        let _ = backend.close_impl(&file);
                    }
                }
                return Err(IndexedError::unattributed(invalid_backend_reply(
                    "too many stream OPEN outcomes",
                )));
            }
            let outcomes = vfsi_core::internal::vec_result_slots(Ok(outcomes), pending.len());
            for ((slot, index), result) in pending.into_iter().zip(outcomes) {
                match result {
                    Ok(file) if file.fd().is_some_and(|fd| fd >= 0 && owners.insert(fd)) => {
                        self.opened[index] = Some(file);
                    }
                    Ok(_) => {
                        failure.get_or_insert_with(|| {
                            (slot, invalid_backend_reply("invalid stream OPEN handle"))
                        });
                    }
                    Err(error) => {
                        failure.get_or_insert_with(|| (slot, error.into_cause()));
                    }
                }
            }
        }
        // Earlier READ errors take precedence over a later input's OPEN error.
        let attempted = failure.as_ref().map_or(reads.len(), |(slot, _)| *slot);
        let reads = &mut reads[..attempted];
        for (&index, read) in indices.iter().zip(reads.iter_mut()) {
            read.file = self.opened[index].clone().expect("admitted stream handle");
        }
        let values = if reads.is_empty() {
            Vec::new()
        } else {
            collect_vec_result(backend.vread_impl(reads), reads.len())?
        };
        if let Some((slot, cause)) = failure {
            return Err(IndexedError::at(slot, cause));
        }
        // EOF handles are finished even when an earlier callback stops
        // delivery. Release this wave before the next one admits new files.
        let mut finished: Vec<_> = indices
            .iter()
            .zip(&values)
            .filter_map(|(&index, result)| result.eof.then_some(index))
            .collect();
        finished.sort_unstable();
        self.close_files(backend.as_mut(), finished.into_iter());
        Ok(values)
    }

    fn close_files(
        &mut self,
        backend: &mut dyn BindingBackend,
        indices: impl Iterator<Item = usize>,
    ) {
        let (indices, files): (Vec<_>, Vec<_>) = indices
            .filter_map(|index| self.opened[index].take().map(|file| (index, file)))
            .unzip();
        if files.is_empty() {
            return;
        }
        if let Err(error) =
            vfsi_core::internal::collect_vec_result(backend.vclose_owned_impl(files), indices.len())
        {
            self.record_close_error(error.map_index(|index| indices[index]));
        }
    }

    fn record_close_error(&mut self, error: vfsi_core::IndexedError) {
        if self
            .close_error
            .as_ref()
            .is_none_or(|earlier| error.index() < earlier.index())
        {
            self.close_error = Some(error);
        }
    }

    fn close(&mut self) -> Result<(), vfsi_core::IndexedError> {
        let fs = self.fs;
        if self.opened.iter().any(Option::is_some) {
            match lock_or_io(&fs.fs) {
                Ok(mut backend) => self.close_files(backend.as_mut(), 0..self.opened.len()),
                Err(cause) => self.record_close_error(vfsi_core::IndexedError::unattributed(cause)),
            }
        }
        self.close_error.take().map_or(Ok(()), Err)
    }
}

impl Drop for CallbackStreamFiles<'_> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Stream several paths in bounded vectorized chunks. Returning `false` from
/// `cb` cancels successfully. Callbacks provide backpressure between bounded
/// read waves and run outside the backend lock. They may query this filesystem.
/// Each file is held open across its read waves. The stream processes bounded
/// groups in input order and may interleave paths within a group.
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
            let Some(cpath) = (unsafe { copy_callback_path(*path) }) else {
                return vfsi_result::invalid(index, "null path");
            };
            let file = match callback_file_operand(fs, &cpath) {
                Ok(file) => file,
                Err(error) => return vfsi_result::from_cause(&error, Some(index)),
            };
            cpaths.push(cpath);
            files.push(file);
        }
        const MAX_OPEN_FILES: usize = 32;
        // Bound retained handles by the number of requests in one read wave.
        let cohort_size = MAX_OPEN_FILES.min(memory_limit.div_ceil(chunk_size));
        let mut completed = 0;
        for (cohort, files) in files.chunks(cohort_size).enumerate() {
            let base = cohort * cohort_size;
            let mut handles = CallbackStreamFiles {
                fs,
                opened: (0..files.len()).map(|_| None).collect(),
                close_error: None,
            };
            let mut stopped = false;
            let result = vfsi_sync::backend::helpers::stream_read_chunks_indexed(
                files,
                |_| chunk_size,
                memory_limit,
                |indices, reads| handles.read(indices, reads),
                |index, result| {
                    let index = base + index;
                    completed += usize::from(result.eof);
                    // SAFETY: callback borrows this owned result only during the
                    // invocation. The read wave has released the backend lock.
                    let more = unsafe {
                        cb(
                            cpaths[index].as_ptr(),
                            index,
                            result.offset,
                            result.data.as_ptr(),
                            result.data.len(),
                            result.eof,
                            userdata,
                        )
                    };
                    stopped = !more;
                    more
                },
            );
            let close = handles.close();
            if stopped && result.is_ok() {
                break;
            }
            if let Err(error) = result.and(close) {
                return vfsi_result::from_indexed(error.map_index(|index| base + index));
            }
        }
        vfsi_result::success(completed)
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
        let mut cpaths = Vec::with_capacity(count);
        for i in 0..count {
            // SAFETY: `paths` points to `count` NUL-terminated strings.
            let p = unsafe { *paths.add(i) };
            // SAFETY: non-null C strings are NUL-terminated and readable throughout this
            // call, as required by the exported ABI.
            let Some(path) = (unsafe { copy_callback_path(p) }) else {
                return libc::EINVAL;
            };
            cpaths.push(path);
        }
        let res = (|| {
            let files = cpaths
                .iter()
                .map(|path| callback_file_operand(fs, path))
                .collect::<Result<Vec<_>, _>>()?;
            let datas = lock_or_io(&fs.fs)?
                .vread_all_with_options_impl(
                    &files,
                    vfsi_sync::ReadAllOptions::new().with_max_total_bytes(max_bytes),
                )
                .and_then(|outcomes| {
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), files.len())
                        .map_err(vfsi_core::IndexedError::into_cause)
                })?;
            for (path, data) in cpaths.iter().zip(datas.iter()) {
                // SAFETY: the supplied callback runs on the calling thread
                // while the path snapshot and result data are alive.
                if !unsafe { cb(path.as_ptr(), data.as_ptr(), data.len(), userdata) } {
                    break;
                }
            }
            Ok::<_, std::io::Error>(())
        })();
        res.map_or_else(|error| vf_code(&error), |()| 0)
    })
}
