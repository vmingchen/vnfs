//! Descriptor and path I/O planning, execution, and completion.

use super::*;

struct DescriptorReadPlan {
    ops: Vec<crate::client::ReadOp>,
    owners: Vec<usize>,
    offsets: Vec<u64>,
}

/// Same-object dependencies for one descriptor WRITE wave. The first range
/// needs no tree allocation; additional independent ranges are indexed in
/// O(log n), avoiding quadratic overlap checks for large scatter vectors.
pub(super) struct WriteWaveAccess {
    positional: bool,
    first: std::ops::Range<u64>,
    additional: std::collections::BTreeMap<u64, u64>,
}

impl WriteWaveAccess {
    pub(super) fn new(range: std::ops::Range<u64>, positional: bool) -> Self {
        Self {
            positional,
            first: range,
            additional: std::collections::BTreeMap::new(),
        }
    }

    pub(super) fn admit(&mut self, range: std::ops::Range<u64>, positional: bool) -> bool {
        if !self.positional || !positional {
            return false;
        }
        // Empty writes do not overlap, and must not replace a nonempty
        // interval with the same start in the overlap index.
        if range.is_empty() {
            return true;
        }
        if !self.first.is_empty() && range.start < self.first.end && self.first.start < range.end {
            return false;
        }
        // Already admitted intervals are disjoint: only the predecessor of
        // this range's end can overlap it, even for nonmonotonic offsets.
        if self
            .additional
            .range(..range.end)
            .next_back()
            .is_some_and(|(_, &end)| end > range.start)
        {
            return false;
        }
        self.additional.insert(range.start, range.end);
        true
    }
}

pub(super) fn remap_descriptor_chunk_error(
    error: VfError,
    start: usize,
    owners: &[usize],
) -> VfError {
    error.index().map_or(error.clone(), |local_index| {
        let chunk_index = start.saturating_add(local_index);
        let index = owners.get(chunk_index).copied().unwrap_or(chunk_index);
        error.with_index(index)
    })
}

pub(super) fn remap_active_error(error: VfError, active: &[usize]) -> VfError {
    match error.index().and_then(|index| active.get(index)).copied() {
        Some(original) => error.with_index(original),
        None => error,
    }
}

pub(super) fn bounded_read_allv_batch(
    active: &[usize],
    remaining: usize,
    compound_budget: usize,
    max_window: usize,
) -> (Vec<usize>, usize) {
    let cohort_len = if remaining == 0 {
        1
    } else {
        active.len().min(remaining)
    };
    let cohort = active[..cohort_len].to_vec();
    if remaining == 0 {
        // Once the payload budget is exhausted, probe one file at a time so
        // exact-limit EOF remains distinguishable from an oversized file
        // without allocating another cohort-sized response.
        return (cohort, 1);
    }
    let protocol_window = (compound_budget / cohort_len)
        .saturating_sub(128)
        .min(max_window)
        .max(1);
    let allocation_window = (remaining / cohort_len).max(1);
    (cohort, protocol_window.min(allocation_window))
}

pub(super) fn merge_read_allv_round(
    active: &[usize],
    results: &[ReadResult],
    out: &mut [Vec<u8>],
    offsets: &mut [u64],
    total: &mut usize,
    max_total_bytes: usize,
) -> VfResult<Vec<usize>> {
    if results.len() != active.len() {
        return Err(VfError::transport(
            None,
            format!(
                "NFS readv returned {} results for {} active files",
                results.len(),
                active.len()
            ),
        ));
    }
    let mut next = Vec::with_capacity(active.len());
    for (result, &original) in results.iter().zip(active) {
        if result.data.is_empty() && !result.eof {
            return Err(VfError::transport(
                original,
                "NFS READ made no progress without reporting EOF",
            ));
        }
        *total = total
            .checked_add(result.data.len())
            .filter(|size| *size <= max_total_bytes)
            .ok_or_else(|| VfError::failure(original, libc::EFBIG as u32))?;
        out[original].extend_from_slice(&result.data);
        offsets[original] = result
            .offset
            .checked_add(result.data.len() as u64)
            .ok_or_else(|| VfError::failure(original, libc::EOVERFLOW as u32))?;
        if !result.eof {
            next.push(original);
        }
    }
    Ok(next)
}

pub(super) fn adb_block_base(pattern: &Adb, block: usize, index: usize) -> VfResult<u64> {
    let relative = (block as u64)
        .checked_mul(pattern.adb_block_size)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))?;
    pattern
        .adb_offset
        .checked_add(relative)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))
}

pub(super) fn adb_field_offset(base: u64, relative: u64, index: usize) -> VfResult<u64> {
    base.checked_add(relative)
        .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))
}

impl NfsVecFs {
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
    ) -> VfResult<(usize, bool)> {
        if received > op.count as usize || (op.count != 0 && received == 0 && !eof) {
            return Err(VfError::client(index, ERR_IO));
        }
        while received < op.count as usize && !eof {
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeReadRepair { index })?;
            let remaining = op.count as usize - received;
            let retry = crate::client::ReadOp {
                fh: op.fh.clone(),
                stateid: op.stateid,
                offset: op
                    .offset
                    .checked_add(received as u64)
                    .ok_or_else(|| VfError::client(index, libc::EOVERFLOW as u32))?,
                count: remaining as u32,
            };
            let mut result = self
                .nfs
                .readv(&[retry])
                .map_err(|error| vfsi_core::error_from_rpc_indexed(error).with_index(index))?;
            if result.len() != 1 {
                return Err(VfError::transport(
                    Some(index),
                    "malformed short READ reply",
                ));
            }
            let (data, end) = result.pop().expect("one result validated");
            if data.len() > remaining || (data.is_empty() && !end) {
                return Err(VfError::client(index, ERR_IO));
            }
            on_data(&data);
            received += data.len();
            eof = end;
        }
        Ok((received, eof))
    }

    fn descriptor_read_plan(
        &mut self,
        reads: &[ReadOp],
        buffers: Option<&[&mut [u8]]>,
    ) -> VfResult<DescriptorReadPlan> {
        let per = self.nfs.read_per_op_bytes();
        let mut ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        let mut owner = Vec::with_capacity(reads.len());
        for (i, op) in reads.iter().enumerate() {
            if buffers.is_some_and(|buffers| {
                buffers
                    .get(i)
                    .is_none_or(|buffer| buffer.len() != op.length)
            }) {
                return Err(VfError::client(i, ERR_INVAL));
            }
            let off = self
                .resolve_offset(&op.file, op.offset)
                .map_err(|e| e.with_index(i))?;
            let length = u64::try_from(op.length)
                .map_err(|_| VfError::failure(i, libc::EOVERFLOW as u32))?;
            off.checked_add(length)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            let o = self
                .open_files
                .get(&op.file.fd().unwrap())
                .cloned()
                .ok_or_else(|| VfError::failure(i, ERR_EBADF))?;
            let mut remaining = op.length;
            let mut chunk_off = 0u64;
            loop {
                let n = remaining.min(per);
                ops.push(crate::client::ReadOp {
                    fh: o.fh.clone(),
                    stateid: o.stateid,
                    offset: off
                        .checked_add(chunk_off)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?,
                    count: n as u32,
                });
                owner.push(i);
                remaining -= n;
                if remaining == 0 {
                    break;
                }
                chunk_off = chunk_off
                    .checked_add(n as u64)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            }
            offsets.push(off);
        }
        Ok(DescriptorReadPlan {
            ops,
            owners: owner,
            offsets,
        })
    }

    pub(super) fn vread_batch_nfs(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let DescriptorReadPlan {
            ops,
            owners: owner,
            offsets,
        } = self.descriptor_read_plan(reads, None)?;
        // The server validates the summed READ counts of a compound against
        // ca_maxresponsesize. Pack by each chunk's actual count so many small
        // descriptor reads share a compound instead of being pessimistically
        // charged the maximum per-op size.
        let byte_limit = if self.nfs.max_response_bytes > 0 {
            self.nfs.read_compound_bytes().saturating_sub(128)
        } else {
            usize::MAX
        };
        let mut results = Vec::with_capacity(ops.len());
        let mut start = 0;
        while start < ops.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < ops.len() {
                let next = ops[end].count as usize;
                if end > start && bytes.saturating_add(next) > byte_limit {
                    break;
                }
                bytes = bytes.saturating_add(next);
                end += 1;
            }
            #[allow(unused_mut)]
            let mut r = self.nfs.readv(&ops[start..end]).map_err(|error| {
                let error = vfsi_core::error_from_rpc_indexed(error);
                remap_descriptor_chunk_error(error, start, &owner)
            })?;
            #[cfg(feature = "test-faults")]
            if let Some((index, limit)) = self.short_read_once
                && let Some(local) = owner[start..end].iter().position(|&item| item == index)
            {
                self.short_read_once = None;
                let (data, eof) = &mut r[local];
                if limit < data.len() {
                    data.truncate(limit);
                    *eof = false;
                }
            }
            results.extend(r);
            start = end;
        }
        let mut out = Vec::with_capacity(reads.len());
        let mut ci = 0usize;
        for (i, op) in reads.iter().enumerate() {
            let off = offsets[i];
            let mut data = Vec::new();
            let mut eof = false;
            while ci < owner.len() && owner[ci] == i {
                if !eof {
                    let (chunk, end) = &results[ci];
                    data.extend_from_slice(chunk);
                    let (_, end) =
                        self.complete_read_chunk(&ops[ci], chunk.len(), *end, i, |chunk| {
                            data.extend_from_slice(chunk)
                        })?;
                    eof = end;
                }
                ci += 1;
            }
            off.checked_add(data.len() as u64)
                .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
            out.push(ReadResult {
                file: op.file.clone(),
                offset: off,
                data,
                eof,
            });
        }
        // Gap repairs can fail and trigger replay of the entire vector. Do
        // not publish any cursor progress until every repair has succeeded.
        for (op, result) in reads.iter().zip(&out) {
            if op.offset == VfOffset::Cur {
                self.advance_offset(&op.file, result.offset + result.data.len() as u64);
            }
        }
        Ok(out)
    }

    /// Descriptor READs decoded directly from NFS reply storage into caller
    /// buffers. The same compound packing and offset rules as `vread_batch_nfs`
    /// apply; no per-result `Vec<u8>` or aggregate read buffer is created.
    pub(super) fn vread_batch_into_nfs(
        &mut self,
        reads: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        let DescriptorReadPlan {
            ops,
            owners,
            offsets,
        } = self.descriptor_read_plan(reads, Some(buffers))?;
        let byte_limit = if self.nfs.max_response_bytes > 0 {
            self.nfs.read_compound_bytes().saturating_sub(128)
        } else {
            usize::MAX
        };
        let mut wire_lengths = vec![0usize; ops.len()];
        let mut wire_eofs = vec![false; ops.len()];
        let mut cursor = 0usize;
        while cursor < ops.len() {
            let mut end = cursor;
            let mut bytes = 0usize;
            while end < ops.len() {
                let next = ops[end].count as usize;
                if end > cursor && bytes.saturating_add(next) > byte_limit {
                    break;
                }
                bytes = bytes.saturating_add(next);
                end += 1;
            }
            #[cfg(feature = "test-faults")]
            let short_read = self.short_read_once.and_then(|(index, bytes)| {
                owners[cursor..end]
                    .iter()
                    .position(|&owner| owner == index)
                    .map(|local| (local, bytes))
            });
            #[cfg(feature = "test-faults")]
            if short_read.is_some() {
                self.short_read_once = None;
            }
            let chunk = self
                .nfs
                .readv_into(&ops[cursor..end], |local_index, data| {
                    #[cfg(feature = "test-faults")]
                    let data = if let Some((_, limit)) =
                        short_read.filter(|&(local, _)| local == local_index)
                    {
                        &data[..limit.min(data.len())]
                    } else {
                        data
                    };
                    let wire_index = cursor + local_index;
                    let owner = owners[wire_index];
                    let start = (ops[wire_index].offset - offsets[owner]) as usize;
                    let Some(destination) = buffers[owner].get_mut(start..start + data.len())
                    else {
                        return Err(RpcError::transport("NFS READ reply exceeds caller buffer")
                            .with_op_index(local_index));
                    };
                    destination.copy_from_slice(data);
                    wire_lengths[wire_index] = data.len();
                    Ok(())
                })
                .map_err(|error| {
                    let error = vfsi_core::error_from_rpc_indexed(error);
                    remap_descriptor_chunk_error(error, cursor, &owners)
                })?;
            for (wire_index, (_count, eof)) in (cursor..end).zip(chunk) {
                #[cfg(feature = "test-faults")]
                let eof = if short_read
                    .is_some_and(|(local, limit)| wire_index == cursor + local && limit < _count)
                {
                    false
                } else {
                    eof
                };
                wire_eofs[wire_index] = eof;
            }
            cursor = end;
        }
        let mut results = Vec::with_capacity(reads.len());
        let mut wire_index = 0;
        for (index, read) in reads.iter().enumerate() {
            let offset = offsets[index];
            let mut length = 0usize;
            let mut eof = false;
            while wire_index < ops.len() && owners[wire_index] == index {
                if !eof {
                    let mut start =
                        (ops[wire_index].offset - offset) as usize + wire_lengths[wire_index];
                    let (count, end) = self.complete_read_chunk(
                        &ops[wire_index],
                        wire_lengths[wire_index],
                        wire_eofs[wire_index],
                        index,
                        |data| {
                            buffers[index][start..start + data.len()].copy_from_slice(data);
                            start += data.len();
                        },
                    )?;
                    length += count;
                    eof = end;
                }
                wire_index += 1;
            }
            offset
                .checked_add(length as u64)
                .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))?;
            results.push(ReadIntoResult {
                file: read.file.clone(),
                offset,
                read: length,
                eof,
            });
        }
        // Match owned reads: whole-vector recovery must see the original
        // cursors, even if a later short-read repair failed after decoding.
        for (read, result) in reads.iter().zip(&results) {
            if read.offset == VfOffset::Cur {
                self.advance_offset(&read.file, result.offset + result.read as u64);
            }
        }
        Ok(results)
    }

    /// Resolve a [`VfOffset`] to a concrete file offset.
    fn resolve_offset(&mut self, file: &VfFile, off: VfOffset) -> VfResult<u64> {
        match off {
            VfOffset::At(offset) => Ok(offset),
            VfOffset::Cur => match file.fd() {
                Some(fd) => Ok(self.open_files.get(&fd).map(|o| o.cur_offset).unwrap_or(0)),
                None => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
            },
            VfOffset::End => {
                let fh = self.resolve_tcfile(file, true)?;
                self.file_size(&fh)
            }
            _ => Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        }
    }

    /// The offset a write should use: like [`resolve_offset`](Self::resolve_offset),
    /// but descriptors opened with `O_APPEND` always write at the end of the
    /// file (one extra size query per write).
    fn write_offset(&mut self, file: &VfFile, off: VfOffset) -> VfResult<u64> {
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
    pub(super) fn advance_offset(&mut self, file: &VfFile, new_offset: u64) {
        if let Some(fd) = file.fd()
            && let Some(o) = self.open_files.get_mut(&fd)
        {
            o.cur_offset = new_offset;
        }
    }

    /// Batched writev for open (descriptor) ops.
    pub(super) fn vwrite_batch_nfs(
        &mut self,
        writes: &[WriteOp<&VfFile, &[u8]>],
    ) -> VfResult<Vec<WriteResult>> {
        let per = self.nfs.per_op_bytes();
        let mut ops = Vec::new();
        let mut offsets = Vec::with_capacity(writes.len());
        let mut ranges = Vec::with_capacity(writes.len());
        let mut handles = Vec::with_capacity(writes.len());
        let mut predecessors = Vec::with_capacity(writes.len());
        let mut positional = Vec::with_capacity(writes.len());
        let mut seen_handles = std::collections::HashSet::new();
        // Validate every operand before issuing any mutating RPC.
        for (index, request) in writes.iter().enumerate() {
            let offset = self
                .write_offset(request.file(), request.offset())
                .map_err(|error| error.with_index(index))?;
            offset
                .checked_add(request.data().len() as u64)
                .ok_or_else(|| VfError::client(index, libc::EOVERFLOW as u32))?;
            let open = self
                .open_files
                .get(&request.file().fd().unwrap())
                .ok_or_else(|| VfError::client(index, ERR_EBADF))?;
            predecessors.push(!seen_handles.insert(open.fh.clone()));
            positional.push(matches!(request.offset(), VfOffset::At(_)) && !open.append);
            handles.push(open.fh.clone());
            let start = ops.len();
            for part in (0..request.data().len().max(1)).step_by(per) {
                let end = request.data().len().min(part.saturating_add(per));
                ops.push(crate::client::WriteOp {
                    fh: open.fh.clone(),
                    stateid: open.stateid,
                    offset: offset + part as u64,
                    data: &request.data()[part..end],
                });
            }
            offsets.push(offset);
            ranges.push(start..ops.len());
        }

        let mut progress = vec![0usize; ops.len()];
        let mut complete = vec![false; ops.len()];
        let mut next_chunks: Vec<_> = ranges.iter().map(|range| range.start).collect();
        let mut complete_prefix = vec![0usize; writes.len()];
        let mut done = vec![false; writes.len()];
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
                            .map_err(|error| error.with_index(index))?
                    } else {
                        offsets[index]
                    };
                    offset
                        .checked_add(request.data().len() as u64)
                        .ok_or_else(|| VfError::client(index, libc::EOVERFLOW as u32))?;
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
            let results = self.nfs.writev(&wire_ops).map_err(|error| {
                let error = vfsi_core::error_from_rpc_indexed(error);
                error.map_index(|index| owners.get(index).copied().unwrap_or(index))
            })?;
            if results.len() != selected.len() {
                return Err(VfError::transport(None, "malformed descriptor WRITE reply"));
            }
            for ((&wire, &index), &(count, committed)) in selected.iter().zip(&owners).zip(&results)
            {
                let remaining = ops[wire].data.len() - progress[wire];
                if count as usize > remaining || (count == 0 && remaining != 0) {
                    return Err(VfError::client(index, ERR_IO));
                }
                progress[wire] += count as usize;
                complete[wire] = progress[wire] == ops[wire].data.len();
                stable[index] &= committed == stable_how4_FILE_SYNC4;
            }
            // Only acknowledged missing suffixes are retried. A failed RPC
            // returns immediately; ambiguous mutations are never replayed.
            for (index, request) in writes.iter().enumerate() {
                if !activated[index] || done[index] {
                    continue;
                }
                while next_chunks[index] < ranges[index].end && complete[next_chunks[index]] {
                    complete_prefix[index] += ops[next_chunks[index]].data.len();
                    next_chunks[index] += 1;
                }
                done[index] = next_chunks[index] == ranges[index].end;
                let contiguous = complete_prefix[index]
                    + if done[index] {
                        0
                    } else {
                        progress[next_chunks[index]]
                    };
                if request.offset() == VfOffset::Cur {
                    self.advance_offset(request.file(), offsets[index] + contiguous as u64);
                }
            }
            #[cfg(feature = "test-faults")]
            {
                self.inject_open_fault(OpenFaultPoint::AfterWriteChunk { chunk: wave })?;
                wave += 1;
            }
        }
        Ok(writes
            .iter()
            .enumerate()
            .map(|(index, request)| WriteResult {
                file: request.file().clone(),
                offset: offsets[index],
                written: request.data().len(),
                stable: stable[index],
            })
            .collect())
    }

    /// The legacy phased path-based readv (resolve + probe + open + read +
    /// close compounds).
    pub(super) fn vread_path_fallback_nfs(
        &mut self,
        reads: &[ReadOp],
    ) -> VfResult<Vec<ReadResult>> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        let mut tmp: Vec<Option<i32>> = vec![None; reads.len()];
        let mut files = Vec::with_capacity(reads.len());
        let mut needs_open = false;
        for (i, r) in reads.iter().enumerate() {
            files.push(&r.file);
            if !r.file.is_descriptor() {
                needs_open = true;
                if r.offset == VfOffset::Cur {
                    // "current position" only exists for open descriptors.
                    return Err(VfError::failure(i, ERR_INVAL));
                }
            }
        }
        if needs_open {
            tmp = self.open_path_batch(
                &files,
                &vec![false; reads.len()],
                false,
                &vec![false; reads.len()],
            )?;
        }
        let remapped: Vec<ReadOp> = reads
            .iter()
            .enumerate()
            .map(|(i, r)| ReadOp {
                file: match tmp[i] {
                    Some(fd) => VfFile::from_fd(fd),
                    None => r.file.clone(),
                },
                offset: r.offset,
                length: r.length,
            })
            .collect();
        let result = self.vread_batch_nfs(&remapped);
        self.close_tmp(&tmp);
        let mut out = result?;
        for (i, r) in out.iter_mut().enumerate() {
            r.file = reads[i].file.clone();
        }
        Ok(out)
    }

    /// One open+read compound, then a separate close compound with the real
    /// stateids (portable fallback).
    pub(super) fn vread_path_openwrite_nfs(
        &mut self,
        reads: &[ReadOp],
        path_ops: &[crate::client::PathReadOp],
        offsets: &[u64],
    ) -> VfResult<Vec<ReadResult>> {
        match self.nfs.readv_path_compound(path_ops, false) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    // Prefix [0..i) succeeded; resume [i..] via the phased
                    // path, re-attributing its error to the original index.
                    let mut out = self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof);
                    match self.vread_path_fallback_nfs(&reads[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                self.close_path_opens(&outcome.opened);
                Ok(self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof))
            }
            Err(error) if error.is_transport() => Err(vfsi_core::error_from_rpc(error, None)),
            Err(_) => self.vread_path_fallback_nfs(reads),
        }
    }

    /// One compound for the whole batch, including the special-stateid
    /// CLOSE. Downgrades to the open+close form if the server rejects that
    /// CLOSE.
    pub(super) fn vread_path_full_nfs(
        &mut self,
        reads: &[ReadOp],
        path_ops: &[crate::client::PathReadOp],
        offsets: &[u64],
    ) -> VfResult<Vec<ReadResult>> {
        match self.nfs.readv_path_compound(path_ops, true) {
            Ok(outcome) => {
                if let Some((i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        // The special stateid itself is rejected: disable the
                        // merged path entirely.
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    let mut out = self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof);
                    match self.vread_path_fallback_nfs(&reads[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                if let Some(_st) = outcome.close_failed {
                    self.merged_mode = MergedIoMode::OpenWrite;
                    return self.vread_path_openwrite_nfs(reads, path_ops, offsets);
                }
                Ok(self.assemble_reads(reads, offsets, &outcome.data, &outcome.eof))
            }
            Err(error) if error.is_transport() => Err(vfsi_core::error_from_rpc(error, None)),
            Err(_) => self.vread_path_fallback_nfs(reads),
        }
    }

    fn assemble_reads(
        &self,
        reads: &[ReadOp],
        offsets: &[u64],
        data: &[Option<Vec<u8>>],
        eof: &[Option<bool>],
    ) -> Vec<ReadResult> {
        reads
            .iter()
            .enumerate()
            .map(|(i, r)| ReadResult {
                file: r.file.clone(),
                offset: offsets[i],
                data: data[i].clone().unwrap_or_default(),
                eof: eof[i].unwrap_or(false),
            })
            .collect()
    }

    /// The legacy phased path-based writev.
    pub(super) fn vwrite_path_fallback_nfs(
        &mut self,
        writes: &[WriteOp<&VfFile, &[u8]>],
    ) -> VfResult<Vec<WriteResult>> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        let mut tmp: Vec<Option<i32>> = vec![None; writes.len()];
        let mut files = Vec::with_capacity(writes.len());
        let mut creation = Vec::with_capacity(writes.len());
        let mut needs_open = false;
        for (i, w) in writes.iter().enumerate() {
            files.push(w.file());
            creation.push(w.creates());
            if !w.file().is_descriptor() {
                needs_open = true;
                if w.offset() == VfOffset::Cur {
                    return Err(VfError::failure(i, ERR_INVAL));
                }
            }
        }
        if needs_open {
            let truncation: Vec<bool> = writes.iter().map(|w| w.truncates()).collect();
            tmp = self.open_path_batch(&files, &creation, true, &truncation)?;
        }
        let remapped_files: Vec<VfFile> = writes
            .iter()
            .enumerate()
            .map(|(i, w)| match tmp[i] {
                Some(fd) => VfFile::from_fd(fd),
                None => w.file().clone(),
            })
            .collect();
        let remapped: Vec<WriteOp<&VfFile, &[u8]>> = writes
            .iter()
            .zip(&remapped_files)
            .map(|(w, file)| WriteOp::new(file, w.offset(), w.data()))
            .collect();
        let result = self.vwrite_batch_nfs(&remapped);
        self.close_tmp(&tmp);
        let mut out = result?;
        for (i, r) in out.iter_mut().enumerate() {
            r.file = writes[i].file().clone();
        }
        Ok(out)
    }

    pub(super) fn vwrite_path_openwrite_nfs(
        &mut self,
        writes: &[WriteOp<&VfFile, &[u8]>],
        path_ops: &[crate::client::PathWriteOp<'_>],
        offsets: &[u64],
    ) -> VfResult<Vec<WriteResult>> {
        match self.nfs.writev_path_compound(path_ops, false) {
            Ok(outcome) => {
                if let Some((i, _st)) = outcome.failed {
                    self.close_path_opens(&outcome.opened);
                    let mut out =
                        self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed);
                    match self.vwrite_path_fallback_nfs(&writes[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                self.close_path_opens(&outcome.opened);
                Ok(self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed))
            }
            Err(error) if error.is_transport() => Err(vfsi_core::error_from_rpc(error, None)),
            Err(_) => self.vwrite_path_fallback_nfs(writes),
        }
    }

    pub(super) fn vwrite_path_full_nfs(
        &mut self,
        writes: &[WriteOp<&VfFile, &[u8]>],
        path_ops: &[crate::client::PathWriteOp<'_>],
        offsets: &[u64],
    ) -> VfResult<Vec<WriteResult>> {
        match self.nfs.writev_path_compound(path_ops, true) {
            Ok(outcome) => {
                if let Some((i, st)) = outcome.failed {
                    if Self::is_stateid_error(st) {
                        self.merged_mode = MergedIoMode::Off;
                    }
                    self.close_path_opens(&outcome.opened);
                    let mut out =
                        self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed);
                    match self.vwrite_path_fallback_nfs(&writes[i..]) {
                        Ok(suffix) => {
                            for (k, r) in suffix.into_iter().enumerate() {
                                out[i + k] = r;
                            }
                            return Ok(out);
                        }
                        Err(e) => {
                            return Err(e.map_index(|rel| i + rel));
                        }
                    }
                }
                if let Some(_st) = outcome.close_failed {
                    self.merged_mode = MergedIoMode::OpenWrite;
                    return self.vwrite_path_openwrite_nfs(writes, path_ops, offsets);
                }
                Ok(self.assemble_writes(writes, offsets, &outcome.counts, &outcome.committed))
            }
            Err(error) if error.is_transport() => Err(vfsi_core::error_from_rpc(error, None)),
            Err(_) => self.vwrite_path_fallback_nfs(writes),
        }
    }

    fn assemble_writes(
        &self,
        writes: &[WriteOp<&VfFile, &[u8]>],
        offsets: &[u64],
        counts: &[Option<u32>],
        committed: &[Option<u32>],
    ) -> Vec<WriteResult> {
        writes
            .iter()
            .enumerate()
            .map(|(i, w)| WriteResult {
                file: w.file().clone(),
                offset: offsets[i],
                written: counts[i].unwrap_or(0) as usize,
                stable: committed[i].unwrap_or(0) == stable_how4_FILE_SYNC4,
            })
            .collect()
    }
}
