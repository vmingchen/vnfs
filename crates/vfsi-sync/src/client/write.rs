//! Write dispatch and acknowledged-progress completion.

use super::*;

// Sparse conflicts commonly give a request just one successor. Retain it
// inline; allocate a vector only when the request has additional successors.
struct WriteSuccessors {
    first: usize,
    more: Vec<usize>,
}

impl Default for WriteSuccessors {
    fn default() -> Self {
        Self {
            first: usize::MAX,
            more: Vec::new(),
        }
    }
}

impl WriteSuccessors {
    fn push(&mut self, successor: usize) {
        if self.first == usize::MAX {
            self.first = successor;
        } else {
            self.more.push(successor);
        }
    }

    fn take(&mut self) -> impl Iterator<Item = usize> {
        let first = std::mem::replace(&mut self.first, usize::MAX);
        std::iter::once(first)
            .filter(|&successor| successor != usize::MAX)
            .chain(std::mem::take(&mut self.more))
    }
}

impl<F: VectorBackend> FsClient<F> {
    #[doc(hidden)]
    pub fn vwrite_native(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<FsWriteResult>> {
        self.vwrite_mapped_native(requests, |request| *request)
    }

    /// Adapter constructing cheap borrowed requests without a temporary vector.
    /// The mapper must return the same file, offset, and payload on each call.
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
        self.vwrite_all_mapped_native(requests, |request| *request)
    }

    /// Complete mapped writes with whole-batch preflight and dependency waves.
    /// The mapper must return the same file, offset, and payload on each call.
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
        // Scalar writes cannot have predecessors. For larger batches,
        // allocate graph slots up front to keep the pairwise conflict loop small.
        let graph_nodes = if requests.len() > 1 {
            requests.len()
        } else {
            0
        };
        let mut successors: Vec<WriteSuccessors> = (0..graph_nodes)
            .map(|_| WriteSuccessors::default())
            .collect();
        let mut remaining_dependencies = vec![0usize; graph_nodes];
        let mut wave_indices = Vec::new();
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
            // Empty requests are complete from the start, after the same
            // whole-batch validation. They neither wait nor block successors.
            if request.data().is_empty() {
                continue;
            }
            // Non-overlapping positional writes commute. Append requests must
            // wait for every earlier write to the same diagnostic path.
            // Finish conflicting requests before dispatching later bytes, so
            // short-write retries cannot overwrite or interleave their payloads.
            let append = request.file().append;
            let prior = prior_by_path.entry(request.file().path()).or_default();
            for &(earlier, start, earlier_end, earlier_append) in prior.iter() {
                if earlier_append || append || (offset < earlier_end && start < end) {
                    successors[earlier].push(index);
                    remaining_dependencies[index] += 1;
                }
            }
            prior.push((index, offset, end, append));
            if remaining_dependencies.get(index).copied().unwrap_or(0) == 0 {
                wave_indices.push(index);
            }
        }
        drop(prior_by_path);
        let mut totals = vec![0usize; requests.len()];
        let mut stable = vec![true; requests.len()];
        let mut next_wave_indices = Vec::new();
        while !wave_indices.is_empty() {
            next_wave_indices.clear();
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
                let request = project(&requests[index]);
                if result.written == 0 {
                    return Err(VfError::client(index, crate::ERR_IO)
                        .with_context("vwrite_all_native", request.file().path()));
                }
                totals[index] += result.written;
                if request.file().append {
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
                if totals[index] == request.data().len() {
                    // Release each edge once, only after all bytes of its
                    // predecessor are acknowledged. Never rescan blocked items.
                    if let Some(successors) = successors.get_mut(index) {
                        for successor in successors.take() {
                            remaining_dependencies[successor] -= 1;
                            if remaining_dependencies[successor] == 0 {
                                next_wave_indices.push(successor);
                            }
                        }
                    }
                } else {
                    next_wave_indices.push(index);
                }
            }
            // Newly released successors may precede or follow ongoing short
            // writes. Match the original input order within every vector wave.
            if !next_wave_indices.is_sorted() {
                next_wave_indices.sort_unstable();
            }
            std::mem::swap(&mut wave_indices, &mut next_wave_indices);
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
    ) -> VfResult<Vec<WriteOp<&'a FileOperand, &'a [u8]>>>
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
