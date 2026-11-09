//! Read dispatch, completion validation, and bounded streaming.

use super::*;

impl<F: VectorBackend> FsClient<F> {
    /// Read independent inputs with a 16 MiB aggregate request limit.
    /// Outcomes retain input order; a failed input does not discard siblings.
    #[doc(hidden)]
    pub fn vread_native(&self, requests: &[FsRead<'_, F>]) -> vfsi_core::VecResult<FsReadResult> {
        self.vread_with_limit_native(requests, self.limits.max_read_bytes())
    }

    /// Read independent inputs with an explicit aggregate request limit.
    #[doc(hidden)]
    pub fn vread_with_limit_native(
        &self,
        requests: &[FsRead<'_, F>],
        max_total_bytes: usize,
    ) -> vfsi_core::VecResult<FsReadResult> {
        self.vread_with_limit_projected_native(requests, max_total_bytes, |request| request)
    }

    /// Project each input once outside the backend lock. Invalid handles are
    /// isolated before one batch dispatch of the remaining independent inputs.
    #[doc(hidden)]
    pub fn vread_with_limit_projected_native<'b, T>(
        &self,
        requests: &[T],
        max_total_bytes: usize,
        project: impl for<'r> Fn(&'r T) -> &'r FsRead<'b, F>,
    ) -> vfsi_core::VecResult<FsReadResult>
    where
        F: 'b,
    {
        use vfsi_core::OpError;
        let mut requested = 0usize;
        let mut reads = Vec::with_capacity(requests.len());
        let mut paths = Vec::with_capacity(requests.len());
        // Homogeneous valid batches need neither an index map nor placeholder
        // results. Allocate them only when preflight rejects an input.
        let mut indices = Vec::new();
        let mut rejected = Vec::new();
        for (index, request) in requests.iter().map(project).enumerate() {
            requested = requested
                .checked_add(request.length)
                .filter(|bytes| *bytes <= max_total_bytes)
                .ok_or_else(|| {
                    VfError::errno(index, libc::EFBIG as u32)
                        .with_context("vread_native", request.file.path())
                })?;
            let raw = self
                .validate_owner(request.file, index)
                .and_then(|()| request.file.raw().cloned());
            match raw {
                Ok(raw) => {
                    if !rejected.is_empty() {
                        indices.push(index);
                    }
                    reads.push(ReadOp::new(raw, request.offset, request.length));
                    paths.push(request.file.path());
                }
                Err(error) => {
                    if rejected.is_empty() {
                        indices.extend(0..index);
                    }
                    rejected.push((
                        index,
                        OpError::not_attempted(error)
                            .with_context("vread_native", request.file.path()),
                    ));
                }
            }
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let results = if reads.is_empty() {
            Vec::new()
        } else {
            // The backend contract permits an outer error only before dispatch.
            let outcomes = self.lock()?.vread_impl(&reads);
            match outcomes {
                Err(error) if rejected.is_empty() => {
                    return Err(with_indexed_context(error, "vread_native", |index| {
                        paths.get(index).copied()
                    }));
                }
                Err(error) => reads
                    .iter()
                    .map(|_| Err(OpError::not_attempted(error.clone())))
                    .collect(),
                Ok(mut outcomes) if outcomes.len() <= reads.len() => {
                    outcomes.resize_with(reads.len(), || {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "missing read outcome"),
                            None,
                        ))
                    });
                    outcomes
                        .into_iter()
                        .enumerate()
                        .map(|(index, outcome)| {
                            let request = &reads[index];
                            outcome
                                .and_then(|result| {
                                    validate_read_results(
                                        "vread_native",
                                        std::slice::from_ref(request),
                                        std::slice::from_ref(&result),
                                    )
                                    .map_err(|error| OpError::unknown(error, None))?;
                                    Ok(read_result(result))
                                })
                                .map_err(|error| validate_read_error(request, error, true))
                        })
                        .collect()
                }
                Ok(_) => reads
                    .iter()
                    .map(|_| {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "read backend returned too many outcomes"),
                            None,
                        ))
                    })
                    .collect(),
            }
        };
        let results = results
            .into_iter()
            .zip(paths)
            .map(|(result, path)| result.map_err(|error| error.with_context("vread_native", path)));
        if rejected.is_empty() {
            return Ok(results.collect());
        }
        let mut output: Vec<_> = (0..requests.len()).map(|_| None).collect();
        for (index, result) in indices.into_iter().zip(results) {
            output[index] = Some(result);
        }
        for (index, error) in rejected {
            output[index] = Some(Err(error));
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every read input assigned"))
            .collect())
    }

    #[doc(hidden)]
    pub fn vread_into_native(
        &self,
        requests: &mut [FsReadInto<'_, F>],
    ) -> vfsi_core::VecResult<FsReadIntoResult> {
        self.vread_into_with_limit_native(requests, self.limits.max_read_bytes())
    }

    /// Read into caller storage with an aggregate buffer budget. Rejected
    /// handles leave their buffers untouched and do not suppress valid siblings.
    #[doc(hidden)]
    pub fn vread_into_with_limit_native(
        &self,
        requests: &mut [FsReadInto<'_, F>],
        max_bytes: usize,
    ) -> vfsi_core::VecResult<FsReadIntoResult> {
        use vfsi_core::OpError;
        let mut requested = 0usize;
        let mut reads = Vec::with_capacity(requests.len());
        let mut paths = Vec::with_capacity(requests.len());
        let mut indices = Vec::new();
        let mut rejected = Vec::new();
        for (index, request) in requests.iter().enumerate() {
            requested = requested
                .checked_add(request.buffer.len())
                .filter(|bytes| *bytes <= max_bytes)
                .ok_or_else(|| {
                    VfError::errno(index, libc::EFBIG as u32)
                        .with_context("vread_into_native", request.file.path())
                })?;
            match self
                .validate_owner(request.file, index)
                .and_then(|()| request.file.raw().cloned())
            {
                Ok(raw) => {
                    if !rejected.is_empty() {
                        indices.push(index);
                    }
                    reads.push(ReadOp::new(raw, request.offset, request.buffer.len()));
                    paths.push(request.file.path());
                }
                Err(error) => {
                    if rejected.is_empty() {
                        indices.extend(0..index);
                    }
                    rejected.push((
                        index,
                        OpError::not_attempted(error)
                            .with_context("vread_into_native", request.file.path()),
                    ));
                }
            }
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let results: Vec<vfsi_core::OpResult<FsReadIntoResult>> = if reads.is_empty() {
            Vec::new()
        } else {
            let outcomes = {
                let mut selected = indices.iter().copied().peekable();
                let mut buffers: Vec<_> = requests
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(index, request)| {
                        if rejected.is_empty() {
                            return Some(&mut *request.buffer);
                        }
                        if selected.peek() != Some(&index) {
                            return None;
                        }
                        selected.next();
                        Some(&mut *request.buffer)
                    })
                    .collect();
                self.lock()?.vread_into_impl(&reads, &mut buffers)
            };
            match outcomes {
                Err(error) if rejected.is_empty() => {
                    return Err(with_indexed_context(error, "vread_into_native", |index| {
                        paths.get(index).copied()
                    }));
                }
                Err(error) => reads
                    .iter()
                    .map(|_| Err(OpError::not_attempted(error.clone())))
                    .collect(),
                Ok(mut outcomes) if outcomes.len() <= reads.len() => {
                    outcomes.resize_with(reads.len(), || {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "missing buffered read outcome"),
                            None,
                        ))
                    });
                    outcomes
                        .into_iter()
                        .enumerate()
                        .map(|(index, outcome)| {
                            let request = &reads[index];
                            outcome
                                .and_then(|result| {
                                    validate_read_into_results(
                                        "vread_into_native",
                                        std::slice::from_ref(request),
                                        std::slice::from_ref(&result),
                                    )
                                    .map_err(|error| OpError::unknown(error, None))?;
                                    Ok(FsReadIntoResult {
                                        offset: result.offset,
                                        bytes_read: result.bytes_read,
                                        eof: result.eof,
                                    })
                                })
                                .map_err(|error| validate_read_error(request, error, false))
                        })
                        .collect()
                }
                Ok(_) => reads
                    .iter()
                    .map(|_| {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "read backend returned too many outcomes"),
                            None,
                        ))
                    })
                    .collect(),
            }
        };
        let results = results.into_iter().zip(paths).map(|(result, path)| {
            result.map_err(|error| error.with_context("vread_into_native", path))
        });
        if rejected.is_empty() {
            return Ok(results.collect());
        }
        let mut output: Vec<_> = (0..requests.len()).map(|_| None).collect();
        for (index, result) in indices.into_iter().zip(results) {
            output[index] = Some(result);
        }
        for (index, error) in rejected {
            output[index] = Some(Err(error));
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every buffered input assigned"))
            .collect())
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Native bounded whole-file reads for backend adapters.
    ///
    /// Applications should use [`VfsiExt::read_files_with_options`].
    #[doc(hidden)]
    pub fn read_files_native<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: ReadAllOptions,
    ) -> vfsi_core::VecResult<Vec<u8>> {
        use vfsi_core::{IoProgress, OpError};
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let files: Vec<_> = paths
            .iter()
            .map(|path| FileOperand::from_os_path(path.as_ref()))
            .collect();
        let mut outcomes = self
            .lock()?
            .vread_all_with_options_impl(&files, options)
            .map_err(|error| {
                with_indexed_context(error, "read_files", |index| {
                    paths.get(index).map(|path| path.as_ref())
                })
            })?;
        if outcomes.len() > paths.len() {
            outcomes = paths
                .iter()
                .map(|_| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "too many whole-file outcomes"),
                        None,
                    ))
                })
                .collect();
        } else {
            outcomes.resize_with(paths.len(), || {
                Err(OpError::unknown(
                    VfError::invalid_reply(None, "missing whole-file outcome"),
                    None,
                ))
            });
        }
        let mut remaining = options.max_total_bytes();
        Ok(outcomes
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                let request = ReadOp::at(files[index].clone(), 0, remaining);
                let result = result
                    .map_err(|error| validate_read_error(&request, error, true))
                    .and_then(|data| {
                        if data.len() > remaining {
                            Err(OpError::unknown(
                                VfError::invalid_reply(
                                    None,
                                    "whole-file backend exceeded byte budget",
                                ),
                                None,
                            ))
                        } else {
                            Ok(data)
                        }
                    });
                remaining -= match &result {
                    Ok(data) => data.len(),
                    Err(error) => match error.progress() {
                        Some(IoProgress::Read(prefix)) => prefix.bytes_read(),
                        _ => 0,
                    },
                };
                result.map_err(|error| error.with_context("read_files", paths[index].as_ref()))
            })
            .collect())
    }
}

// Malformed progress is not safe to publish as confirmed data. Preserve valid
// prefixes without cloning their allocation; sibling results stay intact.
fn validate_read_error(
    request: &ReadOp,
    error: vfsi_core::OpError,
    owned: bool,
) -> vfsi_core::OpError {
    use vfsi_core::{FailureState, IoProgress, OpError};
    let valid = match error.progress() {
        None => true,
        Some(IoProgress::Read(progress)) => {
            error.state() != FailureState::NotAttempted
                && progress.data().is_some() == owned
                && progress.bytes_read() <= request.length
                && match request.offset {
                    VfOffset::At(offset) => progress.offset() == offset,
                    _ => true,
                }
        }
        Some(IoProgress::Write { .. } | IoProgress::Stream { .. }) => false,
    };
    if valid {
        error
    } else {
        OpError::unknown(
            VfError::invalid_reply(None, "invalid read failure progress"),
            None,
        )
    }
}
