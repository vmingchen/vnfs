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
    /// Submit independent borrowed writes and retain each input's completion.
    #[doc(hidden)]
    pub fn vwrite_native(
        &self,
        requests: &[FsWrite<'_, F>],
    ) -> vfsi_core::VecResult<FsWriteResult> {
        self.vwrite_mapped_native(requests, |request| *request)
    }

    /// Project every request once outside the backend lock. Reject invalid
    /// handles independently, then dispatch the remaining borrowed payloads once.
    #[doc(hidden)]
    pub fn vwrite_mapped_native<'b, T>(
        &self,
        requests: &[T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> vfsi_core::VecResult<FsWriteResult>
    where
        F: 'b,
    {
        use vfsi_core::OpError;
        let mut writes = Vec::with_capacity(requests.len());
        let mut paths = Vec::with_capacity(requests.len());
        let mut indices = Vec::new();
        let mut rejected = Vec::new();
        for (index, request) in requests.iter().map(project).enumerate() {
            let file = request.file();
            let offset = if file.append {
                VfOffset::End
            } else {
                request.offset()
            };
            let prepared = self.validate_owner(file, index).and_then(|()| {
                if let VfOffset::At(offset) = offset {
                    offset
                        .checked_add(request.data().len() as u64)
                        .ok_or_else(|| VfError::errno(None, libc::EOVERFLOW as u32))?;
                }
                file.raw()
            });
            match prepared {
                Ok(raw) => {
                    if !rejected.is_empty() {
                        indices.push(index);
                    }
                    writes.push(WriteOp::new(raw, offset, request.data()));
                    paths.push(file.path());
                }
                Err(cause) => {
                    if rejected.is_empty() {
                        indices.extend(0..index);
                    }
                    rejected.push((
                        index,
                        OpError::not_attempted(cause).with_context("vwrite_native", file.path()),
                    ));
                }
            }
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let results = if writes.is_empty() {
            Vec::new()
        } else {
            let outcomes = self.lock()?.vwrite_impl(&writes);
            match outcomes {
                Err(cause) if rejected.is_empty() => {
                    return Err(with_indexed_context(cause, "vwrite_native", |index| {
                        paths.get(index).copied()
                    }));
                }
                Err(cause) => writes
                    .iter()
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect(),
                Ok(mut outcomes) if outcomes.len() <= writes.len() => {
                    outcomes.resize_with(writes.len(), || {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "missing write outcome"),
                            None,
                        ))
                    });
                    outcomes
                        .into_iter()
                        .enumerate()
                        .map(|(index, outcome)| {
                            let request = &writes[index];
                            outcome
                                .and_then(|result| {
                                    validate_write_results(
                                        "vwrite_native",
                                        std::slice::from_ref(request),
                                        std::slice::from_ref(&result),
                                    )
                                    .map_err(|cause| OpError::unknown(cause, None))?;
                                    Ok(write_result(result))
                                })
                                .map_err(|error| validate_write_error(request, error))
                        })
                        .collect()
                }
                Ok(_) => writes
                    .iter()
                    .map(|_| {
                        Err(OpError::unknown(
                            VfError::invalid_reply(
                                None,
                                "write backend returned too many outcomes",
                            ),
                            None,
                        ))
                    })
                    .collect(),
            }
        };
        let results = results.into_iter().zip(paths).map(|(result, path)| {
            result.map_err(|error| error.with_context("vwrite_native", path))
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
            .map(|slot| slot.expect("every WRITE input assigned"))
            .collect())
    }

    /// Write every byte in each positional request, retrying short writes in
    /// vector waves. Like `vwrite_native`, this is not transactional: an error may
    /// follow a successfully written prefix. Overlapping requests through the
    /// same object complete in input order, including different open instances
    /// and hard-link aliases. Unknown identities are ordered conservatively.
    #[doc(hidden)]
    pub fn vwrite_all_native(
        &self,
        requests: &[FsWrite<'_, F>],
    ) -> vfsi_core::VecResult<FsWriteResult> {
        self.vwrite_all_mapped_native(requests, |request| *request)
    }

    /// Complete independent mapped writes in dependency waves. A definite
    /// failure releases later writes; uncertain effects block only descendants.
    /// Each borrowed request is projected once, before acquiring the backend lock.
    #[doc(hidden)]
    pub fn vwrite_all_mapped_native<'b, T>(
        &self,
        requests: &[T],
        project: impl Fn(&T) -> FsWrite<'b, F>,
    ) -> vfsi_core::VecResult<FsWriteResult>
    where
        F: 'b,
    {
        use vfsi_core::internal::add_write_prefix;
        use vfsi_core::{FailureState, OpError};
        let mut prepared = Vec::with_capacity(requests.len());
        let mut raw_files = Vec::with_capacity(requests.len());
        let mut failures = vec![None; requests.len()];
        for (index, request) in requests.iter().map(project).enumerate() {
            let valid = (|| {
                self.validate_owner(request.file(), index)?;
                let raw = request.file().raw()?;
                let VfOffset::At(offset) = request.offset() else {
                    return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
                };
                let offset = if request.file().append && !request.data().is_empty() {
                    0 // supplied positions are ignored for append
                } else {
                    offset
                        .checked_add(request.data().len() as u64)
                        .ok_or_else(|| VfError::errno(None, libc::EOVERFLOW as u32))?;
                    offset
                };
                Ok((raw, offset))
            })();
            let offset = match valid {
                Ok((raw, offset)) => {
                    if !request.data().is_empty() {
                        raw_files.push(raw);
                    }
                    offset
                }
                Err(cause) => {
                    failures[index] = Some(OpError::not_attempted(cause));
                    0
                }
            };
            prepared.push((request, offset));
        }
        let graph_nodes = if raw_files.len() > 1 {
            prepared.len()
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
                prepared
                    .iter()
                    .map(|(request, _)| *request)
                    .enumerate()
                    .filter(|(index, request)| {
                        !request.data().is_empty() && failures[*index].is_none()
                    })
                    .map(|(index, _)| index),
            );
        } else {
            let mut backend = self.lock()?;
            // Identity discovery is an optimization. If unavailable, preserve
            // alias safety with the existing conservative unknown-object graph.
            let all_unknown = backend.prepare_object_refs(&raw_files).is_err();
            let mut unknown = false;
            let mut prior_by_object = HashMap::new();
            let mut prior_unknown = WriteDependencies::default();
            for (index, request) in prepared.iter().map(|(request, _)| *request).enumerate() {
                if request.data().is_empty() || failures[index].is_some() {
                    continue;
                }
                let object = if all_unknown {
                    None
                } else {
                    backend
                        .open_object_ref(request.file().raw().expect("validated handle"))
                        .ok()
                        .flatten()
                };
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
                    for (earlier, previous) in prepared[..index]
                        .iter()
                        .map(|(request, _)| *request)
                        .enumerate()
                    {
                        if previous.data().is_empty() || failures[earlier].is_some() {
                            continue;
                        }
                        let offset = prepared[earlier].1;
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
                let offset = prepared[index].1;
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
        let mut totals = vec![0usize; prepared.len()];
        let mut stable = vec![true; prepared.len()];
        let mut next_wave_indices = Vec::new();
        let mut stopped = None;
        while !wave_indices.is_empty() {
            next_wave_indices.clear();
            let wave: Vec<_> = wave_indices
                .iter()
                .map(|&index| {
                    let request = prepared[index].0;
                    let offset = if request.file().append {
                        0
                    } else {
                        // Preflight checked the entire positional payload.
                        prepared[index].1 + totals[index] as u64
                    };
                    FsWrite::new(
                        request.file(),
                        VfOffset::At(offset),
                        &request.data()[totals[index]..],
                    )
                })
                .collect();
            let outcomes = match self.vwrite_native(&wave) {
                Ok(outcomes) => outcomes,
                Err(cause) => {
                    stopped = Some(cause);
                    break;
                }
            };
            for (&index, outcome) in wave_indices.iter().zip(outcomes) {
                let request = prepared[index].0;
                match outcome {
                    Err(error) => {
                        failures[index] =
                            Some(add_write_prefix(error, totals[index], request.data().len()))
                    }
                    Ok(result) if result.bytes_written == 0 => {
                        failures[index] = Some(add_write_prefix(
                            OpError::failed(
                                VfError::from_kind(None, std::io::ErrorKind::WriteZero),
                                None,
                            ),
                            totals[index],
                            request.data().len(),
                        ));
                    }
                    Ok(result) => {
                        totals[index] += result.bytes_written;
                        stable[index] &= result.stable;
                        if request.file().append {
                            // Preserve the final observed end across append gaps.
                            match result
                                .offset
                                .checked_add(result.bytes_written as u64)
                                .and_then(|end| end.checked_sub(totals[index] as u64))
                            {
                                Some(offset) => prepared[index].1 = offset,
                                None => {
                                    failures[index] = Some(add_write_prefix(
                                        OpError::unknown(
                                            VfError::invalid_reply(
                                                None,
                                                "append completion offset cannot be represented",
                                            ),
                                            None,
                                        ),
                                        totals[index],
                                        request.data().len(),
                                    ))
                                }
                            }
                        }
                    }
                }
                if failures[index].is_some() || totals[index] == request.data().len() {
                    let blocked = failures[index]
                        .as_ref()
                        .filter(|error| error.state() == FailureState::Unknown)
                        .map(|error| error.cause().clone());
                    finish_write_dependencies(
                        index,
                        blocked.as_ref(),
                        &mut successors,
                        &mut remaining_dependencies,
                        &mut failures,
                        &mut next_wave_indices,
                    );
                } else {
                    next_wave_indices.push(index);
                }
            }
            if !next_wave_indices.is_sorted() {
                next_wave_indices.sort_unstable();
            }
            std::mem::swap(&mut wave_indices, &mut next_wave_indices);
        }
        Ok(prepared
            .into_iter()
            .zip(failures)
            .enumerate()
            .map(|(index, ((request, offset), error))| {
                if let Some(error) = error {
                    return Err(error.with_context("vwrite_all_native", request.file().path()));
                }
                if totals[index] != request.data().len() {
                    let cause = stopped.clone().unwrap_or_else(|| {
                        VfError::invalid_reply(
                            None,
                            "complete-write dependency graph did not finish",
                        )
                    });
                    return Err(add_write_prefix(
                        OpError::not_attempted(cause),
                        totals[index],
                        request.data().len(),
                    )
                    .with_context("vwrite_all_native", request.file().path()));
                }
                Ok(FsWriteResult {
                    offset,
                    bytes_written: totals[index],
                    stable: stable[index],
                })
            })
            .collect())
    }
}

fn validate_write_error(
    request: &WriteOp<&FileOperand, &[u8]>,
    error: vfsi_core::OpError,
) -> vfsi_core::OpError {
    use vfsi_core::{FailureState, IoProgress, OpError};
    let valid = match error.progress() {
        None => true,
        Some(IoProgress::Write { bytes_written }) => {
            error.state() != FailureState::NotAttempted && *bytes_written <= request.data().len()
        }
        Some(IoProgress::Read(_) | IoProgress::Stream { .. }) => false,
    };
    if valid {
        error
    } else {
        OpError::unknown(
            VfError::invalid_reply(None, "invalid write failure progress"),
            None,
        )
    }
}

// The graph orders mutations, not error reporting. Definite failures are
// terminal and release successors. Unknown effects cannot safely precede a
// later overlapping write, so cancel only that node's dependency descendants.
fn finish_write_dependencies(
    index: usize,
    blocked: Option<&VfError>,
    successors: &mut [WriteSuccessors],
    remaining: &mut [usize],
    failures: &mut [Option<vfsi_core::OpError>],
    ready: &mut Vec<usize>,
) {
    let Some(edges) = successors.get_mut(index) else {
        return;
    };
    let mut edges = std::mem::take(edges);
    if let Some(cause) = blocked {
        let mut pending: Vec<_> = edges.take().collect();
        while let Some(next) = pending.pop() {
            if failures[next].is_some() {
                continue;
            }
            failures[next] = Some(vfsi_core::OpError::not_attempted(cause.clone()));
            let mut edges = std::mem::take(&mut successors[next]);
            pending.extend(edges.take());
        }
    } else {
        for next in edges.take() {
            remaining[next] -= 1;
            if remaining[next] == 0 && failures[next].is_none() {
                ready.push(next);
            }
        }
    }
}
