//! Descriptor and path I/O planning, execution, and completion.

use super::*;
use vfsi_core::{FailureState, IoProgress, OpError, OpResult, VecResult};

/// Keep a path truncate after all preceding writes have completed. Separate
/// opens may otherwise truncate an alias while an earlier suffix is pending.
pub(super) fn path_write_groups<'a>(
    writes: &'a [WriteOp<&FileOperand, &[u8]>],
) -> impl Iterator<Item = std::ops::Range<usize>> + 'a {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == writes.len() {
            return None;
        }
        let truncates = |index: usize| !writes[index].file().is_fd() && writes[index].truncates();
        let end = if truncates(start) {
            start + 1
        } else {
            (start + 1..writes.len())
                .find(|&index| truncates(index))
                .unwrap_or(writes.len())
        };
        let group = start..end;
        start = end;
        Some(group)
    })
}

// A yielded path input may already have completed earlier compounds. Flags
// from its first OPEN must never be reapplied to an acknowledged continuation.
fn retry_path_write<'a>(
    original: &'a WriteOp<&FileOperand, &[u8]>,
    kind: crate::client::PathWriteRepairKind,
    offset: u64,
    confirmed: usize,
) -> WriteOp<&'a FileOperand, &'a [u8]> {
    use crate::client::PathWriteRepairKind;
    let mut pending = WriteOp::new(
        original.file(),
        VfOffset::At(offset + confirmed as u64),
        &original.data()[confirmed..],
    );
    match kind {
        PathWriteRepairKind::Symlink if confirmed == 0 => {
            if original.creates() {
                pending = pending.with_creation();
            }
            if original.truncates() {
                pending = pending.with_truncate();
            }
        }
        PathWriteRepairKind::TruncateStateid => pending = pending.with_truncate(),
        PathWriteRepairKind::Symlink | PathWriteRepairKind::WriteStateid => {}
    }
    pending
}

pub(super) fn write_all_at<C>(
    client: &mut C,
    index: usize,
    offset: u64,
    data: &[u8],
    mut write: impl FnMut(&mut C, u64, &[u8]) -> VfResult<usize>,
) -> VfResult<()> {
    let data_len =
        u64::try_from(data.len()).map_err(|_| VfError::errno(index, libc::EOVERFLOW as u32))?;
    offset
        .checked_add(data_len)
        .ok_or_else(|| VfError::errno(index, libc::EOVERFLOW as u32))?;

    let mut progress = 0usize;
    while progress < data.len() {
        let current = offset
            .checked_add(progress as u64)
            .ok_or_else(|| VfError::errno(index, libc::EOVERFLOW as u32))?;
        let remaining = &data[progress..];
        let written = write(client, current, remaining)?;
        if written == 0 || written > remaining.len() {
            return Err(VfError::errno(index, ERR_IO));
        }
        progress += written;
    }
    Ok(())
}

// A cursor read observes the actual progress of an earlier read on the same
// descriptor. Keep all independent inputs in the same compound wave.
fn independent_read_prefix(reads: &[ReadOp]) -> usize {
    let mut cursors = std::collections::HashSet::new();
    reads
        .iter()
        .position(|read| {
            read.offset == VfOffset::Cur && read.file.fd().is_some_and(|fd| !cursors.insert(fd))
        })
        .unwrap_or(reads.len())
}

struct DescriptorReadPlan {
    ops: Vec<crate::client::ReadOp>,
    owners: Vec<usize>,
    offsets: Vec<u64>,
    errors: Vec<Option<OpError>>,
    metadata_started: bool,
}

pub(super) use crate::client::WriteWaveAccess;

#[cfg(test)]
pub(super) fn remap_descriptor_chunk_error(
    error: VfError,
    start: usize,
    owners: &[usize],
) -> VfError {
    error.map_index(|local_index| {
        let chunk_index = start.saturating_add(local_index);
        owners.get(chunk_index).copied().unwrap_or(chunk_index)
    })
}

pub(super) fn remap_active_error(error: VfError, active: &[usize]) -> VfError {
    match error.index().and_then(|index| active.get(index)).copied() {
        Some(original) => error.with_index(original),
        None => error,
    }
}

pub(super) fn bounded_read_allv_batch(
    pending: usize,
    remaining: usize,
    compound_budget: usize,
    max_window: usize,
) -> (usize, usize) {
    if remaining == 0 {
        // A one-byte EOF probe distinguishes exact-limit completion from a
        // file that exceeds the logical payload budget.
        return (1, 1);
    }
    let count = pending.min(remaining);
    let protocol_window = (compound_budget / count)
        .saturating_sub(128)
        .min(max_window)
        .max(1);
    (count, protocol_window.min((remaining / count).max(1)))
}

/// Keep the first cause while retaining uncertainty anywhere in this input.
fn merge_descriptor_failure(failure: &mut Option<OpError>, next: OpError) {
    if let Some(first) = failure {
        if next.state() == FailureState::Unknown && first.state() != FailureState::Unknown {
            let first = failure.take().expect("present");
            *failure = Some(OpError::unknown(first.into_cause(), None));
        }
    } else {
        *failure = Some(next);
    }
}

impl NfsVecFs {
    /// Selective read recovery keeps successful siblings and resumes only an
    /// unsuccessful input's remaining range. Prefix storage moves on errors.
    pub(super) fn read_results(&mut self, reads: &[ReadOp]) -> VecResult<ReadResult> {
        if let Some(index) = reads
            .iter()
            .position(|read| read.length > isize::MAX as usize)
        {
            return Err(VfError::errno(index, libc::EFBIG as u32));
        }
        let first_end = independent_read_prefix(reads);
        if first_end < reads.len() {
            let mut output = Vec::with_capacity(reads.len());
            let mut start = 0;
            while start < reads.len() {
                let end = start + independent_read_prefix(&reads[start..]);
                // Finish recovery and publish confirmed cursor progress before
                // resolving the next dependent input.
                match self.read_results(&reads[start..end]) {
                    Ok(results) => output.extend(results),
                    Err(cause) if start == 0 => return Err(cause),
                    Err(cause) => output
                        .extend((start..end).map(|_| Err(OpError::not_attempted(cause.clone())))),
                }
                start = end;
            }
            return Ok(output);
        }
        type SavedPrefix = (FailureState, Option<vfsi_core::api::ReadResult>);
        let retryable = |index: usize, error: &OpError| match error.progress() {
            Some(IoProgress::Read(read)) => !read.eof() && read.bytes_read() < reads[index].length,
            Some(_) => false,
            None => true,
        };
        let mut saved: Vec<Option<SavedPrefix>> = Vec::new();
        let result = if self.recovery_in_progress {
            self.read_unrecovered(reads)
        } else {
            self.vector_read_with_recovery(retryable, |client, indices| {
                if let Some(indices) = indices {
                    let requests: Vec<_> = indices
                        .iter()
                        .map(|&index| {
                            let prefix = saved
                                .get(index)
                                .and_then(Option::as_ref)
                                .and_then(|(_, read)| read.as_ref());
                            let (offset, length) = match prefix {
                                Some(prefix) => (
                                    VfOffset::At(prefix.offset() + prefix.bytes_read() as u64),
                                    reads[index].length - prefix.bytes_read(),
                                ),
                                None => (reads[index].offset, reads[index].length),
                            };
                            ReadOp::new(reads[index].file.clone(), offset, length)
                        })
                        .collect();
                    let outcomes = client.read_unrecovered(&requests)?;
                    if outcomes.len() != indices.len() {
                        return Ok(outcomes);
                    }
                    return Ok(outcomes
                        .into_iter()
                        .enumerate()
                        .map(|(relative, result)| {
                            let Some(&index) = indices.get(relative) else {
                                return result;
                            };
                            result.map(|mut result| {
                                if let Some((_, prefix)) = saved[index].take()
                                    && let Some(prefix) = prefix
                                {
                                    result.offset = prefix.offset();
                                    let mut data =
                                        prefix.into_data().expect("owned native read prefix");
                                    data.extend_from_slice(&result.data);
                                    result.data = data;
                                }
                                result
                            })
                        })
                        .collect());
                }
                let outcomes = client.read_unrecovered(reads)?;
                if !outcomes.iter().enumerate().any(|(index, result)| {
                    result.as_ref().err().is_some_and(|error| {
                        Self::needs_recovery(error.cause()) && retryable(index, error)
                    })
                }) {
                    return Ok(outcomes);
                }
                saved.resize_with(reads.len(), || None);
                Ok(outcomes
                    .into_iter()
                    .enumerate()
                    .map(|(index, result)| match result {
                        Err(error)
                            if Self::needs_recovery(error.cause()) && retryable(index, &error) =>
                        {
                            let (cause, state, progress) = error.into_parts();
                            let prefix = match progress {
                                Some(IoProgress::Read(read)) => Some(read),
                                _ => None,
                            };
                            saved[index] = Some((state, prefix));
                            Err(match state {
                                FailureState::NotAttempted => OpError::not_attempted(cause),
                                FailureState::Failed => OpError::failed(cause, None),
                                FailureState::Unknown => OpError::unknown(cause, None),
                            })
                        }
                        result => result,
                    })
                    .collect())
            })
        };
        let outcomes = result?;
        let outcomes: Vec<_> = outcomes
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.map_err(|error| {
                    let Some((first_state, prefix)) = saved.get_mut(index).and_then(Option::take)
                    else {
                        return error;
                    };
                    let (cause, state, progress) = error.into_parts();
                    let progress = match (prefix, progress) {
                        (Some(prefix), next) => {
                            let offset = prefix.offset();
                            let mut eof = prefix.eof();
                            let mut data = prefix.into_data().expect("owned native read prefix");
                            if let Some(IoProgress::Read(next)) = next {
                                eof = next.eof();
                                data.extend_from_slice(
                                    next.data().expect("owned native retry prefix"),
                                );
                            }
                            Some(IoProgress::Read(vfsi_core::api::ReadResult::owned(
                                offset, data, eof,
                            )))
                        }
                        (None, progress) => progress,
                    };
                    if state == FailureState::Unknown || first_state == FailureState::Unknown {
                        OpError::unknown(cause, progress)
                    } else if state == FailureState::NotAttempted
                        && first_state == FailureState::NotAttempted
                    {
                        OpError::not_attempted(cause)
                    } else {
                        OpError::failed(cause, progress)
                    }
                })
            })
            .collect();
        for (read, outcome) in reads.iter().zip(&outcomes) {
            if read.offset != VfOffset::Cur {
                continue;
            }
            let progress = match outcome {
                Ok(result) => Some((result.offset, result.data.len())),
                Err(error) => match error.progress() {
                    Some(IoProgress::Read(prefix)) => Some((prefix.offset(), prefix.bytes_read())),
                    _ => None,
                },
            };
            if let Some((offset, count)) = progress {
                self.advance_offset(&read.file, offset + count as u64);
            }
        }
        Ok(outcomes)
    }

    pub(super) fn read_unrecovered(&mut self, reads: &[ReadOp]) -> VecResult<ReadResult> {
        if let Some(index) = reads
            .iter()
            .position(|read| read.length > isize::MAX as usize)
        {
            return Err(VfError::errno(index, libc::EFBIG as u32));
        }
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        if reads.iter().all(|read| read.file.is_fd()) {
            return self.descriptor_read_results(reads);
        }
        if self.merged_mode == MergedIoMode::Off {
            return self.phased_read_results(reads);
        }
        let mut output: Vec<Option<OpResult<ReadResult>>> =
            (0..reads.len()).map(|_| None).collect();
        let mut indices = Vec::new();
        let mut ops = Vec::new();
        let mut requests = Vec::new();
        for (index, read) in reads.iter().enumerate() {
            let prepared = (|| -> OpResult<_> {
                if read.file.is_fd()
                    && !read
                        .file
                        .fd()
                        .is_some_and(|fd| self.open_files.contains_key(&fd))
                {
                    return Err(OpError::not_attempted(VfError::errno(None, ERR_EBADF)));
                }
                let offset = match read.offset {
                    VfOffset::At(offset) => offset,
                    VfOffset::Cur => read
                        .file
                        .fd()
                        .and_then(|fd| self.open_files.get(&fd).map(|open| open.cur_offset))
                        .ok_or_else(|| {
                            OpError::not_attempted(VfError::from_kind(
                                None,
                                std::io::ErrorKind::InvalidInput,
                            ))
                        })?,
                    VfOffset::End => {
                        let file = self.resolve_tcfile(&read.file, true).map_err(|cause| {
                            if cause.is_transport() {
                                OpError::unknown(cause, None)
                            } else {
                                OpError::failed(cause, None)
                            }
                        })?;
                        self.file_size(&file).map_err(|cause| {
                            if cause.is_transport() {
                                OpError::unknown(cause, None)
                            } else {
                                OpError::failed(cause, None)
                            }
                        })?
                    }
                    _ => {
                        return Err(OpError::not_attempted(VfError::from_kind(
                            None,
                            std::io::ErrorKind::InvalidInput,
                        )));
                    }
                };
                offset.checked_add(read.length as u64).ok_or_else(|| {
                    let cause = VfError::errno(None, libc::EOVERFLOW as u32);
                    if read.offset == VfOffset::End {
                        OpError::failed(cause, None)
                    } else {
                        OpError::not_attempted(cause)
                    }
                })?;
                let file = if let Some(fd) = read.file.fd() {
                    crate::client::FileRef::Handle(self.open_files[&fd].fh.clone())
                } else {
                    crate::client::FileRef::Path(
                        path_bytes(
                            &self
                                .server_vf_path(&read.file)
                                .map_err(OpError::not_attempted)?,
                        )
                        .to_vec(),
                    )
                };
                Ok(crate::client::PathReadOp {
                    file,
                    offset,
                    count: read.length,
                    stateid: read
                        .file
                        .fd()
                        .and_then(|fd| self.open_files.get(&fd).map(|open| open.stateid)),
                })
            })();
            match prepared {
                Ok(op) => {
                    requests.push(ReadOp::new(
                        read.file.clone(),
                        VfOffset::At(op.offset),
                        read.length,
                    ));
                    indices.push(index);
                    ops.push(op);
                }
                Err(error) => output[index] = Some(Err(error)),
            }
        }
        if !ops.is_empty() {
            let full = self.merged_mode == MergedIoMode::Full;
            match self.nfs.readv_path_compound(&ops, full) {
                Ok(outcome) => {
                    self.close_path_opens(&outcome.opened);
                    if outcome.results.len() != ops.len() {
                        let cause =
                            VfError::invalid_reply(None, "invalid merged READ outcome count");
                        for index in indices {
                            output[index] = Some(Err(OpError::unknown(cause.clone(), None)));
                        }
                        return Ok(output
                            .into_iter()
                            .map(|slot| slot.expect("every NFS read resolved"))
                            .collect());
                    }
                    let close_owner = outcome.close_failed.and_then(|_| {
                        ops.iter()
                            .enumerate()
                            .rev()
                            .find(|(index, op)| {
                                matches!(op.file, crate::client::FileRef::Path(_))
                                    && outcome.results.get(*index).is_some_and(|result| {
                                        result.as_ref().err().is_none_or(|error| {
                                            error.state() != FailureState::NotAttempted
                                        })
                                    })
                            })
                            .map(|(index, _)| index)
                    });
                    if outcome.close_failed.is_some() {
                        self.merged_mode = MergedIoMode::OpenWrite;
                    }
                    let payloads =
                        outcome
                            .results
                            .into_iter()
                            .enumerate()
                            .map(|(index, result)| {
                                result.and_then(|(data, eof)| {
                                    if Some(index) == close_owner {
                                        Err(OpError::failed(
                                            VfError::nfs(
                                                None,
                                                outcome.close_failed.expect("close failure"),
                                            ),
                                            Some(IoProgress::Read(
                                                vfsi_core::api::ReadResult::owned(
                                                    ops[index].offset,
                                                    data,
                                                    eof,
                                                ),
                                            )),
                                        ))
                                    } else {
                                        Ok((data, eof))
                                    }
                                })
                            });
                    let mut results =
                        Self::assemble_reads(&requests, ops.iter().map(|op| op.offset), payloads);
                    if full && results.iter().any(|result| result.as_ref().err().is_some_and(|error|
                        error.progress().is_none() && error.cause().status().is_some_and(|status|
                            matches!(status, vfsi_core::StatusCode::Nfs(status) if crate::client::is_stateid_error(status)))))
                    {
                        self.merged_mode = MergedIoMode::Off;
                    }
                    let retry: Vec<_> = results.iter().enumerate().filter_map(|(index, result)| {
                        result.as_ref().err().filter(|error| error.progress().is_none()
                            && error.state() != FailureState::Unknown
                            && (outcome.close_failed.is_some() && error.state() == FailureState::NotAttempted
                                || error.cause().status().is_some_and(|status| matches!(status,
                                    vfsi_core::StatusCode::Nfs(status) if status == nfsstat4_NFS4ERR_SYMLINK
                                        || status == nfsstat4_NFS4ERR_INVAL || crate::client::is_stateid_error(status)))))
                            .map(|_| index)
                    }).collect();
                    if !retry.is_empty() {
                        let pending: Vec<_> =
                            retry.iter().map(|&index| requests[index].clone()).collect();
                        match self.phased_read_results(&pending) {
                            Ok(retried) if retried.len() == retry.len() => {
                                for (index, result) in retry.into_iter().zip(retried) {
                                    results[index] = result;
                                }
                            }
                            result => {
                                let cause = result.err().unwrap_or_else(|| {
                                    VfError::invalid_reply(None, "invalid phased READ retry count")
                                });
                                for index in retry {
                                    results[index] = Err(OpError::unknown(cause.clone(), None));
                                }
                            }
                        }
                    }
                    for (index, result) in indices.into_iter().zip(results) {
                        output[index] = Some(result);
                    }
                }
                Err(cause) => {
                    for index in indices {
                        output[index] = Some(Err(if reads[index].offset == VfOffset::End {
                            OpError::failed(cause.clone(), None)
                        } else {
                            OpError::not_attempted(cause.clone())
                        }));
                    }
                }
            }
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every NFS read resolved"))
            .collect())
    }

    /// Batched readv for open (descriptor) ops: one compound per chunk of
    /// files, each carrying `[PUTFH, READ]` for every op.
    /// Fill a confirmed short READ at its actual continuation offset. This
    /// slow path adds no RPCs when the original wire chunk was complete.
    fn complete_read_chunk(
        &mut self,
        op: &crate::client::ReadOp,
        mut received: usize,
        mut eof: bool,
        index: usize,
        mut on_data: impl FnMut(&[u8]),
    ) -> OpResult<(usize, bool)> {
        if received > op.count as usize || (op.count != 0 && received == 0 && !eof) {
            return Err(OpError::failed(VfError::errno(None, ERR_IO), None));
        }
        while received < op.count as usize && !eof {
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeReadRepair { index })
                .map_err(OpError::not_attempted)?;
            #[cfg(not(feature = "test-faults"))]
            let _ = index;
            let remaining = op.count as usize - received;
            let retry = crate::client::ReadOp {
                fh: op.fh.clone(),
                stateid: op.stateid,
                offset: op.offset.checked_add(received as u64).ok_or_else(|| {
                    OpError::not_attempted(VfError::errno(None, libc::EOVERFLOW as u32))
                })?,
                count: remaining as u32,
            };
            let mut result = self.nfs.readv(&[retry]).map_err(OpError::not_attempted)?;
            if result.len() != 1 {
                return Err(OpError::unknown(
                    VfError::invalid_reply(None, "malformed short READ reply"),
                    None,
                ));
            }
            let (data, end) = result.pop().expect("one result validated")?;
            if data.len() > remaining || (data.is_empty() && !end) {
                return Err(OpError::failed(VfError::errno(None, ERR_IO), None));
            }
            on_data(&data);
            received += data.len();
            eof = end;
        }
        Ok((received, eof))
    }

    fn descriptor_read_plan(&mut self, reads: &[ReadOp]) -> DescriptorReadPlan {
        let per = self.nfs.read_per_op_bytes().clamp(1, u32::MAX as usize);
        let mut plan = DescriptorReadPlan {
            ops: Vec::with_capacity(reads.len()),
            owners: Vec::with_capacity(reads.len()),
            offsets: vec![0; reads.len()],
            errors: vec![None; reads.len()],
            metadata_started: false,
        };
        for (index, op) in reads.iter().enumerate() {
            let Some(open) = op
                .file
                .fd()
                .and_then(|fd| self.open_files.get(&fd))
                .cloned()
            else {
                plan.errors[index] = Some(OpError::not_attempted(VfError::errno(None, ERR_EBADF)));
                continue;
            };
            let offset = match op.offset {
                VfOffset::At(offset) => offset,
                VfOffset::Cur => open.cur_offset,
                VfOffset::End => {
                    plan.metadata_started = true;
                    match self.file_size(&open.fh) {
                        Ok(size) => size,
                        Err(cause) => {
                            plan.errors[index] = Some(if cause.is_transport() {
                                OpError::unknown(cause, None)
                            } else {
                                OpError::failed(cause, None)
                            });
                            continue;
                        }
                    }
                }
                _ => {
                    plan.errors[index] = Some(OpError::not_attempted(VfError::from_kind(
                        None,
                        std::io::ErrorKind::InvalidInput,
                    )));
                    continue;
                }
            };
            let Some(_end) = offset.checked_add(op.length as u64) else {
                let cause = VfError::errno(None, libc::EOVERFLOW as u32);
                plan.errors[index] = Some(if op.offset == VfOffset::End {
                    OpError::failed(cause, None)
                } else {
                    OpError::not_attempted(cause)
                });
                continue;
            };
            plan.offsets[index] = offset;
            let mut remaining = op.length;
            let mut chunk_offset = offset;
            loop {
                let length = remaining.min(per);
                plan.ops.push(crate::client::ReadOp {
                    fh: open.fh.clone(),
                    stateid: open.stateid,
                    offset: chunk_offset,
                    count: length as u32,
                });
                plan.owners.push(index);
                remaining -= length;
                if remaining == 0 {
                    break;
                }
                chunk_offset += length as u64;
            }
        }
        plan
    }

    /// One rich buffered engine for both owned and borrowed descriptor reads.
    /// Cursors are published by the native caller after its collection policy.
    pub(super) fn descriptor_read_into_results(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VecResult<ReadIntoResult> {
        if reads.len() != buffers.len() {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        if let Some(index) = reads
            .iter()
            .zip(buffers.iter())
            .position(|(op, buffer)| op.length != buffer.len())
        {
            return Err(VfError::from_kind(index, std::io::ErrorKind::InvalidInput));
        }
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        let DescriptorReadPlan {
            ops,
            owners,
            offsets,
            mut errors,
            metadata_started,
        } = self.descriptor_read_plan(reads);
        let mut wire_lengths = vec![0; ops.len()];
        let mut results = if ops.is_empty() {
            Vec::new()
        } else {
            let wire = self.nfs.readv_into(&ops, |wire_index, data| {
                let owner = owners[wire_index];
                #[cfg(feature = "test-faults")]
                let data = if let Some((index, limit)) = self.short_read_once
                    && index == owner
                {
                    self.short_read_once = None;
                    &data[..limit.min(data.len())]
                } else {
                    data
                };
                let start = (ops[wire_index].offset - offsets[owner]) as usize;
                let destination = buffers[owner]
                    .get_mut(start..start + data.len())
                    .ok_or_else(|| RpcError::transport("NFS READ exceeds caller storage"))?;
                destination.copy_from_slice(data);
                wire_lengths[wire_index] = data.len();
                Ok(())
            });
            match wire {
                Ok(results) => results,
                Err(cause) if !metadata_started => return Err(cause),
                Err(cause) => (0..ops.len())
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect(),
            }
        };
        if results.len() != ops.len() {
            let cause = VfError::invalid_reply(None, "NFS READ returned wrong result count");
            results = (0..ops.len())
                .map(|_| Err(OpError::unknown(cause.clone(), None)))
                .collect();
        }
        let mut wire = results.into_iter();
        let mut wire_index = 0;
        let mut output = Vec::with_capacity(reads.len());
        for (index, read) in reads.iter().enumerate() {
            if let Some(error) = errors[index].take() {
                output.push(Err(error));
                continue;
            }
            let offset = offsets[index];
            let mut length = 0usize;
            let mut eof = false;
            let mut attempted = read.offset == VfOffset::End;
            let mut failure = None;
            while wire_index < ops.len() && owners[wire_index] == index {
                let current = wire_index;
                wire_index += 1;
                let result = wire.next().expect("wire cardinality validated");
                if eof {
                    continue;
                }
                match result {
                    Ok((_count, end)) => {
                        attempted = true;
                        if failure.is_some() {
                            continue;
                        }
                        let received = wire_lengths[current];
                        length += received;
                        let mut start = (ops[current].offset - offset) as usize + received;
                        let repaired = self.complete_read_chunk(
                            &ops[current],
                            received,
                            end && received == _count,
                            index,
                            |data| {
                                buffers[index][start..start + data.len()].copy_from_slice(data);
                                start += data.len();
                                length += data.len();
                            },
                        );
                        match repaired {
                            Ok((_, end)) => eof = end,
                            Err(error) => merge_descriptor_failure(&mut failure, error),
                        }
                    }
                    Err(error) => {
                        attempted |= error.state() != FailureState::NotAttempted;
                        merge_descriptor_failure(&mut failure, error);
                    }
                }
            }
            if let Some(error) = failure {
                let (cause, mut state, _) = error.into_parts();
                if attempted && state == FailureState::NotAttempted {
                    state = FailureState::Failed;
                }
                let progress = (length > 0).then(|| {
                    IoProgress::Read(vfsi_core::api::ReadResult::buffered(offset, length, eof))
                });
                output.push(Err(match state {
                    FailureState::NotAttempted => OpError::not_attempted(cause),
                    FailureState::Failed => OpError::failed(cause, progress),
                    FailureState::Unknown => OpError::unknown(cause, progress),
                }));
            } else {
                output.push(Ok(ReadIntoResult {
                    file: read.file.clone(),
                    offset,
                    bytes_read: length,
                    eof,
                }));
            }
        }
        Ok(output)
    }

    pub(super) fn descriptor_read_results(&mut self, reads: &[ReadOp]) -> VecResult<ReadResult> {
        if let Some(index) = reads.iter().position(|op| op.length > isize::MAX as usize) {
            return Err(VfError::errno(index, libc::EFBIG as u32));
        }
        let mut storage: Vec<_> = reads.iter().map(|op| vec![0; op.length]).collect();
        let mut buffers: Vec<_> = storage.iter_mut().map(Vec::as_mut_slice).collect();
        let results = self.descriptor_read_into_results(reads, &mut buffers)?;
        Ok(results
            .into_iter()
            .zip(storage)
            .map(|(result, mut data)| match result {
                Ok(result) => {
                    data.truncate(result.bytes_read);
                    Ok(ReadResult {
                        file: result.file,
                        offset: result.offset,
                        data,
                        eof: result.eof,
                    })
                }
                Err(error) => {
                    let offset = match error.progress() {
                        Some(IoProgress::Read(read)) => read.offset(),
                        _ => 0,
                    };
                    Err(vfsi_core::internal::own_read_error(error, offset, data))
                }
            })
            .collect())
    }

    pub(super) fn read_into_results(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VecResult<ReadIntoResult> {
        if reads.len() != buffers.len() {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        if let Some(index) = reads
            .iter()
            .zip(buffers.iter())
            .position(|(read, buffer)| read.length != buffer.len())
        {
            return Err(VfError::from_kind(index, std::io::ErrorKind::InvalidInput));
        }
        if let Some(index) = reads
            .iter()
            .position(|read| read.length > isize::MAX as usize)
        {
            return Err(VfError::errno(index, libc::EFBIG as u32));
        }
        let first_end = independent_read_prefix(reads);
        if first_end < reads.len() {
            let mut output = Vec::with_capacity(reads.len());
            let mut start = 0;
            while start < reads.len() {
                let end = start + independent_read_prefix(&reads[start..]);
                match self.read_into_results(&reads[start..end], &mut buffers[start..end]) {
                    Ok(results) => output.extend(results),
                    Err(cause) if start == 0 => return Err(cause),
                    Err(cause) => output
                        .extend((start..end).map(|_| Err(OpError::not_attempted(cause.clone())))),
                }
                start = end;
            }
            return Ok(output);
        }
        // Saved progress is only metadata: confirmed bytes stay in the caller's
        // destination while recovery borrows disjoint remaining suffixes.
        type Prefix = (FailureState, u64, usize, bool);
        let retryable = |index: usize, error: &OpError| match error.progress() {
            Some(IoProgress::Read(read)) => !read.eof() && read.bytes_read() < reads[index].length,
            Some(_) => false,
            None => true,
        };
        let mut saved: Vec<Option<Prefix>> = Vec::new();
        let outcomes = if self.recovery_in_progress {
            self.read_into_unrecovered(reads, buffers)
        } else {
            self.vector_read_with_recovery(retryable, |client, indices| {
                let Some(indices) = indices else {
                    let outcomes = client.read_into_unrecovered(reads, buffers)?;
                    if outcomes.iter().enumerate().any(|(index, result)| {
                        result.as_ref().err().is_some_and(|error| {
                            Self::needs_recovery(error.cause()) && retryable(index, error)
                        })
                    }) {
                        saved.resize_with(reads.len(), || None);
                        for (index, result) in outcomes.iter().enumerate() {
                            if let Err(error) = result
                                && Self::needs_recovery(error.cause())
                                && retryable(index, error)
                            {
                                let (offset, count, eof) = match error.progress() {
                                    Some(IoProgress::Read(prefix)) => {
                                        (prefix.offset(), prefix.bytes_read(), prefix.eof())
                                    }
                                    _ => (0, 0, false),
                                };
                                saved[index] = Some((error.state(), offset, count, eof));
                            }
                        }
                    }
                    return Ok(outcomes);
                };
                let requests: Vec<_> = indices
                    .iter()
                    .map(|&index| {
                        let (_, offset, count, _) = saved[index].expect("saved retry progress");
                        ReadOp::new(
                            reads[index].file.clone(),
                            if count > 0 {
                                VfOffset::At(offset + count as u64)
                            } else {
                                reads[index].offset
                            },
                            reads[index].length - count,
                        )
                    })
                    .collect();
                let mut selected = indices.iter().copied().peekable();
                let mut destinations: Vec<_> = buffers
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(index, buffer)| {
                        if selected.peek() != Some(&index) {
                            return None;
                        }
                        selected.next();
                        let (_, _, count, _) = saved[index].expect("saved retry progress");
                        Some(&mut buffer[count..])
                    })
                    .collect();
                let outcomes = client.read_into_unrecovered(&requests, &mut destinations)?;
                if outcomes.len() != indices.len() {
                    return Ok(outcomes);
                }
                Ok(outcomes
                    .into_iter()
                    .zip(indices)
                    .map(|(result, &index)| {
                        let (first_state, offset, count, eof) =
                            saved[index].expect("saved retry progress");
                        match result {
                            Ok(mut result) => {
                                if count > 0 {
                                    result.offset = offset;
                                    result.bytes_read += count;
                                }
                                Ok(result)
                            }
                            Err(error) => {
                                let (cause, state, next) = error.into_parts();
                                let progress = match next {
                                    Some(IoProgress::Read(next)) => Some(IoProgress::Read(
                                        vfsi_core::api::ReadResult::buffered(
                                            if count > 0 { offset } else { next.offset() },
                                            count + next.bytes_read(),
                                            next.eof(),
                                        ),
                                    )),
                                    _ if count > 0 => Some(IoProgress::Read(
                                        vfsi_core::api::ReadResult::buffered(offset, count, eof),
                                    )),
                                    next => next,
                                };
                                Err(
                                    if state == FailureState::Unknown
                                        || first_state == FailureState::Unknown
                                    {
                                        OpError::unknown(cause, progress)
                                    } else if state == FailureState::NotAttempted
                                        && first_state == FailureState::NotAttempted
                                    {
                                        OpError::not_attempted(cause)
                                    } else {
                                        OpError::failed(cause, progress)
                                    },
                                )
                            }
                        }
                    })
                    .collect())
            })
        }?;
        let outcomes: Vec<_> = outcomes
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.map_err(|error| {
                    let Some((first_state, offset, count, eof)) =
                        saved.get(index).copied().flatten()
                    else {
                        return error;
                    };
                    if count == 0 || error.progress().is_some() {
                        return error;
                    }
                    let (cause, state, _) = error.into_parts();
                    let progress = Some(IoProgress::Read(vfsi_core::api::ReadResult::buffered(
                        offset, count, eof,
                    )));
                    if state == FailureState::Unknown || first_state == FailureState::Unknown {
                        OpError::unknown(cause, progress)
                    } else {
                        OpError::failed(cause, progress)
                    }
                })
            })
            .collect();
        for (read, result) in reads.iter().zip(&outcomes) {
            if read.offset != VfOffset::Cur {
                continue;
            }
            let progress = match result {
                Ok(result) => Some((result.offset, result.bytes_read)),
                Err(error) => match error.progress() {
                    Some(IoProgress::Read(prefix)) => Some((prefix.offset(), prefix.bytes_read())),
                    _ => None,
                },
            };
            if let Some((offset, count)) = progress {
                self.advance_offset(&read.file, offset + count as u64);
            }
        }
        Ok(outcomes)
    }

    fn read_into_unrecovered(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VecResult<ReadIntoResult> {
        if reads.iter().all(|read| read.file.is_fd()) {
            self.descriptor_read_into_results(reads, buffers)
        } else {
            // The merged path decoder owns its payloads; the shared adapter
            // copies each known result or prefix without discarding siblings.
            vfsi_sync::backend::helpers::vread_into_impl_default(self, reads, buffers)
        }
    }

    /// Resolve a [`VfOffset`] to a concrete file offset.
    fn resolve_offset(&mut self, file: &FileOperand, off: VfOffset) -> VfResult<u64> {
        match off {
            VfOffset::At(offset) => Ok(offset),
            VfOffset::Cur => match file.fd() {
                Some(fd) => Ok(self.open_files.get(&fd).map(|o| o.cur_offset).unwrap_or(0)),
                None => Err(VfError::errno(0, nfsstat4_NFS4ERR_INVAL)),
            },
            VfOffset::End => {
                let fh = self.resolve_tcfile(file, true)?;
                self.file_size(&fh)
            }
            _ => Err(VfError::errno(0, nfsstat4_NFS4ERR_INVAL)),
        }
    }

    /// The offset a write should use: like [`resolve_offset`](Self::resolve_offset),
    /// but descriptors opened with `O_APPEND` always write at the end of the
    /// file (one extra size query per write).
    fn write_offset(&mut self, file: &FileOperand, off: VfOffset) -> VfResult<u64> {
        let offset = self.resolve_offset(file, off)?;
        let append_fh = match file.fd() {
            Some(fd) => self
                .open_files
                .get(&fd)
                .filter(|o| o.append)
                .map(|o| o.fh.clone()),
            None => None,
        };
        match append_fh {
            Some(fh) => self.file_size(&fh),
            None => Ok(offset),
        }
    }

    /// Record the new read/write offset of an open (descriptor) file.
    pub(super) fn advance_offset(&mut self, file: &FileOperand, new_offset: u64) {
        if let Some(fd) = file.fd()
            && let Some(o) = self.open_files.get_mut(&fd)
        {
            o.cur_offset = new_offset;
        }
    }

    /// Batched writev for open (descriptor) ops.
    pub(super) fn vwrite_batch_nfs(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> VecResult<WriteResult> {
        let per = self.nfs.per_op_bytes();
        let mut ops = Vec::new();
        let mut offsets = vec![0; writes.len()];
        let mut ranges = vec![0..0; writes.len()];
        let mut handles = vec![None; writes.len()];
        let mut predecessors = vec![false; writes.len()];
        let mut positional = vec![false; writes.len()];
        let mut failures: Vec<Option<OpError>> = vec![None; writes.len()];
        let mut done = vec![false; writes.len()];
        let mut seen_handles = std::collections::HashSet::new();
        // Reject invalid operands individually before any WRITE is dispatched.
        for (index, request) in writes.iter().enumerate() {
            let prepared = (|| {
                let offset = self.write_offset(request.file(), request.offset())?;
                offset
                    .checked_add(request.data().len() as u64)
                    .ok_or_else(|| VfError::errno(None, libc::EOVERFLOW as u32))?;
                let open = request
                    .file()
                    .fd()
                    .and_then(|fd| self.open_files.get(&fd))
                    .ok_or_else(|| VfError::errno(None, ERR_EBADF))?;
                Ok::<_, VfError>((offset, open.fh.clone(), open.stateid, open.append))
            })();
            let (offset, fh, stateid, append) = match prepared {
                Ok(prepared) => prepared,
                Err(cause) => {
                    failures[index] = Some(OpError::not_attempted(cause));
                    done[index] = true;
                    continue;
                }
            };
            predecessors[index] = !seen_handles.insert(fh.clone());
            positional[index] = matches!(request.offset(), VfOffset::At(_)) && !append;
            let start = ops.len();
            for part in (0..request.data().len().max(1)).step_by(per) {
                let end = request.data().len().min(part.saturating_add(per));
                ops.push(crate::client::WriteOp {
                    fh: fh.clone(),
                    stateid,
                    offset: offset + part as u64,
                    data: &request.data()[part..end],
                });
            }
            handles[index] = Some(fh);
            offsets[index] = offset;
            ranges[index] = start..ops.len();
        }

        let mut progress = vec![0usize; ops.len()];
        let mut complete = vec![false; ops.len()];
        let mut next_chunks: Vec<_> = ranges.iter().map(|range| range.start).collect();
        let mut complete_prefix = vec![0usize; writes.len()];
        let mut activated = vec![false; writes.len()];
        let mut stable = vec![true; writes.len()];
        let byte_limit = self.nfs.max_request_arg_bytes().saturating_sub(1024);
        #[cfg(feature = "test-faults")]
        let mut wave = 0;
        while done.iter().any(|done| !done) {
            let mut selected = Vec::new();
            let mut owners = Vec::new();
            let mut bytes = 0usize;
            let mut active_files = std::collections::HashMap::new();
            'requests: for (index, request) in writes.iter().enumerate() {
                if done[index] {
                    continue;
                }
                let range = offsets[index]..offsets[index] + request.data().len() as u64;
                match active_files.entry(&handles[index]) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(WriteWaveAccess::new(range, positional[index]));
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        if !entry.get_mut().admit(range, positional[index]) {
                            // Overlaps and cursor/end/append dependencies
                            // must finish before subsequent items proceed.
                            break;
                        }
                    }
                }
                // Finish an earlier request to the same descriptor before a
                // later request can overwrite it. This also gives Cur and
                // append requests the cursor/size after the earlier write.
                if !activated[index] {
                    let offset = if predecessors[index] && !positional[index] {
                        self.write_offset(request.file(), request.offset())
                    } else {
                        Ok(offsets[index])
                    }
                    .and_then(|offset| {
                        offset
                            .checked_add(request.data().len() as u64)
                            .ok_or_else(|| VfError::errno(None, libc::EOVERFLOW as u32))
                            .map(|_| offset)
                    });
                    let offset = match offset {
                        Ok(offset) => offset,
                        Err(cause) => {
                            failures[index] = Some(OpError::not_attempted(cause));
                            done[index] = true;
                            continue;
                        }
                    };
                    offsets[index] = offset;
                    let mut part = 0;
                    for wire in ranges[index].clone() {
                        ops[wire].offset = offset + part as u64;
                        part += ops[wire].data.len();
                    }
                    activated[index] = true;
                }
                for wire in next_chunks[index]..ranges[index].end {
                    if complete[wire] {
                        continue;
                    }
                    let next = (ops[wire].data.len() - progress[wire]).saturating_add(256);
                    if !selected.is_empty() && bytes.saturating_add(next) > byte_limit {
                        // The earlier logical request is not fully selected;
                        // later files must wait, even if they fit the spare
                        // bytes. Keep ordinary complete small writes batched.
                        break 'requests;
                    }
                    bytes = bytes.saturating_add(next);
                    selected.push(wire);
                    owners.push(index);
                }
            }

            if selected.is_empty() {
                continue;
            }
            let wire_ops: Vec<_> = selected
                .iter()
                .map(|&wire| {
                    let op = &ops[wire];
                    crate::client::WriteOp {
                        fh: op.fh.clone(),
                        stateid: op.stateid,
                        offset: op.offset + progress[wire] as u64,
                        data: &op.data[progress[wire]..],
                    }
                })
                .collect();
            #[cfg(feature = "test-faults")]
            let wire_ops = {
                let limit = self.short_write_once.take();
                let mut wire_ops = wire_ops;
                if let Some(limit) = limit {
                    wire_ops[0].data = &wire_ops[0].data[..limit.min(wire_ops[0].data.len())];
                }
                wire_ops
            };
            let mut stopped = None;
            let results = match self.nfs.writev(&wire_ops) {
                Ok(results) if results.len() == selected.len() => results,
                Ok(_) => {
                    let cause = VfError::invalid_reply(None, "malformed descriptor WRITE reply");
                    stopped = Some(cause.clone());
                    selected
                        .iter()
                        .map(|_| Err(OpError::unknown(cause.clone(), None)))
                        .collect()
                }
                Err(cause) => {
                    stopped = Some(cause.clone());
                    selected
                        .iter()
                        .map(|_| Err(OpError::not_attempted(cause.clone())))
                        .collect()
                }
            };
            for ((&wire, &index), result) in selected.iter().zip(&owners).zip(results) {
                let result = result.and_then(|(count, committed)| {
                    let remaining = ops[wire].data.len() - progress[wire];
                    if count as usize > remaining {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "WRITE exceeds remaining source"),
                            None,
                        ))
                    } else if count == 0 && remaining != 0 {
                        Err(OpError::failed(VfError::errno(None, ERR_IO), None))
                    } else {
                        Ok((count, committed))
                    }
                });
                match result {
                    Ok((count, committed)) => {
                        progress[wire] += count as usize;
                        complete[wire] = progress[wire] == ops[wire].data.len();
                        stable[index] &= committed == stable_how4_FILE_SYNC4;
                    }
                    Err(error) => {
                        if error.cause().is_transport() {
                            stopped.get_or_insert_with(|| error.cause().clone());
                        }
                        // Several wire chunks may belong to one logical write.
                        // Unknown effects dominate definite failures, which in
                        // turn dominate a rejected untouched chunk.
                        let rank = |state| match state {
                            FailureState::NotAttempted => 0,
                            FailureState::Failed => 1,
                            FailureState::Unknown => 2,
                        };
                        if failures[index]
                            .as_ref()
                            .is_none_or(|prior| rank(error.state()) > rank(prior.state()))
                        {
                            failures[index] = Some(error);
                        }
                    }
                }
            }
            // Retry only acknowledged short suffixes of still-healthy inputs.
            // Failed/uncertain logical writes retain their confirmed prefix.
            for (index, request) in writes.iter().enumerate() {
                if !activated[index] || done[index] {
                    continue;
                }
                while next_chunks[index] < ranges[index].end && complete[next_chunks[index]] {
                    complete_prefix[index] += ops[next_chunks[index]].data.len();
                    next_chunks[index] += 1;
                }
                let finished = next_chunks[index] == ranges[index].end;
                let contiguous = complete_prefix[index]
                    + if finished {
                        0
                    } else {
                        progress[next_chunks[index]]
                    };
                done[index] = finished || failures[index].is_some();
                if request.offset() == VfOffset::Cur {
                    self.advance_offset(request.file(), offsets[index] + contiguous as u64);
                }
            }
            #[cfg(feature = "test-faults")]
            {
                if let Err(cause) =
                    self.inject_open_fault(OpenFaultPoint::AfterWriteChunk { chunk: wave })
                {
                    stopped.get_or_insert(cause);
                }
                wave += 1;
            }
            if let Some(cause) = stopped {
                for index in 0..writes.len() {
                    if !done[index] {
                        failures[index] = Some(OpError::not_attempted(cause.clone()));
                        done[index] = true;
                    }
                }
                break;
            }
        }
        Ok(writes
            .iter()
            .enumerate()
            .map(|(index, request)| {
                if let Some(error) = failures[index].take() {
                    let count = complete_prefix[index]
                        + if next_chunks[index] < ranges[index].end {
                            progress[next_chunks[index]]
                        } else {
                            0
                        };
                    let (cause, state, _) = error.into_parts();
                    let prefix = (count != 0).then_some(IoProgress::Write {
                        bytes_written: count,
                    });
                    return Err(match state {
                        FailureState::Unknown => OpError::unknown(cause, prefix),
                        FailureState::Failed => OpError::failed(cause, prefix),
                        FailureState::NotAttempted if count != 0 => OpError::failed(cause, prefix),
                        FailureState::NotAttempted => OpError::not_attempted(cause),
                    });
                }
                Ok(WriteResult {
                    file: request.file().clone(),
                    offset: offsets[index],
                    bytes_written: request.data().len(),
                    stable: stable[index],
                })
            })
            .collect())
    }

    /// The legacy phased path-based readv (resolve + probe + open + read +
    /// close compounds).
    pub(super) fn phased_read_results(&mut self, reads: &[ReadOp]) -> VecResult<ReadResult> {
        if let Some(index) = reads
            .iter()
            .position(|read| read.length > isize::MAX as usize)
        {
            return Err(VfError::errno(index, libc::EFBIG as u32));
        }
        let mut output: Vec<Option<OpResult<ReadResult>>> =
            (0..reads.len()).map(|_| None).collect();
        let mut files: Vec<Option<FileOperand>> = vec![None; reads.len()];
        let mut paths = Vec::new();
        let mut path_indices = Vec::new();
        let mut temporary = Vec::new();
        for (index, read) in reads.iter().enumerate() {
            if let Some(fd) = read.file.fd() {
                if self.open_files.contains_key(&fd) {
                    files[index] = Some(read.file.clone());
                } else {
                    output[index] =
                        Some(Err(OpError::not_attempted(VfError::errno(None, ERR_EBADF))));
                }
                continue;
            }
            if matches!(read.offset, VfOffset::Cur) {
                output[index] = Some(Err(OpError::not_attempted(VfError::from_kind(
                    None,
                    std::io::ErrorKind::InvalidInput,
                ))));
                continue;
            }
            match self.server_vf_path(&read.file) {
                Ok(path) => {
                    paths.push(self.visible_path(&path));
                    path_indices.push(index);
                }
                Err(cause) => output[index] = Some(Err(OpError::not_attempted(cause))),
            }
        }
        if !paths.is_empty() {
            let pending: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
            let count = paths.len();
            let results = self
                .vopen_raw_impl(&pending, &vec![libc::O_RDONLY; count], &vec![0; count])
                .unwrap_or_else(|cause| {
                    (0..count)
                        .map(|_| Err(OpError::not_attempted(cause.clone())))
                        .collect()
                });
            for (index, result) in path_indices.into_iter().zip(results) {
                match result {
                    Ok(file) => {
                        files[index] = Some(file.clone());
                        temporary.push(file);
                    }
                    Err(error) => output[index] = Some(Err(error)),
                }
            }
        }
        let indices: Vec<_> = files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| file.as_ref().map(|_| index))
            .collect();
        let requests: Vec<_> = indices
            .iter()
            .map(|&index| ReadOp {
                file: files[index].as_ref().expect("selected file").clone(),
                offset: reads[index].offset,
                length: reads[index].length,
            })
            .collect();
        let results = if requests.is_empty() {
            Ok(Vec::new())
        } else {
            self.descriptor_read_results(&requests)
        };
        match results {
            Ok(results) if results.len() == requests.len() => {
                for (index, result) in indices.into_iter().zip(results) {
                    output[index] = Some(result.map(|mut result| {
                        result.file = reads[index].file.clone();
                        result
                    }));
                }
            }
            result => {
                let invalid_count = result.is_ok();
                let cause = result.err().unwrap_or_else(|| {
                    VfError::invalid_reply(None, "invalid phased READ result count")
                });
                for index in indices {
                    output[index] = Some(Err(if invalid_count {
                        OpError::unknown(cause.clone(), None)
                    } else if !reads[index].file.is_fd() {
                        OpError::failed(cause.clone(), None)
                    } else {
                        OpError::not_attempted(cause.clone())
                    }));
                }
            }
        }
        // Keep temporary-open cleanup vectorized and retain unconfirmed closes
        // under backend ownership instead of forgetting live descriptors.
        let _ = self.close_temporary_descriptors(temporary);
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every phased READ resolved"))
            .collect())
    }

    pub(super) fn assemble_reads(
        reads: &[ReadOp],
        offsets: impl IntoIterator<Item = u64>,
        payloads: impl IntoIterator<Item = OpResult<(Vec<u8>, bool)>>,
    ) -> Vec<OpResult<ReadResult>> {
        payloads
            .into_iter()
            .zip(reads)
            .zip(offsets)
            .map(|((result, read), offset)| {
                result.map(|(data, eof)| ReadResult {
                    file: read.file.clone(),
                    offset,
                    data,
                    eof,
                })
            })
            .collect()
    }

    /// Dispatch the native WRITE vector without collapsing sibling outcomes.
    pub(super) fn write_results(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> VecResult<WriteResult> {
        self.ensure_writable(writes.len())?;
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        if writes.iter().all(|write| write.file().is_fd()) {
            return self.vwrite_batch_nfs(writes);
        }
        // Cursor/end/append offsets depend on preceding writes, which can
        // alias pathname operands. Resolve them at execution time and keep
        // mutation boundaries while retaining every group's outcomes.
        if writes.iter().any(|write| {
            matches!(write.offset(), VfOffset::Cur | VfOffset::End)
                || write
                    .file()
                    .fd()
                    .and_then(|fd| self.open_files.get(&fd))
                    .is_some_and(|open| open.append)
        }) {
            let mut output = Vec::with_capacity(writes.len());
            for group in path_write_groups(writes) {
                match self.vwrite_path_fallback_nfs(&writes[group.clone()]) {
                    Ok(results) => output.extend(results),
                    Err(cause) => {
                        output.extend(group.map(|_| Err(OpError::not_attempted(cause.clone()))))
                    }
                }
            }
            return Ok(output);
        }
        if self.merged_mode == MergedIoMode::Off {
            return self.vwrite_path_fallback_nfs(writes);
        }
        let mut path_ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        let mut errors = Vec::new();
        for (index, write) in writes.iter().enumerate() {
            let prepared = (|| {
                let offset = match write.offset() {
                    VfOffset::At(offset) => offset,
                    VfOffset::End => {
                        let fh = self.resolve_tcfile(write.file(), true)?;
                        self.file_size(&fh)?
                    }
                    VfOffset::Cur => match write.file().fd() {
                        Some(fd) => self
                            .open_files
                            .get(&fd)
                            .map(|open| open.cur_offset)
                            .ok_or_else(|| VfError::errno(None, ERR_EBADF))?,
                        None => {
                            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
                        }
                    },
                    _ => return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput)),
                };
                offset
                    .checked_add(write.data().len() as u64)
                    .ok_or_else(|| VfError::errno(None, libc::EOVERFLOW as u32))?;
                let file = match write.file().fd() {
                    Some(fd) => crate::client::FileRef::Handle(
                        self.open_files
                            .get(&fd)
                            .ok_or_else(|| VfError::errno(None, ERR_EBADF))?
                            .fh
                            .clone(),
                    ),
                    None => crate::client::FileRef::Path(
                        path_bytes(&self.server_vf_path(write.file())?).to_vec(),
                    ),
                };
                Ok::<_, VfError>(crate::client::PathWriteOp {
                    file,
                    offset,
                    data: write.data(),
                    create: write.creates() && !write.file().is_fd(),
                    truncate: write.truncates() && !write.file().is_fd(),
                    stateid: write
                        .file()
                        .fd()
                        .and_then(|fd| self.open_files.get(&fd).map(|open| open.stateid)),
                })
            })();
            match prepared {
                Ok(op) => {
                    offsets.push(op.offset);
                    path_ops.push(op);
                }
                Err(cause) => errors.push((index, OpError::not_attempted(cause))),
            }
        }
        let full = self.merged_mode == MergedIoMode::Full;
        // The common valid cohort borrows the original request vector.
        if errors.is_empty() {
            return self.vwrite_path_nfs(writes, &path_ops, &offsets, full);
        }
        let mut output: Vec<Option<OpResult<WriteResult>>> =
            (0..writes.len()).map(|_| None).collect();
        for (index, error) in errors {
            output[index] = Some(Err(error));
        }
        let indices: Vec<_> = output
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.is_none().then_some(index))
            .collect();
        let pending: Vec<_> = indices.iter().map(|&index| writes[index]).collect();
        if !pending.is_empty() {
            let results = self
                .vwrite_path_nfs(&pending, &path_ops, &offsets, full)
                .unwrap_or_else(|cause| {
                    pending
                        .iter()
                        .map(|_| Err(OpError::not_attempted(cause.clone())))
                        .collect()
                });
            for (index, result) in indices.into_iter().zip(results) {
                output[index] = Some(result);
            }
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every native WRITE resolved"))
            .collect())
    }

    /// OPEN healthy paths together, then submit one descriptor WRITE vector.
    /// Opening a sibling must never discard another input's completion.
    pub(super) fn vwrite_path_fallback_nfs(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
    ) -> VecResult<WriteResult> {
        let mut output: Vec<Option<OpResult<WriteResult>>> =
            (0..writes.len()).map(|_| None).collect();
        let mut files = vec![None; writes.len()];
        let mut paths = Vec::new();
        let mut path_indices = Vec::new();
        let mut flags = Vec::new();
        let mut temporary = Vec::new();
        for (index, write) in writes.iter().enumerate() {
            let prepared = (|| {
                if let Some(fd) = write.file().fd() {
                    if !self.open_files.contains_key(&fd) {
                        return Err(VfError::errno(None, ERR_EBADF));
                    }
                    files[index] = Some((*write.file()).clone());
                    return Ok(());
                }
                if let VfOffset::At(offset) = write.offset() {
                    offset
                        .checked_add(write.data().len() as u64)
                        .ok_or_else(|| VfError::errno(None, libc::EOVERFLOW as u32))?;
                }
                if !matches!(write.offset(), VfOffset::At(_) | VfOffset::End) {
                    return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
                }
                if matches!(write.file(), FileOperand::Cwd) {
                    return Err(VfError::errno(None, ERR_ISDIR));
                }
                let path = self.server_vf_path(write.file())?;
                paths.push(self.visible_path(&path));
                path_indices.push(index);
                flags.push(
                    libc::O_RDWR
                        | if write.creates() { libc::O_CREAT } else { 0 }
                        | if write.truncates() { libc::O_TRUNC } else { 0 },
                );
                Ok(())
            })();
            if let Err(cause) = prepared {
                output[index] = Some(Err(OpError::not_attempted(cause)));
            }
        }
        if !paths.is_empty() {
            let pending: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
            let results = self
                .vopen_raw_impl(&pending, &flags, &vec![0o644; pending.len()])
                .unwrap_or_else(|cause| {
                    pending
                        .iter()
                        .map(|_| Err(OpError::not_attempted(cause.clone())))
                        .collect()
                });
            for (index, result) in path_indices.into_iter().zip(results) {
                match result {
                    Ok(file) => {
                        files[index] = Some(file.clone());
                        temporary.push(file);
                    }
                    Err(error) => output[index] = Some(Err(error)),
                }
            }
        }
        let indices: Vec<_> = files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| file.as_ref().map(|_| index))
            .collect();
        let requests: Vec<_> = indices
            .iter()
            .map(|&index| {
                WriteOp::new(
                    files[index].as_ref().expect("selected file"),
                    writes[index].offset(),
                    writes[index].data(),
                )
            })
            .collect();
        if !requests.is_empty() {
            match self.vwrite_batch_nfs(&requests) {
                Ok(results) if results.len() == requests.len() => {
                    for (index, result) in indices.into_iter().zip(results) {
                        output[index] = Some(
                            result
                                .map(|mut result| {
                                    result.file = (*writes[index].file()).clone();
                                    result
                                })
                                .map_err(|error| {
                                    if !writes[index].file().is_fd()
                                        && error.state() == FailureState::NotAttempted
                                    {
                                        let (cause, _, progress) = error.into_parts();
                                        OpError::failed(cause, progress)
                                    } else {
                                        error
                                    }
                                }),
                        );
                    }
                }
                result => {
                    let invalid_count = result.is_ok();
                    let cause = result.err().unwrap_or_else(|| {
                        VfError::invalid_reply(None, "invalid phased WRITE result count")
                    });
                    for index in indices {
                        output[index] = Some(Err(if invalid_count {
                            OpError::unknown(cause.clone(), None)
                        } else if !writes[index].file().is_fd() {
                            OpError::failed(cause.clone(), None)
                        } else {
                            OpError::not_attempted(cause.clone())
                        }));
                    }
                }
            }
        }
        let _ = self.close_temporary_descriptors(temporary);
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every phased WRITE resolved"))
            .collect())
    }

    pub(super) fn vwrite_path_nfs(
        &mut self,
        writes: &[WriteOp<&FileOperand, &[u8]>],
        path_ops: &[crate::client::PathWriteOp<'_>],
        offsets: &[u64],
        full: bool,
    ) -> VecResult<WriteResult> {
        let mut results = Vec::new();
        let mut first = 0;
        while first < writes.len() {
            if self.merged_mode == MergedIoMode::Off {
                for group in path_write_groups(&writes[first..]) {
                    let batch = &writes[first + group.start..first + group.end];
                    results.extend(
                        self.vwrite_path_fallback_nfs(batch)
                            .unwrap_or_else(|cause| {
                                batch
                                    .iter()
                                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                                    .collect()
                            }),
                    );
                }
                break;
            }
            let (outcome, repair) = match self.nfs.writev_path_run(
                &path_ops[first..],
                full && self.merged_mode == MergedIoMode::Full,
                true,
            ) {
                Ok(outcome) => outcome,
                Err(cause) if results.is_empty() => return Err(cause),
                Err(cause) => {
                    results.extend(
                        (first..writes.len()).map(|_| Err(OpError::not_attempted(cause.clone()))),
                    );
                    break;
                }
            };
            self.close_path_opens(&outcome.opened);
            if outcome.close_failed.is_some() {
                self.merged_mode = MergedIoMode::OpenWrite;
            }
            if outcome.results.iter().any(|result| {
                result.as_ref().err().is_some_and(|error| {
                    error.cause().status().is_some_and(|status| {
                        matches!(status,
                    vfsi_core::StatusCode::Nfs(status) if crate::client::is_stateid_error(status))
                    })
                })
            }) {
                self.merged_mode = MergedIoMode::Off;
            }
            let take = repair
                .map(|repair| repair.index)
                .unwrap_or(outcome.results.len());
            let confirmed = repair.map_or(0, |repair| {
                match outcome.results[repair.index]
                    .as_ref()
                    .err()
                    .and_then(OpError::progress)
                {
                    Some(IoProgress::Write { bytes_written }) => *bytes_written,
                    _ => 0,
                }
            });
            results.extend(outcome.results.into_iter().take(take).enumerate().map(
                |(index, result)| {
                    let index = first + index;
                    result.map(|(bytes_written, committed)| WriteResult {
                        file: (*writes[index].file()).clone(),
                        offset: offsets[index],
                        bytes_written,
                        stable: committed == stable_how4_FILE_SYNC4,
                    })
                },
            ));
            let Some(repair) = repair else { break };
            let index = first + repair.index;
            let original = &writes[index];
            let pending = retry_path_write(original, repair.kind, offsets[index], confirmed);
            // OPEN already ran for a stateid rejection. Never repeat CREATE or
            // a completed truncate, and never replay acknowledged payload bytes.
            let repaired = self
                .vwrite_path_fallback_nfs(&[pending])
                .unwrap_or_else(|cause| vec![Err(OpError::failed(cause, None))])
                .pop()
                .expect("one repaired write");
            results.push(
                repaired
                    .map(|mut result| {
                        result.offset = offsets[index];
                        result.bytes_written += confirmed;
                        result.stable &= repair.prefix_stable;
                        result
                    })
                    .map_err(|error| {
                        vfsi_core::internal::add_write_prefix(
                            error,
                            confirmed,
                            original.data().len(),
                        )
                    }),
            );
            first = index + 1;
        }
        for ((write, &offset), result) in writes.iter().zip(offsets).zip(&results) {
            if write.offset() == VfOffset::Cur && write.file().is_fd() {
                let confirmed = match result {
                    Ok(result) => result.bytes_written,
                    Err(error) => match error.progress() {
                        Some(IoProgress::Write { bytes_written }) => *bytes_written,
                        _ => 0,
                    },
                };
                if result.is_ok() || confirmed != 0 {
                    self.advance_offset(write.file(), offset + confirmed as u64);
                }
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod path_write_tests {
    use super::*;

    #[test]
    fn yielded_path_retry_preserves_only_proven_unexecuted_flags_and_borrowed_bytes() {
        use crate::client::PathWriteRepairKind;
        let file = FileOperand::from_path("/file");
        let data = b"abcde";
        let original = WriteOp::new(&file, VfOffset::At(10), data.as_slice())
            .with_creation()
            .with_truncate();
        for (kind, confirmed, create, truncate) in [
            (PathWriteRepairKind::Symlink, 0, true, true),
            (PathWriteRepairKind::Symlink, 2, false, false),
            (PathWriteRepairKind::WriteStateid, 0, false, false),
            (PathWriteRepairKind::WriteStateid, 2, false, false),
            (PathWriteRepairKind::TruncateStateid, 0, false, true),
        ] {
            let pending = retry_path_write(&original, kind, 10, confirmed);
            assert_eq!(pending.offset(), VfOffset::At(10 + confirmed as u64));
            assert_eq!(pending.data(), &data[confirmed..]);
            assert_eq!(pending.data().as_ptr(), data[confirmed..].as_ptr());
            assert_eq!((pending.creates(), pending.truncates()), (create, truncate));
        }
    }

    #[test]
    fn path_write_retries_short_counts_and_rejects_zero_progress() {
        let mut calls = Vec::new();
        write_all_at(&mut calls, 0, 10, b"abcdefg", |calls, offset, suffix| {
            let count = suffix.len().min(3);
            calls.push((offset, suffix.to_vec()));
            Ok(count)
        })
        .unwrap();
        assert_eq!(
            calls,
            vec![
                (10, b"abcdefg".to_vec()),
                (13, b"defg".to_vec()),
                (16, b"g".to_vec()),
            ]
        );

        let mut attempts = 0;
        let error = write_all_at(&mut attempts, 0, 0, b"x", |attempts, _, _| {
            *attempts += 1;
            Ok(0)
        })
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(error.raw_code(), ERR_IO);
    }
}

#[cfg(test)]
mod descriptor_outcome_tests {
    use super::*;

    #[test]
    fn independent_cursor_reads_share_a_wave_until_a_descriptor_repeats() {
        let reads = [
            ReadOp::from_fd(1, VfOffset::Cur, 2),
            ReadOp::from_fd(1, VfOffset::At(0), 2),
            ReadOp::from_fd(2, VfOffset::Cur, 2),
            ReadOp::from_path("/path", VfOffset::At(0), 2),
            ReadOp::from_fd(1, VfOffset::Cur, 2),
        ];
        assert_eq!(independent_read_prefix(&reads), 4);
        assert_eq!(independent_read_prefix(&reads[..4]), 4);
        assert_eq!(independent_read_prefix(&reads[4..]), 1);
        assert_eq!(independent_read_prefix(&[]), 0);
    }

    #[cfg(feature = "test-faults")]
    #[test]
    fn live_dependent_cursor_reads_publish_progress_before_later_inputs() {
        use std::sync::Arc;
        use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["full", "openwrite", "off"] {
            for owned in [false, true] {
                let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
                fs.set_merged_mode(mode);
                let path = PathBuf::from(format!(
                    "/vfsi-dependent-read-{}-{mode}-{owned}",
                    std::process::id()
                ));
                let file = fs
                    .open_raw_impl(&path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
                    .unwrap();
                fs.write_raw_impl(&file, 0, b"abcdef").unwrap();
                // A path and an invalid descriptor between cursor reads exercise
                // both merged and phased planning without dropping siblings.
                let reads = [
                    ReadOp::new(file.clone(), VfOffset::Cur, 2),
                    ReadOp::at(FileOperand::from_os_path(&path), 4, 2),
                    ReadOp::at(FileOperand::from_fd(i32::MAX), 0, 2),
                    ReadOp::new(file.clone(), VfOffset::Cur, 2),
                    ReadOp::new(file.clone(), VfOffset::Cur, 4),
                    ReadOp::new(file.clone(), VfOffset::Cur, 2),
                ];
                if owned {
                    let results = fs.read_results(&reads).unwrap();
                    assert_eq!(results.len(), reads.len());
                    for (index, offset, expected) in [
                        (0, 0, b"ab".as_slice()),
                        (1, 4, b"ef"),
                        (3, 2, b"cd"),
                        (4, 4, b"ef"),
                        (5, 6, b""),
                    ] {
                        let result = results[index].as_ref().unwrap();
                        assert_eq!((result.offset, result.data.as_slice()), (offset, expected));
                    }
                    assert!(results[4].as_ref().unwrap().eof);
                    assert!(results[5].as_ref().unwrap().eof);
                    assert_eq!(
                        results[2].as_ref().unwrap_err().state(),
                        FailureState::NotAttempted
                    );
                } else {
                    let mut storage: Vec<_> =
                        reads.iter().map(|read| vec![99; read.length]).collect();
                    let mut buffers: Vec<_> = storage.iter_mut().map(Vec::as_mut_slice).collect();
                    let results = fs.read_into_results(&reads, &mut buffers).unwrap();
                    assert_eq!(results.len(), reads.len());
                    for (index, offset, expected) in [
                        (0, 0, b"ab".as_slice()),
                        (1, 4, b"ef"),
                        (3, 2, b"cd"),
                        (4, 4, b"ef"),
                        (5, 6, b""),
                    ] {
                        let result = results[index].as_ref().unwrap();
                        assert_eq!((result.offset, result.bytes_read), (offset, expected.len()));
                        assert_eq!(&storage[index][..result.bytes_read], expected);
                    }
                    assert!(results[4].as_ref().unwrap().eof);
                    assert!(results[5].as_ref().unwrap().eof);
                    assert_eq!(
                        results[2].as_ref().unwrap_err().state(),
                        FailureState::NotAttempted
                    );
                }
                assert_eq!(fs.open_files[&file.fd().unwrap()].cur_offset, 6);
                fs.seek_raw_impl(&file, 0, vfsi_core::SeekFrom::Set)
                    .unwrap();
                let script = Arc::new(FaultScript::one(
                    OpenFaultPoint::BeforeReadRepair { index: 0 },
                    VfError::errno(None, libc::EACCES as u32),
                ));
                fs.set_fault_injector(script.clone());
                fs.test_short_read_for_request_once(0, 1);
                let dependent = [
                    ReadOp::new(file.clone(), VfOffset::Cur, 2),
                    ReadOp::new(file.clone(), VfOffset::Cur, 2),
                ];
                if owned {
                    let results = fs.read_results(&dependent).unwrap();
                    let error = results[0].as_ref().unwrap_err();
                    assert_eq!(error.state(), FailureState::Failed);
                    let Some(IoProgress::Read(prefix)) = error.progress() else {
                        panic!("missing confirmed read prefix")
                    };
                    assert_eq!(prefix.data().unwrap(), b"a");
                    let second = results[1].as_ref().unwrap();
                    assert_eq!(
                        (second.offset, second.data.as_slice()),
                        (1, b"bc".as_slice())
                    );
                } else {
                    let mut first = [99; 2];
                    let mut second = [99; 2];
                    let results = fs
                        .read_into_results(&dependent, &mut [&mut first, &mut second])
                        .unwrap();
                    let error = results[0].as_ref().unwrap_err();
                    assert_eq!(error.state(), FailureState::Failed);
                    let Some(IoProgress::Read(prefix)) = error.progress() else {
                        panic!("missing confirmed read prefix")
                    };
                    assert_eq!(prefix.bytes_read(), 1);
                    assert_eq!(first[0], b"a"[0]);
                    assert_eq!((results[1].as_ref().unwrap().offset, second), (1, *b"bc"));
                }
                assert!(script.is_consumed());
                assert_eq!(fs.open_files[&file.fd().unwrap()].cur_offset, 3);
                fs.seek_raw_impl(&file, 0, vfsi_core::SeekFrom::Set)
                    .unwrap();
                let recover = Arc::new(FaultScript::one(
                    OpenFaultPoint::BeforeReadRepair { index: 0 },
                    VfError::transport(None, "injected cursor read repair disconnect"),
                ));
                fs.set_fault_injector(recover.clone());
                fs.test_short_read_for_request_once(0, 1);
                if owned {
                    let results = fs.read_results(&dependent).unwrap();
                    assert_eq!(results[0].as_ref().unwrap().data, b"ab");
                    let second = results[1].as_ref().unwrap();
                    assert_eq!(
                        (second.offset, second.data.as_slice()),
                        (2, b"cd".as_slice())
                    );
                } else {
                    let mut first = [99; 2];
                    let mut second = [99; 2];
                    let results = fs
                        .read_into_results(&dependent, &mut [&mut first, &mut second])
                        .unwrap();
                    assert_eq!(results[0].as_ref().unwrap().bytes_read, 2);
                    assert_eq!(first, *b"ab");
                    assert_eq!((results[1].as_ref().unwrap().offset, second), (2, *b"cd"));
                }
                assert!(recover.is_consumed());
                assert_eq!(fs.open_files[&file.fd().unwrap()].cur_offset, 4);
                // Positional reads on the same object remain independent and
                // retain the normal single-compound fast path.
                let _ = crate::compound::thread_compound_stats();
                let results = fs
                    .read_results(&[
                        ReadOp::at(file.clone(), 0, 2),
                        ReadOp::at(file.clone(), 2, 2),
                    ])
                    .unwrap();
                assert!(results.iter().all(Result::is_ok));
                assert_eq!(crate::compound::thread_compound_stats().0, 1);
                assert_eq!(fs.open_files[&file.fd().unwrap()].cur_offset, 4);
                fs.close_impl(&file).unwrap();
                fs.remove_impl(&path, false).unwrap();
                fs.shutdown().unwrap();
            }
        }
    }

    #[test]
    fn chunk_failure_merging_retains_first_cause_and_any_uncertainty() {
        let mut failure = None;
        merge_descriptor_failure(
            &mut failure,
            OpError::failed(VfError::errno(None, libc::EACCES as u32), None),
        );
        merge_descriptor_failure(
            &mut failure,
            OpError::unknown(VfError::transport(None, "later lost reply"), None),
        );
        let error = failure.unwrap();
        assert_eq!(error.cause().raw_code(), libc::EACCES as u32);
        assert_eq!(error.state(), FailureState::Unknown);
    }

    #[cfg(feature = "test-faults")]
    #[test]
    fn live_descriptor_outcomes_retain_siblings_and_short_repair_prefix() {
        use std::sync::Arc;
        use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
        use vfsi_sync::test_support::borrow_writes;
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for owned in [false, true] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            let path = format!("/vfsi-rich-descriptor-{}-{owned}", std::process::id());
            fs.vwrite_impl(&borrow_writes(&[WriteOp::from_path(
                &path,
                VfOffset::At(0),
                b"abcdefgh".to_vec(),
            )
            .with_creation()
            .with_truncate()]))
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap();
            let file = fs
                .open_raw_impl(Path::new(&path), libc::O_RDONLY, 0)
                .unwrap();
            let script = Arc::new(FaultScript::one(
                OpenFaultPoint::BeforeReadRepair { index: 2 },
                VfError::errno(None, libc::EACCES as u32),
            ));
            fs.set_fault_injector(script.clone());
            fs.test_short_read_for_request_once(2, 2);
            let reads = [
                ReadOp::at(file.clone(), 0, 4),
                ReadOp::at(FileOperand::from_fd(i32::MAX), 0, 4),
                ReadOp::at(file.clone(), 0, 4),
                ReadOp::at(file.clone(), 4, 4),
            ];
            let mut storage = [[99; 4]; 4];
            let _ = crate::compound::thread_compound_stats();
            if owned {
                let results = fs.descriptor_read_results(&reads).unwrap();
                assert_eq!(results.len(), reads.len());
                assert_eq!(results[0].as_ref().unwrap().data, b"abcd");
                assert_eq!(results[3].as_ref().unwrap().data, b"efgh");
                assert_eq!(
                    results[1].as_ref().unwrap_err().state(),
                    FailureState::NotAttempted
                );
                let error = results[2].as_ref().unwrap_err();
                assert_eq!(error.state(), FailureState::Failed);
                assert_eq!(error.cause().raw_code(), libc::EACCES as u32);
                let Some(IoProgress::Read(prefix)) = error.progress() else {
                    panic!("missing prefix")
                };
                assert_eq!(prefix.data().unwrap(), b"ab");
                assert_eq!(prefix.offset(), 0);
            } else {
                let mut buffers: Vec<_> =
                    storage.iter_mut().map(|buffer| &mut buffer[..]).collect();
                let results = fs
                    .descriptor_read_into_results(&reads, &mut buffers)
                    .unwrap();
                assert_eq!(results.len(), reads.len());
                assert_eq!(results[0].as_ref().unwrap().bytes_read, 4);
                assert_eq!(results[3].as_ref().unwrap().bytes_read, 4);
                assert_eq!(
                    results[1].as_ref().unwrap_err().state(),
                    FailureState::NotAttempted
                );
                let error = results[2].as_ref().unwrap_err();
                assert_eq!(error.state(), FailureState::Failed);
                let Some(IoProgress::Read(prefix)) = error.progress() else {
                    panic!("missing prefix")
                };
                assert!(prefix.is_buffered());
                assert_eq!(prefix.bytes_read(), 2);
                assert_eq!(storage, [*b"abcd", [99; 4], [b'a', b'b', 99, 99], *b"efgh"]);
            }
            assert!(script.is_consumed());
            assert_eq!(
                crate::compound::thread_compound_stats().0,
                1,
                "valid siblings must share one descriptor compound"
            );
            let _ = crate::compound::thread_compound_stats();
            assert!(fs.descriptor_read_results(&[]).unwrap().is_empty());
            assert!(
                fs.descriptor_read_into_results(&[], &mut [])
                    .unwrap()
                    .is_empty()
            );
            let invalid = [ReadOp::at(FileOperand::from_fd(i32::MAX), 0, 1)];
            assert_eq!(
                fs.descriptor_read_results(&invalid).unwrap()[0]
                    .as_ref()
                    .unwrap_err()
                    .state(),
                FailureState::NotAttempted
            );
            let mismatched = [ReadOp::new(file.clone(), VfOffset::End, 2)];
            assert!(
                fs.descriptor_read_into_results(&mismatched, &mut [&mut [0u8; 1]])
                    .is_err()
            );
            assert_eq!(
                crate::compound::thread_compound_stats().0,
                0,
                "empty/invalid/shape-rejected vectors must not dispatch RPCs"
            );
            fs.close_impl(&file).unwrap();
            fs.unlink_impl(Path::new(&path)).unwrap();
        }
    }
    #[cfg(feature = "test-faults")]
    #[test]
    fn live_phased_reads_keep_missing_symlink_and_descriptor_siblings_batched() {
        use std::sync::Arc;
        use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
        use vfsi_sync::test_support::borrow_writes;
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let directory = format!("/vfsi-phased-read-outcomes-{}", std::process::id());
        fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
        let a = format!("{directory}/a");
        let b = format!("{directory}/b");
        let link = format!("{directory}/link");
        fs.vwrite_impl(&borrow_writes(&[
            WriteOp::from_path(&a, VfOffset::At(0), b"abcdefgh".to_vec())
                .with_creation()
                .with_truncate(),
            WriteOp::from_path(&b, VfOffset::At(0), b"ijklmnop".to_vec())
                .with_creation()
                .with_truncate(),
        ]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
        fs.vsymlink_impl(&[Path::new(&b)], &[Path::new(&link)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
            })
            .unwrap();
        let file = fs.open_raw_impl(Path::new(&a), libc::O_RDONLY, 0).unwrap();
        let baseline = fs.open_files.len();
        let reads = [
            ReadOp::from_path(&a, VfOffset::At(0), 4),
            ReadOp::from_path(&format!("{directory}/missing"), VfOffset::At(0), 4),
            ReadOp::at(file.clone(), 2, 3),
            ReadOp::from_path(&link, VfOffset::At(0), 4),
            ReadOp::at(FileOperand::from_fd(i32::MAX), 0, 1),
            ReadOp::from_path(&b, VfOffset::End, 2),
        ];
        let outcomes = fs.phased_read_results(&reads).unwrap();
        assert_eq!(outcomes.len(), reads.len());
        assert_eq!(outcomes[0].as_ref().unwrap().data, b"abcd");
        assert_eq!(outcomes[2].as_ref().unwrap().data, b"cde");
        assert_eq!(outcomes[3].as_ref().unwrap().data, b"ijkl");
        assert_eq!(
            outcomes[1].as_ref().unwrap_err().cause().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            outcomes[4].as_ref().unwrap_err().state(),
            FailureState::NotAttempted
        );
        assert_eq!(outcomes[5].as_ref().unwrap().offset, 8);
        assert!(outcomes[5].as_ref().unwrap().data.is_empty());
        assert!(outcomes[5].as_ref().unwrap().eof);
        assert_eq!(fs.open_files.len(), baseline);
        let many: Vec<_> = (0..31)
            .map(|_| ReadOp::from_path(&a, VfOffset::At(0), 4))
            .collect();
        let _ = crate::compound::thread_compound_stats();
        assert!(
            fs.phased_read_results(&many)
                .unwrap()
                .iter()
                .all(Result::is_ok)
        );
        let compounds = crate::compound::thread_compound_stats().0;
        assert!(
            compounds < 16,
            "phased reads must remain batched, got {compounds}"
        );
        assert_eq!(fs.open_files.len(), baseline);
        let _ = crate::compound::thread_compound_stats();
        assert!(fs.phased_read_results(&[]).unwrap().is_empty());
        assert!(
            fs.phased_read_results(&[ReadOp::at(FileOperand::from_fd(i32::MAX), 0, 1)])
                .unwrap()[0]
                .is_err()
        );
        assert_eq!(crate::compound::thread_compound_stats().0, 0);
        let close = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeCloseDispatch { index: 0 },
            VfError::transport(None, "temporary close not submitted"),
        ));
        fs.set_fault_injector(close.clone());
        let data = fs.phased_read_results(&reads[..1]).unwrap();
        assert_eq!(data[0].as_ref().unwrap().data, b"abcd");
        assert!(close.is_consumed());
        assert_eq!(fs.open_files.len(), baseline);
        assert_eq!(fs.deferred_descriptor_closes.len(), 1);
        fs.drain_deferred_descriptor_closes().unwrap();
        assert!(fs.deferred_descriptor_closes.is_empty());
        fs.close_impl(&file).unwrap();
        fs.remove_impl(Path::new(&directory), true).unwrap();
    }
    #[cfg(feature = "test-faults")]
    #[test]
    fn live_native_read_hook_preserves_outcomes_for_every_merged_mode() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        for mode in ["off", "openwrite", "full"] {
            fs.set_merged_mode(mode);
            vfsi_sync::test_support::read_vector_outcomes(
                &mut fs,
                Path::new(&format!(
                    "/vfsi-native-read-outcomes-{}-{mode}",
                    std::process::id()
                )),
            );
        }
    }
}

#[cfg(all(test, feature = "test-faults"))]
mod phased_write_tests {
    use super::*;
    use std::sync::Arc;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};

    #[test]
    fn live_full_data_failure_retains_exact_owner_after_unlink() {
        use crate::client::{FileRef, PathReadOp, PathWriteOp};
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for write in [false, true] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            let directory = format!("/vfsi-full-data-owner-{}-{write}", std::process::id());
            fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
            let paths = [format!("{directory}/bad"), format!("{directory}/good")];
            for path in &paths {
                let file = fs
                    .open_raw_impl(
                        Path::new(path),
                        libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                        0o600,
                    )
                    .unwrap();
                fs.write_raw_impl(&file, 0, b"original").unwrap();
                fs.close_impl(&file).unwrap();
            }
            let _ = crate::compound::thread_compound_stats();
            if write {
                let ops: Vec<_> = paths
                    .iter()
                    .map(|path| PathWriteOp {
                        file: FileRef::Path(path.as_bytes().to_vec()),
                        offset: 0,
                        data: b"original",
                        create: false,
                        truncate: false,
                        stateid: None,
                    })
                    .collect();
                let healthy = fs.nfs.writev_path_compound(&ops, true).unwrap();
                assert!(healthy.results.iter().all(Result::is_ok));
                assert_eq!(
                    healthy.opened.capacity(),
                    0,
                    "healthy full compounds allocate no cleanup owners"
                );
            } else {
                let ops: Vec<_> = paths
                    .iter()
                    .map(|path| PathReadOp {
                        file: FileRef::Path(path.as_bytes().to_vec()),
                        offset: 0,
                        count: 8,
                        stateid: None,
                    })
                    .collect();
                let healthy = fs.nfs.readv_path_compound(&ops, true).unwrap();
                assert!(healthy.results.iter().all(Result::is_ok));
                assert_eq!(
                    healthy.opened.capacity(),
                    0,
                    "healthy full compounds allocate no cleanup owners"
                );
            }
            assert_eq!(
                crate::compound::thread_compound_stats().0,
                1,
                "healthy full cohorts keep one data-and-close RPC"
            );
            let opened = if write {
                fs.nfs.test_fail_merged_write_once(0, 0);
                let ops: Vec<_> = paths
                    .iter()
                    .map(|path| PathWriteOp {
                        file: FileRef::Path(path.as_bytes().to_vec()),
                        offset: 0,
                        data: b"new",
                        create: false,
                        truncate: false,
                        stateid: None,
                    })
                    .collect();
                let outcome = fs.nfs.writev_path_compound(&ops, true).unwrap();
                assert_eq!(
                    outcome.results[0].as_ref().unwrap_err().state(),
                    FailureState::Failed
                );
                assert_eq!(outcome.results[1].as_ref().unwrap().0, 3);
                outcome.opened
            } else {
                fs.nfs.fail_merged_read_once = Some(0);
                let ops: Vec<_> = paths
                    .iter()
                    .map(|path| PathReadOp {
                        file: FileRef::Path(path.as_bytes().to_vec()),
                        offset: 0,
                        count: 8,
                        stateid: None,
                    })
                    .collect();
                let outcome = fs.nfs.readv_path_compound(&ops, true).unwrap();
                assert_eq!(
                    outcome.results[0].as_ref().unwrap_err().state(),
                    FailureState::Failed
                );
                assert_eq!(outcome.results[1].as_ref().unwrap().0, b"original");
                outcome.opened
            };
            assert_eq!(
                opened.len(),
                1,
                "retain failed input only; healthy inline CLOSE retires its owner"
            );
            fs.unlink_impl(Path::new(&paths[0])).unwrap();
            // The original pathname no longer resolves. Cleanup must use the
            // exact GETFH returned immediately after its successful OPEN.
            fs.nfs.close_path(&opened[0].0, &opened[0].1).unwrap();
            fs.remove_impl(Path::new(&directory), true).unwrap();
        }
    }

    #[test]
    fn live_full_alias_close_retires_an_earlier_failed_open() {
        use crate::client::{FileRef, PathWriteOp};
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let directory = format!("/vfsi-full-alias-owner-{}", std::process::id());
        fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
        let path = PathBuf::from(format!("{directory}/target"));
        let alias = PathBuf::from(format!("{directory}/alias"));
        let file = fs
            .open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
            .unwrap();
        fs.close_impl(&file).unwrap();
        fs.hard_link_impl(&path, &alias).unwrap();
        fs.nfs.test_fail_merged_write_once(0, 0);
        let paths = [&path, &alias];
        let ops: Vec<_> = paths
            .iter()
            .map(|path| PathWriteOp {
                file: FileRef::Path(path.as_os_str().as_encoded_bytes().to_vec()),
                offset: 0,
                data: b"healthy",
                create: false,
                truncate: false,
                stateid: None,
            })
            .collect();
        let _ = crate::compound::thread_compound_stats();
        let outcome = fs.nfs.writev_path_compound(&ops, true).unwrap();
        assert_eq!(
            outcome.results[0].as_ref().unwrap_err().state(),
            FailureState::Failed
        );
        assert_eq!(outcome.results[1].as_ref().unwrap().0, 7);
        assert!(
            outcome.opened.is_empty(),
            "a successful CLOSE of the shared owner retires its older stateid too"
        );
        assert_eq!(
            crate::compound::thread_compound_stats().0,
            2,
            "only the failed data compound and untouched suffix are submitted"
        );
        fs.remove_impl(Path::new(&directory), true).unwrap();
    }

    #[test]
    fn live_inline_close_failure_resumes_the_untouched_input() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for write in [false, true] {
            for defer in [false, true] {
                let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
                fs.set_merged_mode("full");
                let directory =
                    format!("/vfsi-inline-close-{}-{write}-{defer}", std::process::id());
                fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
                let paths: Vec<_> = (0..3)
                    .map(|i| PathBuf::from(format!("{directory}/{i}")))
                    .collect();
                if !write {
                    for path in &paths {
                        let file = fs
                            .open_raw_impl(
                                path,
                                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                                0o600,
                            )
                            .unwrap();
                        fs.write_raw_impl(&file, 0, b"original").unwrap();
                        fs.close_impl(&file).unwrap();
                    }
                }
                let files: Vec<_> = paths
                    .iter()
                    .map(|path| FileOperand::from_os_path(path))
                    .collect();
                let fault = Arc::new(FaultScript::one(
                    OpenFaultPoint::BeforePathCloseBatch,
                    VfError::transport(None, "defer exact owner after inline CLOSE failure"),
                ));
                if defer {
                    fs.set_fault_injector(fault.clone());
                }
                fs.nfs.fail_inline_path_close_once = true;
                let _ = crate::compound::thread_compound_stats();
                if write {
                    let ops: Vec<_> = files
                        .iter()
                        .map(|file| {
                            WriteOp::at(file, 0, b"written-once".as_slice())
                                .with_creation()
                                .with_truncate()
                        })
                        .collect();
                    let results = fs.write_results(&ops).unwrap();
                    assert_eq!(results.len(), 3);
                    assert!(results.iter().all(|result| {
                        result.as_ref().is_ok_and(|value| value.bytes_written == 12)
                    }));
                } else {
                    let requests: Vec<_> = files
                        .iter()
                        .map(|file| ReadOp::at(file.clone(), 0, 8))
                        .collect();
                    let results = fs.read_results(&requests).unwrap();
                    assert_eq!(results.len(), 3);
                    assert!(results.iter().all(|result| {
                        result.as_ref().is_ok_and(|value| value.data == b"original")
                    }));
                }
                let stats = crate::compound::thread_compound_stats();
                assert!(!fs.nfs.fail_inline_path_close_once);
                assert_eq!(
                    stats.0,
                    if defer { 3 } else { 4 },
                    "two data compounds and cleanup only; completed I/O must not be replayed"
                );
                if defer {
                    assert!(fault.is_consumed());
                }
                assert_eq!(fs.nfs.deferred_path_close_count(), usize::from(defer));
                fs.set_fault_injector(Arc::new(FaultScript::new([])));
                fs.nfs.shutdown().unwrap();
                assert_eq!(fs.nfs.deferred_path_close_count(), 0);
                // A fresh session can still inspect and remove every created file.
                let mut cleanup = NfsVecFs::connect_minor(&server, minor).unwrap();
                for path in &paths {
                    let file = cleanup.open_raw_impl(path, libc::O_RDONLY, 0).unwrap();
                    assert_eq!(
                        cleanup.read_raw_impl(&file, 0, 32).unwrap(),
                        if write {
                            b"written-once".as_slice()
                        } else {
                            b"original".as_slice()
                        }
                    );
                    cleanup.close_impl(&file).unwrap();
                }
                cleanup.remove_impl(Path::new(&directory), true).unwrap();
            }
        }
    }

    #[test]
    fn live_phased_writes_keep_healthy_siblings_and_cleanup_owners() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["off", "openwrite", "full"] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            fs.set_merged_mode(mode);
            let directory = format!("/vfsi-phased-writes-{}-{mode}", std::process::id());
            fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
            let first = PathBuf::from(format!("{directory}/first"));
            let last = PathBuf::from(format!("{directory}/last"));
            let overflow = PathBuf::from(format!("{directory}/overflow"));
            let missing = FileOperand::from_path(&format!("{directory}/missing"));
            let first_file = fs
                .open_raw_impl(&first, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
                .unwrap();
            let baseline = fs.open_files.len();
            let bad = FileOperand::from_fd(i32::MAX);
            let last_file = FileOperand::from_os_path(&last);
            let overflow_file = FileOperand::from_os_path(&overflow);
            let writes = [
                WriteOp::new(&first_file, VfOffset::At(0), b"first".as_slice()),
                WriteOp::new(&bad, VfOffset::At(0), b"bad".as_slice()),
                WriteOp::new(&missing, VfOffset::At(0), b"missing".as_slice()),
                WriteOp::new(&overflow_file, VfOffset::At(u64::MAX), b"bad".as_slice())
                    .with_creation()
                    .with_truncate(),
                WriteOp::new(&last_file, VfOffset::At(0), b"last".as_slice())
                    .with_creation()
                    .with_truncate(),
            ];
            let close = Arc::new(FaultScript::one(
                OpenFaultPoint::BeforeCloseDispatch { index: 0 },
                VfError::transport(None, "temporary close not submitted"),
            ));
            fs.set_fault_injector(close.clone());
            let results = fs.write_results(&writes).unwrap();
            assert_eq!(results.len(), writes.len());
            for (index, count) in [(0, 5), (4, 4)] {
                let result = results[index].as_ref().unwrap();
                assert_eq!(result.bytes_written, count);
                assert_eq!(&result.file, writes[index].file());
            }
            for index in [1, 3] {
                let error = results[index].as_ref().unwrap_err();
                assert_eq!(error.state(), FailureState::NotAttempted);
                assert_eq!(error.cause().index(), None);
                assert!(error.progress().is_none());
            }
            assert_eq!(
                results[2].as_ref().unwrap_err().cause().kind(),
                std::io::ErrorKind::NotFound
            );
            assert_eq!(close.is_consumed(), mode == "off");
            assert_eq!(fs.open_files.len(), baseline);
            assert_eq!(
                fs.deferred_descriptor_closes.len(),
                usize::from(mode == "off")
            );
            fs.set_fault_injector(Arc::new(FaultScript::new([])));
            fs.drain_deferred_descriptor_closes().unwrap();
            assert!(fs.deferred_descriptor_closes.is_empty());
            assert!(!fs.exists_impl(&overflow).unwrap());
            assert_eq!(fs.read_raw_impl(&first_file, 0, 16).unwrap(), b"first");
            assert_eq!(fs.read_raw_impl(&last_file, 0, 16).unwrap(), b"last");
            fs.close_impl(&first_file).unwrap();
            fs.remove_impl(Path::new(&directory), true).unwrap();
        }
    }
    #[test]
    fn live_symlink_write_repair_precedes_later_alias_mutations() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["off", "openwrite", "full"] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            fs.set_merged_mode(mode);
            let directory = format!("/vfsi-write-symlink-alias-{}-{mode}", std::process::id());
            fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
            let target = PathBuf::from(format!("{directory}/target"));
            let alias = PathBuf::from(format!("{directory}/alias"));
            let link = PathBuf::from(format!("{directory}/link"));
            let first = FileOperand::from_path(&format!("{directory}/first"));
            let last = FileOperand::from_path(&format!("{directory}/last"));
            let file = fs
                .open_raw_impl(&target, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
                .unwrap();
            fs.write_raw_impl(&file, 0, b"old contents").unwrap();
            fs.close_impl(&file).unwrap();
            fs.hard_link_impl(&target, &alias).unwrap();
            fs.symlink_raw_impl(Path::new("target"), &link).unwrap();
            let link = FileOperand::from_os_path(&link);
            let alias = FileOperand::from_os_path(&alias);
            let invalid = FileOperand::from_fd(i32::MAX);
            let writes = [
                WriteOp::new(&first, VfOffset::At(0), b"first".as_slice()).with_creation(),
                WriteOp::new(&invalid, VfOffset::At(0), b"bad".as_slice()),
                WriteOp::new(&link, VfOffset::At(0), b"aaaa".as_slice())
                    .with_creation()
                    .with_truncate(),
                WriteOp::new(&alias, VfOffset::At(0), b"ZZ".as_slice()),
                WriteOp::new(&last, VfOffset::At(0), b"last".as_slice()).with_creation(),
            ];
            let baseline = fs.open_files.len();
            let results = fs.write_results(&writes).unwrap();
            assert_eq!(results.len(), writes.len());
            for (index, count) in [(0, 5), (2, 4), (3, 2), (4, 4)] {
                assert_eq!(
                    results[index].as_ref().unwrap().bytes_written,
                    count,
                    "{mode} input {index}"
                );
            }
            assert_eq!(
                results[1].as_ref().unwrap_err().state(),
                FailureState::NotAttempted
            );
            assert_eq!(fs.open_files.len(), baseline);
            assert_eq!(
                fs.read_raw_impl(&FileOperand::from_os_path(&target), 0, 32)
                    .unwrap(),
                b"ZZaa",
                "repairing the symlink after its accepted alias would overwrite the later write"
            );
            assert_eq!(fs.read_raw_impl(&first, 0, 8).unwrap(), b"first");
            assert_eq!(fs.read_raw_impl(&last, 0, 8).unwrap(), b"last");
            fs.remove_impl(Path::new(&directory), true).unwrap();
        }
    }
    #[test]
    fn live_native_write_hook_retains_per_input_outcomes_in_every_mode() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["off", "openwrite", "full"] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            fs.set_merged_mode(mode);
            vfsi_sync::test_support::write_vector_outcomes(
                &mut fs,
                Path::new(&format!(
                    "/vfsi-native-write-outcomes-{}-{mode}",
                    std::process::id()
                )),
                vfsi_sync::FailureState::Failed,
            );
        }
    }
    #[test]
    fn live_stateid_write_recovery_never_replays_confirmed_bytes_or_later_aliases() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["openwrite", "full"] {
            for later_truncate in [false, true] {
                let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
                fs.set_merged_mode(mode);
                fs.nfs.set_max_compound_bytes(64 * 1024);
                let directory = format!(
                    "/vfsi-stateid-suffix-{}-{mode}-{later_truncate}",
                    std::process::id()
                );
                fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
                let target_path = PathBuf::from(format!("{directory}/target"));
                let alias_path = PathBuf::from(format!("{directory}/alias"));
                let target = fs
                    .open_raw_impl(
                        &target_path,
                        libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                        0o600,
                    )
                    .unwrap();
                fs.close_impl(&target).unwrap();
                fs.hard_link_impl(&target_path, &alias_path).unwrap();
                let target = FileOperand::from_os_path(&target_path);
                let alias = FileOperand::from_os_path(&alias_path);
                let invalid = FileOperand::from_fd(i32::MAX);
                let cwd = FileOperand::cwd();
                let per = fs.nfs.per_op_bytes();
                let data = vec![b'A'; per * 2 + 17];
                fs.nfs.test_fail_merged_write_once(0, 1);
                let mut later = WriteOp::new(&alias, VfOffset::At(0), b"ZZ".as_slice());
                if later_truncate {
                    later = later.with_truncate();
                }
                let _ = crate::compound::thread_compound_stats();
                let results = fs
                    .write_results(&[
                        WriteOp::new(&invalid, VfOffset::At(0), b"bad".as_slice()),
                        WriteOp::new(&cwd, VfOffset::At(0), b"bad".as_slice()),
                        WriteOp::new(&target, VfOffset::At(0), data.as_slice())
                            .with_creation()
                            .with_truncate(),
                        later,
                    ])
                    .unwrap();
                let stats = crate::compound::thread_compound_stats();
                assert_eq!(results.len(), 4);
                for index in [0, 1] {
                    assert_eq!(
                        results[index].as_ref().unwrap_err().state(),
                        FailureState::NotAttempted
                    );
                }
                let result = results[2].as_ref().unwrap();
                assert_eq!(result.file, target);
                assert_eq!(
                    (result.offset, result.bytes_written, result.stable),
                    (0, data.len(), true)
                );
                assert_eq!(results[3].as_ref().unwrap().bytes_written, 2);
                assert!(fs.merged_mode == MergedIoMode::Off);
                assert!(
                    stats.2 < (per * 4) as u64,
                    "replaying the acknowledged first chunk adds another full payload: {stats:?}, chunk {per}"
                );
                let actual = fs.read_raw_impl(&target, 0, data.len() + 1).unwrap();
                if later_truncate {
                    assert_eq!(actual, b"ZZ");
                } else {
                    let mut expected = data;
                    expected[..2].copy_from_slice(b"ZZ");
                    assert_eq!(actual, expected);
                }
                assert!(fs.open_files.is_empty());
                fs.remove_impl(Path::new(&directory), true).unwrap();
            }
        }
    }

    #[test]
    fn live_stateid_truncate_recovery_preserves_the_unexecuted_size_update() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["openwrite", "full"] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            fs.set_merged_mode(mode);
            let directory = format!("/vfsi-stateid-truncate-{}-{mode}", std::process::id());
            fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
            let target_path = PathBuf::from(format!("{directory}/target"));
            let alias_path = PathBuf::from(format!("{directory}/alias"));
            let file = fs
                .open_raw_impl(
                    &target_path,
                    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                    0o600,
                )
                .unwrap();
            fs.write_raw_impl(&file, 0, b"stale contents must disappear")
                .unwrap();
            fs.close_impl(&file).unwrap();
            fs.hard_link_impl(&target_path, &alias_path).unwrap();
            let target = FileOperand::from_os_path(&target_path);
            let alias = FileOperand::from_os_path(&alias_path);
            fs.nfs.fail_merged_truncate_once = true;
            let results = fs
                .write_results(&[
                    WriteOp::new(&target, VfOffset::At(0), b"new".as_slice()).with_truncate(),
                    WriteOp::new(&alias, VfOffset::At(1), b"Z".as_slice()),
                ])
                .unwrap();
            assert!(
                !fs.nfs.fail_merged_truncate_once,
                "exercise the wire SETATTR rejection"
            );
            assert_eq!(results[0].as_ref().unwrap().bytes_written, 3);
            assert!(results[0].as_ref().unwrap().stable);
            assert_eq!(results[1].as_ref().unwrap().bytes_written, 1);
            assert_eq!(fs.read_raw_impl(&target, 0, 64).unwrap(), b"nZw");
            assert_eq!(fs.stat_impl(&target_path).unwrap().mode & 0o777, 0o600);
            assert!(fs.open_files.is_empty());
            fs.remove_impl(Path::new(&directory), true).unwrap();
        }
    }
    #[test]
    fn live_phased_write_does_not_call_a_created_path_unattempted_after_wave_stop() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        fs.set_merged_mode("off");
        fs.nfs.set_max_compound_bytes(64 * 1024);
        let directory = format!("/vfsi-created-write-stop-{}", std::process::id());
        fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
        let first_path = PathBuf::from(format!("{directory}/first"));
        let created_path = PathBuf::from(format!("{directory}/created"));
        let first = fs
            .open_raw_impl(
                &first_path,
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o600,
            )
            .unwrap();
        let created = FileOperand::from_os_path(&created_path);
        let per = fs.nfs.per_op_bytes();
        let data = vec![b'A'; per * 2 + 17];
        let stop = Arc::new(FaultScript::one(
            OpenFaultPoint::AfterWriteChunk { chunk: 0 },
            VfError::errno(None, libc::EACCES as u32),
        ));
        fs.set_fault_injector(stop.clone());
        let results = fs
            .write_results(&[
                WriteOp::new(&first, VfOffset::At(0), data.as_slice()),
                WriteOp::new(&created, VfOffset::At(0), b"never written".as_slice())
                    .with_creation()
                    .with_truncate(),
            ])
            .unwrap();
        assert!(stop.is_consumed());
        let first_error = results[0].as_ref().unwrap_err();
        assert_eq!(first_error.state(), FailureState::Failed);
        assert_eq!(
            first_error.progress(),
            Some(&IoProgress::Write { bytes_written: per })
        );
        let created_error = results[1].as_ref().unwrap_err();
        assert_eq!(
            created_error.state(),
            FailureState::Failed,
            "OPEN already created/truncated this logical input before its WRITE remained unsent"
        );
        assert!(created_error.progress().is_none());
        assert_eq!(created_error.cause().index(), None);
        assert!(fs.exists_impl(&created_path).unwrap());
        assert_eq!(fs.stat_impl(&created_path).unwrap().size, 0);
        assert_eq!(
            fs.open_files.len(),
            1,
            "retire temporary owners after the stopped phase"
        );
        fs.close_impl(&first).unwrap();
        fs.remove_impl(Path::new(&directory), true).unwrap();
    }

    #[test]
    fn live_later_stateid_failure_never_replays_an_earlier_failed_short_repair() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["openwrite", "full"] {
            let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
            fs.set_merged_mode(mode);
            fs.nfs.set_max_compound_bytes(4 * 64 * 1024);
            let directory = format!("/vfsi-repair-first-error-{}-{mode}", std::process::id());
            fs.ensure_dir_impl(Path::new(&directory), 0o755).unwrap();
            let target_path = PathBuf::from(format!("{directory}/target"));
            let last_path = PathBuf::from(format!("{directory}/last"));
            let target = FileOperand::from_os_path(&target_path);
            let last = FileOperand::from_os_path(&last_path);
            let data = vec![b'A'; fs.nfs.per_op_bytes() * 2 + 17];
            fs.nfs.test_short_merged_write_once(7);
            fs.nfs.test_fail_merged_write_once(0, 1);
            let denied = Arc::new(FaultScript::one(
                OpenFaultPoint::BeforeWriteRepair { index: 0 },
                VfError::errno(None, libc::EACCES as u32),
            ));
            fs.set_fault_injector(denied.clone());
            let results = fs
                .write_results(&[
                    WriteOp::new(&target, VfOffset::At(0), data.as_slice())
                        .with_creation()
                        .with_truncate(),
                    WriteOp::new(&last, VfOffset::At(0), b"last".as_slice()).with_creation(),
                ])
                .unwrap();
            assert!(denied.is_consumed());
            let error = results[0].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::Failed);
            assert_eq!(error.cause().raw_code(), libc::EACCES as u32);
            assert_eq!(
                error.progress(),
                Some(&IoProgress::Write { bytes_written: 7 })
            );
            assert_eq!(results[1].as_ref().unwrap().bytes_written, 4);
            assert_eq!(
                fs.read_raw_impl(&target, 0, 64).unwrap(),
                b"AAAAAAA",
                "a later status must not authorize replay of an earlier failed repair"
            );
            assert_eq!(fs.read_raw_impl(&last, 0, 8).unwrap(), b"last");
            fs.remove_impl(Path::new(&directory), true).unwrap();
        }
    }
    #[test]
    fn live_mixed_cursor_writes_observe_earlier_writes_in_every_mode() {
        let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .unwrap();
        for mode in ["full", "openwrite", "off"] {
            for offset in [VfOffset::Cur, VfOffset::End] {
                let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
                fs.set_merged_mode(mode);
                let directory = PathBuf::from(format!(
                    "/mixed-dynamic-{}-{mode}-{offset:?}",
                    std::process::id()
                ));
                fs.ensure_dir_impl(&directory, 0o755).unwrap();
                let path = directory.join("target");
                let file = fs
                    .open_raw_impl(&path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
                    .unwrap();
                let side = FileOperand::from_os_path(&directory.join("side"));
                let bad = FileOperand::from_fd(i32::MAX);
                let writes = [
                    WriteOp::new(&file, offset, b"a".as_slice()),
                    WriteOp::at(&side, 0, b"side".as_slice()).with_creation(),
                    WriteOp::at(&bad, 0, b"bad".as_slice()),
                    WriteOp::new(&file, offset, b"b".as_slice()),
                ];
                let results = fs.write_results(&writes).unwrap();
                assert_eq!(results.len(), 4);
                assert!(results[2].is_err());
                assert_eq!(results[0].as_ref().unwrap().offset, 0);
                assert_eq!(
                    results[3].as_ref().unwrap().offset,
                    1,
                    "mode={mode}, offset={offset:?}"
                );
                assert_eq!(fs.read_raw_impl(&file, 0, 16).unwrap(), b"ab");
                assert_eq!(fs.read_raw_impl(&side, 0, 16).unwrap(), b"side");
                assert_eq!(
                    fs.open_files[&file.fd().unwrap()].cur_offset,
                    if offset == VfOffset::Cur { 2 } else { 0 }
                );
                fs.close_impl(&file).unwrap();
                fs.remove_impl(&directory, true).unwrap();
                fs.shutdown().unwrap();
            }
        }
    }
}
