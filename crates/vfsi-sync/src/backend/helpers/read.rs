use super::*;
use vfsi_core::{FailureState, IndexedError, IoProgress, OpError, OpResult, VecResult};

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
                        vfsi_core::internal::errors::invalid_reply(
                            "missing whole-file read outcome",
                        ),
                        None,
                    ))
                });
                outcomes
            }
            Ok(_) => reads
                .iter()
                .map(|_| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply(
                            "too many whole-file read outcomes",
                        ),
                        None,
                    ))
                })
                .collect(),
            Err(cause) => vfsi_core::internal::errors::fanout(cause, reads.len())
                .map(|cause| Err(OpError::not_attempted(cause)))
                .collect(),
        };
        for ((&index, request), outcome) in indices.iter().zip(&reads).zip(outcomes) {
            let outcome = outcome.and_then(|read| {
                validate_read_results(
                    "read_files",
                    std::slice::from_ref(request),
                    std::slice::from_ref(&read),
                )
                .map_err(|error| OpError::unknown(error.into_cause(), None))?;
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
                                vfsi_core::internal::errors::invalid_reply(
                                    "invalid whole-file read prefix",
                                ),
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
                        std::io::Error::from_raw_os_error(libc::EFBIG),
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
) -> Result<(), IndexedError> {
    stream_read_chunks(
        files,
        chunk_size,
        memory_limit,
        |reads| crate::internal::collect_vec_result(backend.vread_impl(reads), reads.len()),
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
    mut read: impl FnMut(&[ReadOp]) -> Result<Vec<ReadResult>, IndexedError>,
    mut cb: impl FnMut(usize, ReadResult) -> bool,
) -> Result<(), IndexedError> {
    use std::collections::VecDeque;

    if memory_limit == 0 || chunk_size(files.len()) == 0 {
        return Err(IndexedError::unattributed(std::io::Error::from(
            std::io::ErrorKind::InvalidInput,
        )));
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
            return Err(IndexedError::unattributed(std::io::Error::from(
                std::io::ErrorKind::InvalidInput,
            )));
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
        let remap = |error: IndexedError| match error.index() {
            Some(index) => match batch_indices.get(index) {
                Some(&owner) => error.map_index(|_| owner),
                None => IndexedError::unattributed(vfsi_core::internal::errors::invalid_reply(
                    "stream error index is outside the submitted wave",
                )),
            },
            None => error,
        };
        let results = read(&reads).map_err(remap)?;
        validate_read_results("vstream_impl", &reads, &results).map_err(remap)?;
        for (batch_index, result) in results.into_iter().enumerate() {
            let index = batch_indices[batch_index];
            let offset = offsets[index];
            let eof = result.eof;
            offsets[index] = offset
                .checked_add(result.data.len() as u64)
                .ok_or_else(|| {
                    IndexedError::at(index, std::io::Error::from_raw_os_error(libc::EOVERFLOW))
                })?;
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
                .map_err(vfsi_core::IndexedError::into_cause)
        })
        .and_then(|mut results| {
            if results.len() != 1 {
                return Err(vfsi_core::internal::errors::invalid_reply(
                    "read_file backend returned an invalid result count",
                ));
            }
            let data = results.pop().expect("validated result count");
            if data.len() > max_bytes {
                return Err(std::io::Error::from_raw_os_error(libc::EFBIG));
            }
            Ok(data)
        })
}

#[cfg(test)]
mod stream_error_tests {
    use super::*;

    #[test]
    fn stream_errors_reject_indices_outside_the_submitted_wave() {
        let files: Vec<_> = (0..4).map(FileOperand::from_fd).collect();
        let mut calls = 0;
        let error = stream_read_chunks(
            &files,
            |_| 1,
            1,
            |_| {
                calls += 1;
                // Input 1 exists globally but was absent from this one-input wave.
                Err(IndexedError::at(
                    1,
                    std::io::Error::from_raw_os_error(libc::EACCES),
                ))
            },
            |_, _| panic!("failed wave must not deliver data"),
        )
        .unwrap_err();
        assert_eq!(calls, 1, "contract errors must not trigger replay");
        assert_eq!(error.index(), None);
        assert_eq!(
            vfsi_core::transport_kind(error.cause()),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
    }

    #[test]
    fn stream_errors_map_wave_indices_and_preserve_unknown_attribution() {
        for index in [Some(0), None] {
            let files: Vec<_> = (0..4).map(FileOperand::from_fd).collect();
            let mut calls = 0;
            let mut delivered = Vec::new();
            let error = stream_read_chunks(
                &files,
                |_| 1,
                1,
                |reads| {
                    calls += 1;
                    if calls == 3 {
                        return Err(match index {
                            Some(index) => IndexedError::at(
                                index,
                                std::io::Error::from_raw_os_error(libc::EACCES),
                            ),
                            None => IndexedError::unattributed(std::io::Error::from_raw_os_error(
                                libc::EACCES,
                            )),
                        });
                    }
                    Ok(vec![ReadResult {
                        file: reads[0].file.clone(),
                        offset: 0,
                        data: vec![1],
                        eof: true,
                    }])
                },
                |input, _| {
                    delivered.push(input);
                    true
                },
            )
            .unwrap_err();
            assert_eq!(calls, 3);
            assert_eq!(delivered, [0, 1]);
            assert_eq!(error.index(), index.map(|_| 2));
            assert_eq!(error.cause().raw_os_error(), Some(libc::EACCES));
        }
    }
    #[test]
    fn malformed_later_wave_results_keep_item_indices_but_not_cardinality_indices() {
        for malformed in 0..5 {
            let files: Vec<_> = (0..6).map(FileOperand::from_fd).collect();
            let mut calls = 0;
            let mut delivered = Vec::new();
            let error = stream_read_chunks(
                &files,
                |_| 1,
                2,
                |requests| {
                    calls += 1;
                    let mut results: Vec<_> = requests
                        .iter()
                        .map(|request| ReadResult {
                            file: request.file.clone(),
                            offset: 0,
                            data: vec![1],
                            eof: true,
                        })
                        .collect();
                    if calls == 2 {
                        match malformed {
                            0 => results[1].offset = 1,
                            1 => results[1].file = FileOperand::from_fd(99),
                            2 => results[1].data.push(2),
                            3 => {
                                results[1].data.clear();
                                results[1].eof = false;
                            }
                            _ => {
                                results.pop();
                            }
                        }
                    }
                    Ok(results)
                },
                |input, _| {
                    delivered.push(input);
                    true
                },
            )
            .unwrap_err();
            assert_eq!(error.index(), (malformed < 4).then_some(3));
            assert_eq!(
                vfsi_core::transport_kind(error.cause()),
                Some(vfsi_core::TransportKind::InvalidReply)
            );
            assert_eq!(
                calls, 2,
                "never replay validation failures or dispatch another wave"
            );
            assert_eq!(
                delivered,
                [0, 1],
                "validate the complete wave before callbacks"
            );
        }
    }
}
