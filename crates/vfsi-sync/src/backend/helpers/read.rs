use super::*;
use vfsi_core::{FailureState, IoProgress, OpError, OpResult, VecResult};

pub fn vread_all_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
) -> VecResult<Vec<u8>> {
    backend.vread_all_with_options_impl(files, ReadAllOptions::default())
}

pub fn vread_all_with_options_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
    options: ReadAllOptions,
) -> VecResult<Vec<u8>> {
    let memory = options
        .max_total_bytes()
        .clamp(1, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    read_all_vectors(
        backend,
        files,
        options,
        |pending, remaining| {
            let chunk = memory.min(1024 * 1024);
            let count = pending.min((memory / chunk).max(1)).min(remaining.max(1));
            (count, chunk.min((remaining / count).max(1)))
        },
        false,
    )
}

/// Collect retained descriptor cohorts, adopting small full first chunks without
/// changing the established allocation policy for large-file reads.
pub fn vread_all_owned_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
    options: ReadAllOptions,
    batch_size: std::num::NonZeroUsize,
) -> VecResult<Vec<u8>> {
    let memory = options
        .max_total_bytes()
        .clamp(1, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    read_all_vectors(
        backend,
        files,
        options,
        |pending, remaining| {
            let chunk = memory
                .min(1024 * 1024)
                .min((memory / pending.clamp(1, batch_size.get())).max(4096));
            let count = pending.min((memory / chunk).max(1)).min(remaining.max(1));
            (count, chunk.min((remaining / count).max(1)))
        },
        true,
    )
}

/// Shared whole-file accumulation. The backend supplies its existing cohort and
/// chunk policy; each logical file stops only on its own EOF or failure.
pub fn read_all_vectors<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
    options: ReadAllOptions,
    plan: impl Fn(usize, usize) -> (usize, usize),
    adopt_small: bool,
) -> VecResult<Vec<u8>> {
    use std::collections::VecDeque;
    let mut pending: VecDeque<_> = (0..files.len()).collect();
    let mut data: Vec<Vec<u8>> = (0..files.len()).map(|_| Vec::new()).collect();
    let mut output: Vec<Option<OpResult<Vec<u8>>>> = (0..files.len()).map(|_| None).collect();
    let mut total = 0usize;
    let mut indices = Vec::new();
    let mut reads = Vec::new();
    while !pending.is_empty() {
        let remaining = options.max_total_bytes() - total;
        let (count, length) = plan(pending.len(), remaining);
        // Callers provide a bounded internal policy, never user projection code.
        assert!(count > 0 && count <= pending.len() && length > 0);
        indices.clear();
        reads.clear();
        for _ in 0..count {
            let index = pending.pop_front().expect("planned pending file");
            reads.push(ReadOp::at(
                files[index].clone(),
                data[index].len() as u64,
                length,
            ));
            indices.push(index);
        }
        let outcomes = match backend.vread_impl(&reads) {
            Ok(mut outcomes) if outcomes.len() <= reads.len() => {
                outcomes.resize_with(reads.len(), || {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "missing whole-file read outcome"),
                        None,
                    ))
                });
                outcomes
            }
            Ok(_) => reads
                .iter()
                .map(|_| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "too many whole-file read outcomes"),
                        None,
                    ))
                })
                .collect(),
            Err(cause) => reads
                .iter()
                .map(|_| Err(OpError::not_attempted(cause.clone())))
                .collect(),
        };
        for ((&index, request), outcome) in indices.iter().zip(&reads).zip(outcomes) {
            let outcome = outcome.and_then(|read| {
                validate_read_results(
                    "read_files",
                    std::slice::from_ref(request),
                    std::slice::from_ref(&read),
                )
                .map_err(|cause| OpError::unknown(cause, None))?;
                Ok(read)
            });
            let (mut chunk, eof, failure) = match outcome {
                Ok(read) => (read.data, read.eof, None),
                Err(error) => {
                    let (cause, state, progress) = error.into_parts();
                    match progress {
                        None => (Vec::new(), false, Some((cause, state))),
                        Some(IoProgress::Read(prefix))
                            if prefix.data().is_some()
                                && prefix.offset() == data[index].len() as u64
                                && prefix.bytes_read() <= length
                                && state != FailureState::NotAttempted =>
                        {
                            let eof = prefix.eof();
                            (
                                prefix.into_data().expect("validated owned prefix"),
                                eof,
                                Some((cause, state)),
                            )
                        }
                        _ => (
                            Vec::new(),
                            false,
                            Some((
                                VfError::invalid_reply(None, "invalid whole-file read prefix"),
                                FailureState::Unknown,
                            )),
                        ),
                    }
                }
            };
            let available = options.max_total_bytes() - total;
            let exceeded = chunk.len() > available;
            if exceeded {
                chunk.truncate(available);
            }
            total += chunk.len();
            let bytes = &mut data[index];
            // Preserve the measured small-file adoption and large-file layout.
            if adopt_small
                && bytes.is_empty()
                && chunk.len() <= 64 * 1024
                && chunk.capacity() == chunk.len()
            {
                *bytes = chunk;
            } else {
                bytes.extend_from_slice(&chunk);
            }
            let failure = failure.or_else(|| {
                exceeded.then(|| {
                    (
                        VfError::errno(None, libc::EFBIG as u32),
                        FailureState::Failed,
                    )
                })
            });
            if let Some((cause, state)) = failure {
                let progress = (!bytes.is_empty()).then(|| {
                    IoProgress::Read(vfsi_core::api::ReadResult::owned(
                        0,
                        std::mem::take(bytes),
                        eof && !exceeded,
                    ))
                });
                output[index] = Some(Err(match state {
                    FailureState::Unknown => OpError::unknown(cause, progress),
                    FailureState::NotAttempted if progress.is_none() => {
                        OpError::not_attempted(cause)
                    }
                    _ => OpError::failed(cause, progress),
                }));
            } else if eof {
                output[index] = Some(Ok(std::mem::take(bytes)));
            } else {
                pending.push_back(index);
            }
        }
    }
    Ok(output
        .into_iter()
        .map(|slot| slot.expect("every whole-file outcome assigned"))
        .collect())
}

pub fn vstream_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
    chunk_size: impl Fn(usize) -> usize,
    memory_limit: usize,
    cb: &mut ReadStreamCallback<'_>,
) -> VfRes {
    stream_read_chunks(
        files,
        chunk_size,
        memory_limit,
        |reads| {
            backend.vread_impl(reads).and_then(|outcomes| {
                let count = outcomes.len();
                crate::internal::collect_vec_result(Ok(outcomes), count)
            })
        },
        |index, result| cb(index, result.offset, &result.data, result.eof),
    )
}

/// Shared bounded stream state machine. The caller controls each read wave's
/// lock scope; callback delivery starts only after the read closure returns.
/// Owned results keep chunk bytes valid independently of that lock.
#[doc(hidden)]
pub fn stream_read_chunks(
    files: &[FileOperand],
    chunk_size: impl Fn(usize) -> usize,
    memory_limit: usize,
    mut read: impl FnMut(&[ReadOp]) -> VfResult<Vec<ReadResult>>,
    mut cb: impl FnMut(usize, ReadResult) -> bool,
) -> VfRes {
    use std::collections::VecDeque;

    if memory_limit == 0 || chunk_size(files.len()) == 0 {
        return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
    }
    let mut pending: VecDeque<usize> = (0..files.len()).collect();
    let mut offsets = vec![0u64; files.len()];
    let mut batch_indices = Vec::new();
    let mut reads = Vec::new();
    while !pending.is_empty() {
        // Select once per round from the live cohort, not the original input.
        // A constant policy preserves the ordinary backend's fixed chunk size.
        let chunk_size = chunk_size(pending.len());
        if chunk_size == 0 {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        let mut budget = memory_limit;
        batch_indices.clear();
        reads.clear();
        while budget > 0 && !pending.is_empty() {
            let index = pending.pop_front().expect("pending was non-empty");
            let length = chunk_size.min(budget);
            reads.push(ReadOp::at(files[index].clone(), offsets[index], length));
            batch_indices.push(index);
            budget -= length;
        }
        let remap_error = |error: VfError| {
            error.map_index(|index| batch_indices.get(index).copied().unwrap_or(index))
        };
        let results = read(&reads).map_err(remap_error)?;
        validate_read_results("vstream_impl", &reads, &results).map_err(remap_error)?;
        for (batch_index, result) in results.into_iter().enumerate() {
            let index = batch_indices[batch_index];
            let offset = offsets[index];
            let eof = result.eof;
            offsets[index] = offset
                .checked_add(result.data.len() as u64)
                .ok_or_else(|| VfError::errno(index, libc::EOVERFLOW as u32))?;
            if !cb(index, result) {
                return Ok(());
            }
            if !eof {
                pending.push_back(index);
            }
        }
    }
    Ok(())
}

pub fn native_read_file_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    file: &FileOperand,
    max_bytes: usize,
) -> VfResult<Vec<u8>> {
    backend
        .vread_all_with_options_impl(
            std::slice::from_ref(file),
            ReadAllOptions::new().with_max_total_bytes(max_bytes),
        )
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .and_then(|mut results| {
            if results.len() != 1 {
                return Err(VfError::invalid_reply(
                    None,
                    "read_file backend returned an invalid result count",
                ));
            }
            let data = results.pop().expect("validated result count");
            if data.len() > max_bytes {
                return Err(VfError::errno(0, libc::EFBIG as u32));
            }
            Ok(data)
        })
}
