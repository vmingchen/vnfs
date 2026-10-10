//! Write dispatch and acknowledged-progress completion.

use super::*;

impl<F: VectorBackend> FsClient<F> {
    #[doc(hidden)]
    pub fn vwrite_native(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<FsWriteResult>> {
        self.vwrite_projected_native(requests, |request| request)
    }

    /// Backend adapter for opaque application requests, preserving borrowed payloads.
    /// The projection must return the same embedded request on every invocation.
    #[doc(hidden)]
    pub fn vwrite_projected_native<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        self.vwrite_mapped_native(requests, |item| {
            let request = project(item);
            *request
        })
    }

    /// Adapter constructing cheap borrowed requests without a temporary vector.
    /// The mapper must return the same file, offset, and payload on each call.
    #[doc(hidden)]
    #[doc(hidden)]
    pub fn vwrite_mapped_native<'b, T>(
        &self,
        requests: &[T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        let writes = self.write_ops(requests, &project)?;
        let results = self.lock()?.vwrite_impl(&writes).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vwrite_native", request.file().path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count(
                "vwrite_native",
                requests.len(),
                results.len(),
            ));
        }
        validate_write_results("vwrite_native", &writes, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vwrite_native", request.file().path())
                })
        })?;
        Ok(results.into_iter().map(write_result).collect())
    }

    /// Write every byte in each positional request, retrying short writes in
    /// vector waves. Like `vwrite_native`, this is not transactional: an error may
    /// follow a successfully written prefix. Overlapping requests through the
    /// same path complete in input order; different paths are presumed
    /// independent (including hard-link aliases).
    #[doc(hidden)]
    pub fn vwrite_all_native(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<FsWriteResult>> {
        self.vwrite_all_projected_native(requests, |request| request)
    }

    /// Complete projected requests with the same preflight and dependency waves.
    /// The projection must return the same embedded request on every invocation.
    #[doc(hidden)]
    pub fn vwrite_all_projected_native<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        self.vwrite_all_mapped_native(requests, |item| {
            let request = project(item);
            *request
        })
    }

    /// Complete mapped writes with whole-batch preflight and dependency waves.
    /// The mapper must return the same file, offset, and payload on each call.
    #[doc(hidden)]
    #[doc(hidden)]
    pub fn vwrite_all_mapped_native<'b, T>(
        &self,
        requests: &[T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> VfResult<Vec<FsWriteResult>>
    where
        F: 'b,
    {
        // Validate the entire batch before writing any prefix. In particular,
        // empty requests must not conceal a foreign or already-closed file.
        let mut prior_by_path: HashMap<&Path, Vec<(usize, u64, u64, bool)>> = HashMap::new();
        let mut blocked_by = vec![Vec::new(); requests.len()];
        let mut offsets = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file(), index)?;
            request.file().raw().map_err(|error| {
                error
                    .with_index(index)
                    .with_context("vwrite_all_native", request.file().path())
            })?;
            let VfOffset::At(offset) = request.offset() else {
                return Err(VfError::client(index, crate::ERR_INVAL)
                    .with_context("vwrite_all_native", request.file().path()));
            };
            let end = offset
                .checked_add(request.data().len() as u64)
                .ok_or_else(|| {
                    VfError::client(index, libc::EOVERFLOW as u32)
                        .with_context("vwrite_all_native", request.file().path())
                })?;
            offsets.push(offset);
            // Non-overlapping positional writes commute. Append requests must
            // wait for every earlier write to the same diagnostic path.
            // Finish conflicting requests before dispatching later bytes, so
            // short-write retries cannot overwrite or interleave their payloads.
            let prior = prior_by_path.entry(request.file().path()).or_default();
            for &(earlier, start, earlier_end, append) in prior.iter() {
                if append || request.file().append || (offset < earlier_end && start < end) {
                    blocked_by[index].push(earlier);
                }
            }
            prior.push((index, offset, end, request.file().append));
        }
        let mut totals = vec![0usize; requests.len()];
        let mut stable = vec![true; requests.len()];
        let mut pending: Vec<usize> = (0..requests.len())
            .filter(|&index| !project(&requests[index]).data().is_empty())
            .collect();
        while !pending.is_empty() {
            let wave_indices: Vec<usize> = pending
                .iter()
                .copied()
                .filter(|&index| {
                    blocked_by[index]
                        .iter()
                        .all(|&earlier| totals[earlier] == project(&requests[earlier]).data().len())
                })
                .collect();
            let wave: Vec<_> = wave_indices
                .iter()
                .map(|&index| {
                    let request = project(&requests[index]);
                    let VfOffset::At(offset) = request.offset() else {
                        return Err(VfError::client(index, crate::ERR_INVAL)
                            .with_context("vwrite_all_native", request.file().path()));
                    };
                    let offset = offset.checked_add(totals[index] as u64).ok_or_else(|| {
                        VfError::client(index, libc::EOVERFLOW as u32)
                            .with_context("vwrite_all_native", request.file().path())
                    })?;
                    Ok(FsWrite::new(
                        request.file(),
                        VfOffset::At(offset),
                        &request.data()[totals[index]..],
                    ))
                })
                .collect::<VfResult<_>>()?;
            let results = self.vwrite_native(&wave).map_err(|error| {
                error.map_index(|index| wave_indices.get(index).copied().unwrap_or(index))
            })?;
            for (&index, result) in wave_indices.iter().zip(results) {
                if result.written == 0 {
                    return Err(VfError::client(index, crate::ERR_IO).with_context(
                        "vwrite_all_native",
                        project(&requests[index]).file().path(),
                    ));
                }
                totals[index] += result.written;
                if project(&requests[index]).file().append {
                    // Preserve the last reported end even when outside writers
                    // append between short-write waves. This is an aggregate
                    // completion offset, not a guarantee of a contiguous extent.
                    offsets[index] = result
                        .offset
                        .checked_add(result.written as u64)
                        .and_then(|end| end.checked_sub(totals[index] as u64))
                        .ok_or_else(|| {
                            VfError::transport_with_kind(
                                Some(index),
                                crate::TransportKind::InvalidReply,
                                "append completion offset cannot be represented",
                            )
                        })?;
                }
                stable[index] &= result.stable;
            }
            pending.retain(|&index| totals[index] < project(&requests[index]).data().len());
        }
        Ok(offsets
            .into_iter()
            .enumerate()
            .map(|(index, offset)| FsWriteResult {
                offset,
                written: totals[index],
                stable: stable[index],
            })
            .collect())
    }

    fn write_ops<'a, 'b, T>(
        &self,
        requests: &'a [T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> VfResult<Vec<WriteOp<&'a VfFile, &'a [u8]>>>
    where
        F: 'b,
        'b: 'a,
    {
        let mut writes = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file(), index)?;
            writes.push(WriteOp::new(
                request
                    .file()
                    .raw()
                    .map_err(|error| error.with_index(index))?,
                // Canonicalize append requests before common result validation:
                // the backend selects EOF, not the caller's positional offset.
                if request.file().append {
                    VfOffset::End
                } else {
                    request.offset()
                },
                request.data(),
            ));
        }
        Ok(writes)
    }
}
