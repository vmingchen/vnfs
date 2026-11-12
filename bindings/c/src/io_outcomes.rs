//! Optional independent I/O outcomes; legacy ABI v3 entry points are unchanged.
use super::*;
use vfsi_sync::{FailureState, IoProgress, OpError};

fn rejection(error: VfError, indices: &[usize]) -> vfsi_result {
    let error = match error.index() {
        Some(local) => match indices.get(local) {
            Some(&index) => error.with_index(index),
            None => invalid_backend_reply("invalid vector rejection index"),
        },
        None => error,
    };
    vfsi_result::from_error(error)
}

fn failed(index: usize, error: OpError) -> vfsi_io_result {
    let state = match error.state() {
        FailureState::NotAttempted => VFSI_IO_NOT_ATTEMPTED,
        FailureState::Failed => VFSI_IO_FAILED,
        FailureState::Unknown => VFSI_IO_UNKNOWN,
    };
    vfsi_io_result {
        status: vfsi_result::from_error(error.into_cause().with_index(index)),
        failure_state: state,
        offset: 0,
        offset_known: 0,
        eof: 0,
    }
}
fn success(index: usize, offset: u64, eof: bool) -> vfsi_io_result {
    vfsi_io_result {
        status: vfsi_result::item_success(index),
        failure_state: VFSI_IO_SUCCESS,
        offset,
        offset_known: 1,
        eof: u8::from(eof),
    }
}

/// Independent positioned reads. A successful return means the batch was accepted:
/// inspect every item, including siblings after failures. A return error rejects
/// the batch before dispatch. got includes confirmed failed prefixes;
/// progress never grants replay permission. Never fall back after dispatch.
/// # Safety
/// Same pointer, lifetime and non-aliasing contract as the legacy vector API,
/// with count writable vfsi_io_result outputs. Read buffers may be uninitialized.
#[no_mangle]
pub unsafe extern "C" fn vfsi_preadv_ex(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_pread_op,
    count: usize,
    results: *mut vfsi_io_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        // SAFETY: the C caller supplies a live aligned filesystem pointer; null is handled below.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        if results.is_null() || ops.is_null() {
            return vfsi_result::invalid(0, "null vector array");
        }
        // SAFETY: the documented C pointer and lifetime contract holds for this synchronous call.
        let results = unsafe {
            initialized_output_slice(results, count, |index| {
                failed(
                    index,
                    OpError::not_attempted(VfError::errno(None, libc::ECANCELED as u32)),
                )
            })
        };
        // SAFETY: the non-null operation array has count live aligned exclusive elements by the C contract.
        let ops = unsafe { std::slice::from_raw_parts_mut(ops, count) };
        let mut invalid_payload = None;
        for (index, op) in ops.iter_mut().enumerate() {
            op.got = 0;
            if op.len > 0 && op.buf.is_null() && invalid_payload.is_none() {
                invalid_payload = Some(index);
            }
        }
        if let Some(index) = invalid_payload {
            return vfsi_result::invalid(index, "null payload");
        }
        let table = match lock_or_io(&fs.files) {
            Ok(t) => t,
            Err(e) => return vfsi_result::from_error(e),
        };
        let mut files = Vec::new();
        let mut indices = Vec::new();
        for (index, op) in ops.iter().enumerate() {
            if let Some(file) = table.get(&op.fd) {
                files.push(file.clone());
                indices.push(index);
            } else {
                results[index] = failed(
                    index,
                    OpError::not_attempted(VfError::errno(None, ERR_EBADF)),
                );
            }
        }
        drop(table);
        if files.is_empty() {
            return vfsi_result::success(count);
        }
        let requests: Vec<_> = files
            .iter()
            .zip(&indices)
            .map(|(file, &index)| {
                let op = &ops[index];
                ReadOp::at(file.file.clone(), op.offset, op.len)
            })
            .collect();
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(b) => b,
            Err(e) => return vfsi_result::from_error(e),
        };
        let requests_ref = &requests;
        // Move the guard into the unwind boundary so a backend panic poisons
        // its mutex before we translate the submitted inputs to Unknown.
        let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            backend.vread_impl(requests_ref)
        }));
        let outcomes = match dispatched {
            Ok(Ok(results)) => results,
            Ok(Err(error)) => return rejection(error, &indices),
            Err(_) => (0..requests.len())
                .map(|_| {
                    Err(OpError::unknown(
                        VfError::transport(None, "backend panicked after dispatch"),
                        None,
                    ))
                })
                .collect(),
        };
        let malformed = outcomes.len() != requests.len();
        let mut outcomes = outcomes.into_iter();
        for (local, index) in indices.iter().copied().enumerate() {
            let outcome = if malformed {
                Err(OpError::unknown(
                    invalid_backend_reply("invalid vector outcome count"),
                    None,
                ))
            } else {
                outcomes.next().expect("validated count").and_then(|value| {
                    super::io::validate_c_read_results(
                        &requests[local..local + 1],
                        std::slice::from_ref(&value),
                    )
                    .map_err(|cause| OpError::unknown(cause, None))?;
                    Ok(value)
                })
            };
            match outcome {
                Ok(value) => {
                    if !value.data.is_empty() {
                        // SAFETY: owned source bytes are initialized and disjoint; validated counts fit the caller-owned writable destination.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                value.data.as_ptr(),
                                ops[index].buf.cast(),
                                value.data.len(),
                            )
                        };
                    }
                    ops[index].got = value.data.len();
                    results[index] = success(index, value.offset, value.eof);
                }
                Err(error) => {
                    let mut offset = None;
                    if let Some(progress) = error.progress() {
                        let valid_state = error.state() != FailureState::NotAttempted;
                        match progress {
                            IoProgress::Read(read)
                                if valid_state
                                    && read.offset() == ops[index].offset
                                    && read.bytes_read() <= ops[index].len
                                    && read.data().is_some() =>
                            {
                                let bytes = read.data().expect("validated owned prefix");
                                if !bytes.is_empty() {
                                    // SAFETY: owned source bytes are initialized and disjoint; validated counts fit the caller-owned writable destination.
                                    unsafe {
                                        std::ptr::copy_nonoverlapping(
                                            bytes.as_ptr(),
                                            ops[index].buf.cast(),
                                            bytes.len(),
                                        )
                                    };
                                }
                                ops[index].got = bytes.len();
                                offset = Some(read.offset());
                            }
                            _ => {
                                results[index] = failed(
                                    index,
                                    OpError::unknown(
                                        invalid_backend_reply("invalid I/O failure progress"),
                                        None,
                                    ),
                                );
                                continue;
                            }
                        }
                    }
                    results[index] = failed(index, error);
                    if let Some(actual) = offset {
                        results[index].offset = actual;
                        results[index].offset_known = 1;
                    }
                }
            }
        }
        vfsi_result::success(count)
    })
}

/// Independent positioned writes. A successful return means the batch was accepted:
/// inspect every item, including siblings after failures. A return error rejects
/// the batch before dispatch. wrote includes confirmed failed prefixes;
/// progress never grants replay permission. Never fall back after dispatch.
/// # Safety
/// Same pointer, lifetime and non-aliasing contract as the legacy vector API,
/// with count writable vfsi_io_result outputs. Read buffers may be uninitialized.
#[no_mangle]
pub unsafe extern "C" fn vfsi_pwritev_ex(
    fs: *mut vfsi_fs,
    ops: *mut vfsi_pwrite_op,
    count: usize,
    results: *mut vfsi_io_result,
) -> vfsi_result {
    ffi_guard!(vfsi_result::panic(), {
        // SAFETY: the C caller supplies a live aligned filesystem pointer; null is handled below.
        let Some(fs) = (unsafe { fs.as_ref() }) else {
            return vfsi_result::invalid(0, "null filesystem");
        };
        if count == 0 {
            return vfsi_result::success(0);
        }
        if results.is_null() || ops.is_null() {
            return vfsi_result::invalid(0, "null vector array");
        }
        // SAFETY: the documented C pointer and lifetime contract holds for this synchronous call.
        let results = unsafe {
            initialized_output_slice(results, count, |index| {
                failed(
                    index,
                    OpError::not_attempted(VfError::errno(None, libc::ECANCELED as u32)),
                )
            })
        };
        // SAFETY: the non-null operation array has count live aligned exclusive elements by the C contract.
        let ops = unsafe { std::slice::from_raw_parts_mut(ops, count) };
        let mut invalid_payload = None;
        for (index, op) in ops.iter_mut().enumerate() {
            op.wrote = 0;
            if op.len > 0 && op.buf.is_null() && invalid_payload.is_none() {
                invalid_payload = Some(index);
            }
        }
        if let Some(index) = invalid_payload {
            return vfsi_result::invalid(index, "null payload");
        }
        let table = match lock_or_io(&fs.files) {
            Ok(t) => t,
            Err(e) => return vfsi_result::from_error(e),
        };
        let mut files = Vec::new();
        let mut indices = Vec::new();
        for (index, op) in ops.iter().enumerate() {
            if let Some(file) = table.get(&op.fd) {
                files.push(file.clone());
                indices.push(index);
            } else {
                results[index] = failed(
                    index,
                    OpError::not_attempted(VfError::errno(None, ERR_EBADF)),
                );
            }
        }
        drop(table);
        if files.is_empty() {
            return vfsi_result::success(count);
        }
        let requests: Vec<_> = files
            .iter()
            .zip(&indices)
            .map(|(file, &index)| {
                let op = &ops[index];
                let data = if op.len == 0 {
                    &[]
                } else {
                    // SAFETY: the caller retains len initialized readable payload bytes through this synchronous call.
                    unsafe { std::slice::from_raw_parts(op.buf.cast(), op.len) }
                };
                WriteOp::new(
                    &file.file,
                    if file.append {
                        vfsi_sync::VfOffset::End
                    } else {
                        vfsi_sync::VfOffset::At(op.offset)
                    },
                    data,
                )
            })
            .collect();
        let mut backend = match lock_or_io(&fs.fs) {
            Ok(b) => b,
            Err(e) => return vfsi_result::from_error(e),
        };
        let requests_ref = &requests;
        // Move the guard into the unwind boundary so a backend panic poisons
        // its mutex before we translate the submitted inputs to Unknown.
        let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            backend.vwrite_impl(requests_ref)
        }));
        let outcomes = match dispatched {
            Ok(Ok(results)) => results,
            Ok(Err(error)) => return rejection(error, &indices),
            Err(_) => (0..requests.len())
                .map(|_| {
                    Err(OpError::unknown(
                        VfError::transport(None, "backend panicked after dispatch"),
                        None,
                    ))
                })
                .collect(),
        };
        let malformed = outcomes.len() != requests.len();
        let mut outcomes = outcomes.into_iter();
        for (local, index) in indices.iter().copied().enumerate() {
            let outcome = if malformed {
                Err(OpError::unknown(
                    invalid_backend_reply("invalid vector outcome count"),
                    None,
                ))
            } else {
                outcomes.next().expect("validated count").and_then(|value| {
                    super::io::validate_c_write_results(
                        &requests[local..local + 1],
                        std::slice::from_ref(&value),
                    )
                    .map_err(|cause| OpError::unknown(cause, None))?;
                    if value
                        .offset
                        .checked_add(value.bytes_written as u64)
                        .is_none()
                    {
                        return Err(OpError::unknown(
                            invalid_backend_reply("write offset overflow"),
                            None,
                        ));
                    }
                    Ok(value)
                })
            };
            match outcome {
                Ok(value) => {
                    ops[index].wrote = value.bytes_written;
                    results[index] = success(index, value.offset, false);
                }
                Err(error) => {
                    let offset: Option<u64> = None;
                    if let Some(progress) = error.progress() {
                        let valid_state = error.state() != FailureState::NotAttempted;
                        match progress {
                            IoProgress::Write { bytes_written }
                                if valid_state && *bytes_written <= ops[index].len =>
                            {
                                ops[index].wrote = *bytes_written
                            }
                            _ => {
                                results[index] = failed(
                                    index,
                                    OpError::unknown(
                                        invalid_backend_reply("invalid I/O failure progress"),
                                        None,
                                    ),
                                );
                                continue;
                            }
                        }
                    }
                    results[index] = failed(index, error);
                    if let Some(actual) = offset {
                        results[index].offset = actual;
                        results[index].offset_known = 1;
                    }
                }
            }
        }
        vfsi_result::success(count)
    })
}
