use super::*;

pub fn vread_into_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    reads: &[ReadOp],
    buffers: &mut [&mut [u8]],
) -> vfsi_core::VecResult<ReadIntoResult> {
    use vfsi_core::{FailureState, IoProgress, OpError};
    if reads.len() != buffers.len() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    for (request, buffer) in reads.iter().zip(buffers.iter()) {
        if request.length != buffer.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
    }
    let outcomes = backend.vread_impl(reads)?;
    if outcomes.len() > reads.len() {
        return Ok(reads
            .iter()
            .map(|_| {
                Err(OpError::unknown(
                    vfsi_core::internal::errors::invalid_reply(
                        "read backend returned too many outcomes",
                    ),
                    None,
                ))
            })
            .collect());
    }
    let mut outcomes = outcomes.into_iter();
    Ok(reads.iter().zip(buffers.iter_mut()).map(|(request, buffer)| {
        let outcome = outcomes.next().unwrap_or_else(|| Err(OpError::unknown(
            vfsi_core::internal::errors::invalid_reply("missing read outcome"), None,
        )));
        match outcome {
            Ok(result) => {
                validate_read_results("vread_into_impl", std::slice::from_ref(request),
                    std::slice::from_ref(&result)).map_err(vfsi_core::IndexedError::into_cause).map_err(|error| OpError::unknown(error, None))?;
                buffer[..result.data.len()].copy_from_slice(&result.data);
                Ok(ReadIntoResult {
                    file: result.file, offset: result.offset,
                    bytes_read: result.data.len(), eof: result.eof,
                })
            }
            Err(error) => {
                let (cause, state, progress) = error.into_parts();
                let progress = match progress {
                    None => None,
                    Some(IoProgress::Read(prefix)) if state != FailureState::NotAttempted
                        && prefix.data().is_some()
                        && prefix.bytes_read() <= buffer.len()
                        && !matches!(request.offset, VfOffset::At(offset) if prefix.offset() != offset) => {
                            let data = prefix.data().expect("validated owned prefix");
                            buffer[..data.len()].copy_from_slice(data);
                            Some(IoProgress::Read(vfsi_core::api::ReadOutput::buffered(
                                prefix.offset(), data.len(), prefix.eof(),
                            )))
                        }
                    _ => return Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply("invalid owned read failure progress"), None,
                    )),
                };
                Err(match state {
                    FailureState::NotAttempted => OpError::not_attempted(cause),
                    FailureState::Failed => OpError::failed(cause, progress),
                    FailureState::Unknown => OpError::unknown(cause, progress),
                })
            }
        }
    }).collect())
}

pub fn before_open_cleanup_default<F: VectorBackend + ?Sized>(
    _backend: &mut F,
    _index: usize,
    _file: &FileOperand,
) -> VfResult<()> {
    Ok(())
}

/// Strict compatibility collection: select one input error and clean every
/// confirmed descriptor, without promising rollback or an unexecuted suffix.
pub fn open_raw_batch<F: VectorBackend + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: &[i32],
    modes: &[u32],
) -> Result<Vec<FileOperand>, vfsi_core::IndexedError> {
    let results = backend
        .vopen_raw_impl(paths, flags, modes)
        .map_err(vfsi_core::IndexedError::unattributed)?;
    let results = ManyResults::new(
        paths.len(),
        results
            .into_iter()
            .map(|result| result.map_err(vfsi_core::OpError::into_cause))
            .collect(),
    );
    let mut seen = std::collections::HashSet::new();
    results.try_collect_with_cleanup(paths.len(), |index, file| {
        if !file.fd().is_some_and(|fd| fd >= 0 && seen.insert(fd)) {
            return Ok(());
        }
        let injected = backend.before_open_cleanup(index, file);
        let closed = backend.close_impl(file);
        injected.and(closed)
    })
}

pub fn vopen_raw_simple_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: i32,
    mode: u32,
) -> VfResult<Vec<FileOperand>> {
    let flags_v = vec![flags; paths.len()];
    let modes_v = vec![mode; paths.len()];
    open_raw_batch(backend, paths, &flags_v, &modes_v).map_err(vfsi_core::IndexedError::into_cause)
}

pub fn vopen_typed_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    requests: &[OpenOp],
) -> vfsi_core::VecResult<FileOperand> {
    let paths: Vec<&std::path::Path> = requests.iter().map(|request| request.path()).collect();
    let mut flags = translate_open_flags(requests)?;
    let positioned = backend.positioned_io_open_flags();
    if positioned != 0 {
        for (request, flags) in requests.iter().zip(&mut flags) {
            if request.is_positioned_io() {
                *flags |= positioned;
            }
        }
    }
    let modes: Vec<u32> = requests
        .iter()
        .map(|request| request.creation_mode())
        .collect();
    backend.vopen_raw_impl(&paths, &flags, &modes)
}

pub fn native_read_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
) -> VfResult<ReadOutput> {
    let mut results = vfsi_core::internal::collect_vec_result(
        backend.vread_impl(std::slice::from_ref(request)),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)?;
    validate_read_results("read_impl", std::slice::from_ref(request), &results)
        .map_err(vfsi_core::IndexedError::into_cause)?;
    Ok(results.pop().expect("validated one result"))
}

pub fn native_read_into_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
    buffer: &mut [u8],
) -> VfResult<ReadIntoResult> {
    let mut results = vfsi_core::internal::collect_vec_result(
        backend.vread_into_impl(std::slice::from_ref(request), &mut [buffer]),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)?;
    validate_read_into_results("read_into_impl", std::slice::from_ref(request), &results)
        .map_err(vfsi_core::IndexedError::into_cause)?;
    Ok(results.pop().expect("validated one result"))
}

pub fn native_write_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    request: WriteOp<&FileOperand, &[u8]>,
) -> VfResult<WriteOutput> {
    let mut results = vfsi_core::internal::collect_vec_result(
        backend.vwrite_impl(std::slice::from_ref(&request)),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)?;
    validate_write_results("write_impl", std::slice::from_ref(&request), &results)
        .map_err(vfsi_core::IndexedError::into_cause)?;
    Ok(results.pop().expect("validated one result"))
}
