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

type WriteRange = (usize, u64, u64, bool);

#[cfg(test)]
#[test]
fn large_write_graphs_have_linear_edges_and_preserve_initial_readiness() {
    for shape in ["overlap", "scatter", "append"] {
        let count = 4096;
        let mut prior = WriteDependencies::default();
        let mut successors: Vec<_> = (0..count).map(|_| WriteSuccessors::default()).collect();
        let mut remaining = vec![0; count];
        let mut ready = Vec::new();
        for index in 0..count {
            let start = if shape == "scatter" {
                index as u64 * 4
            } else {
                0
            };
            add_write_dependencies(
                (index, start, start + 4, shape == "append"),
                &mut prior,
                &mut successors,
                &mut remaining,
                &mut ready,
            );
        }
        let edges: usize = remaining.iter().sum();
        if shape == "scatter" {
            assert_eq!(edges, 0);
            assert_eq!(ready, (0..count).collect::<Vec<_>>());
        } else {
            assert!(edges <= count + 120, "{shape}: {edges} edges");
            assert_eq!(ready, [0]);
        }
        assert!(prior.prior.is_empty());
        assert!(prior.frontier.is_some());
    }
}

// Small/sparse object cohorts retain the cheap linear path. Larger ones remember
// only the latest writer of each disjoint byte interval. An overwritten writer
// remains ordered through its successors, so redundant transitive edges vanish.
#[derive(Default)]
struct WriteDependencies {
    prior: Vec<WriteRange>,
    frontier: Option<Box<WriteFrontier>>,
}

#[derive(Default)]
struct WriteFrontier {
    ranges: std::collections::BTreeMap<u64, (u64, usize)>,
    append: Option<usize>,
    dependencies: Vec<usize>,
}

impl WriteFrontier {
    fn insert(&mut self, (index, start, end, append): WriteRange, mut depend: impl FnMut(usize)) {
        self.dependencies.clear();
        self.dependencies.extend(self.append);
        if append {
            self.dependencies
                .extend(self.ranges.values().map(|&(_, writer)| writer));
            self.ranges.clear();
            self.append = Some(index);
        } else {
            // Preserve uncovered tails when replacing the most recent writer.
            if let Some((&left, &(right, writer))) = self.ranges.range(..=start).next_back()
                && right > start
            {
                self.ranges.remove(&left);
                self.dependencies.push(writer);
                if left < start {
                    self.ranges.insert(left, (start, writer));
                }
                if end < right {
                    self.ranges.insert(end, (right, writer));
                }
            }
            while let Some((&left, &(right, writer))) = self.ranges.range(start..end).next() {
                self.ranges.remove(&left);
                self.dependencies.push(writer);
                if end < right {
                    self.ranges.insert(end, (right, writer));
                }
            }
            self.ranges.insert(start, (end, index));
        }
        // One writer can own several fragments. Emit its edge only once.
        self.dependencies.sort_unstable();
        self.dependencies.dedup();
        for &writer in &self.dependencies {
            depend(writer);
        }
    }
}

fn add_write_dependencies(
    range: WriteRange,
    prior: &mut WriteDependencies,
    successors: &mut [WriteSuccessors],
    remaining_dependencies: &mut [usize],
    wave_indices: &mut Vec<usize>,
) {
    let (index, offset, end, append) = range;
    let count = prior.prior.len();
    let dense = prior
        .prior
        .last()
        .is_some_and(|&(last, _, _, _)| remaining_dependencies[last] > count / 4);
    // Dense graphs benefit early; a BTreeMap costs more than a few cheap
    // comparisons in a sparse chain. Both thresholds bound the quadratic prefix.
    if prior.frontier.is_none() && count >= 16 && (count >= 256 || dense) {
        let mut frontier = Box::<WriteFrontier>::default();
        for previous in prior.prior.drain(..) {
            frontier.insert(previous, |_| {});
        }
        prior.prior = Vec::new();
        prior.frontier = Some(frontier);
    }
    let mut depend = |earlier: usize| {
        successors[earlier].push(index);
        remaining_dependencies[index] += 1;
    };
    if let Some(frontier) = &mut prior.frontier {
        frontier.insert(range, depend);
    } else {
        for &(earlier, start, earlier_end, earlier_append) in &prior.prior {
            if earlier_append || append || (offset < earlier_end && start < end) {
                depend(earlier);
            }
        }
        prior.prior.push(range);
    }
    if remaining_dependencies[index] == 0 {
        wave_indices.push(index);
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
    /// same object complete in input order, including different open instances
    /// and hard-link aliases. Unknown identities are ordered conservatively.
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
        // Whole-batch validation precedes identity queries and all writes,
        // including validation of empty requests and foreign/closed handles.
        let mut offsets = Vec::with_capacity(requests.len());
        let mut raw_files = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file(), index)?;
            let raw = request.file().raw().map_err(|error| {
                error
                    .with_index(index)
                    .with_context("vwrite_all_native", request.file().path())
            })?;
            let VfOffset::At(offset) = request.offset() else {
                return Err(VfError::client(index, crate::ERR_INVAL)
                    .with_context("vwrite_all_native", request.file().path()));
            };
            offset
                .checked_add(request.data().len() as u64)
                .ok_or_else(|| {
                    VfError::client(index, libc::EOVERFLOW as u32)
                        .with_context("vwrite_all_native", request.file().path())
                })?;
            offsets.push(offset);
            if !request.data().is_empty() {
                raw_files.push(raw);
            }
        }
        let graph_nodes = if raw_files.len() > 1 {
            requests.len()
        } else {
            0
        };
        let mut successors: Vec<WriteSuccessors> = (0..graph_nodes)
            .map(|_| WriteSuccessors::default())
            .collect();
        let mut remaining_dependencies = vec![0usize; graph_nodes];
        let mut wave_indices = Vec::new();
        if raw_files.len() < 2 {
            // A scalar nonempty request needs neither identity nor graph work.
            wave_indices.extend(
                requests
                    .iter()
                    .map(&project)
                    .enumerate()
                    .filter(|(_, request)| !request.data().is_empty())
                    .map(|(index, _)| index),
            );
        } else {
            let map_identity_error = |error: VfError| {
                let error = error.map_index(|raw_index| {
                    requests
                        .iter()
                        .map(&project)
                        .enumerate()
                        .filter(|(_, request)| !request.data().is_empty())
                        .nth(raw_index)
                        .map_or(raw_index, |(index, _)| index)
                });
                error
                    .index()
                    .and_then(|index| requests.get(index).map(&project))
                    .map_or(error.clone(), |request| {
                        error.with_context("vwrite_all_native", request.file().path())
                    })
            };
            let mut backend = self.lock()?;
            backend
                .prepare_object_refs(&raw_files)
                .map_err(map_identity_error)?;
            let mut unknown = false;
            let mut prior_by_object = HashMap::new();
            let mut prior_unknown = WriteDependencies::default();
            for (index, request) in requests.iter().map(&project).enumerate() {
                if request.data().is_empty() {
                    continue;
                }
                let object = backend
                    .open_object_ref(request.file().raw()?)
                    .map_err(|error| {
                        error
                            .with_index(index)
                            .with_context("vwrite_all_native", request.file().path())
                    })?;
                if object.is_none() && !unknown {
                    // Unknown may alias any earlier object. Rebuild the prefix
                    // as one group, without repeating identity queries. Known
                    // batches need only one pass over backend references.
                    unknown = true;
                    prior_by_object.clear();
                    for successors in successors.iter_mut() {
                        *successors = WriteSuccessors::default();
                    }
                    remaining_dependencies.fill(0);
                    wave_indices.clear();
                    for (earlier, previous) in requests[..index].iter().map(&project).enumerate() {
                        if previous.data().is_empty() {
                            continue;
                        }
                        let offset = offsets[earlier];
                        add_write_dependencies(
                            (
                                earlier,
                                offset,
                                offset + previous.data().len() as u64,
                                previous.file().append,
                            ),
                            &mut prior_unknown,
                            &mut successors,
                            &mut remaining_dependencies,
                            &mut wave_indices,
                        );
                    }
                }
                let prior = if unknown {
                    &mut prior_unknown
                } else {
                    prior_by_object
                        .entry(object.expect("known object reference"))
                        .or_default()
                };
                let offset = offsets[index];
                // Complete conflicting requests before dispatching later bytes,
                // so retries cannot interleave aliases or overwrite their data.
                add_write_dependencies(
                    (
                        index,
                        offset,
                        offset + request.data().len() as u64,
                        request.file().append,
                    ),
                    prior,
                    &mut successors,
                    &mut remaining_dependencies,
                    &mut wave_indices,
                );
            }
        }
        drop(raw_files);
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
                if result.bytes_written == 0 {
                    return Err(VfError::client(index, crate::ERR_IO)
                        .with_context("vwrite_all_native", request.file().path()));
                }
                totals[index] += result.bytes_written;
                if request.file().append {
                    // Preserve the last reported end even when outside writers
                    // append between short-write waves. This is an aggregate
                    // completion offset, not a guarantee of a contiguous extent.
                    offsets[index] = result
                        .offset
                        .checked_add(result.bytes_written as u64)
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
                bytes_written: totals[index],
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
