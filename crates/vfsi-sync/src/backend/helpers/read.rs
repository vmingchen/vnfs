use super::*;

pub fn vread_all_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
) -> VfResult<Vec<Vec<u8>>> {
    backend.vread_all_with_options_impl(files, ReadAllOptions::default())
}

pub fn vread_all_with_options_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
    options: ReadAllOptions,
) -> VfResult<Vec<Vec<u8>>> {
    let mut collector = ReadCollector::new(files.len(), options);
    let stream_budget = options
        .max_total_bytes()
        .clamp(1, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    let chunk_size = stream_budget.min(1024 * 1024);
    backend.vstream_impl(
        files,
        chunk_size,
        stream_budget,
        &mut |index, _, data, _| collector.push(index, data),
    )?;
    collector.finish()
}

/// Collect retained local read cohorts without copying full small first chunks.
/// Unlike the ordinary default above, this explicitly uses the shared vector
/// driver. Backends with specialized stream overrides keep the ordinary default.
pub fn vread_all_owned_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    files: &[FileOperand],
    options: ReadAllOptions,
    batch_size: std::num::NonZeroUsize,
) -> VfResult<Vec<Vec<u8>>> {
    let mut collector = ReadCollector::new(files.len(), options);
    let budget = options
        .max_total_bytes()
        .clamp(1, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    stream_read_chunks(
        files,
        |pending| {
            budget
                .min(1024 * 1024)
                .min((budget / pending.clamp(1, batch_size.get())).max(4096))
        },
        budget,
        |reads| backend.vread_impl(reads),
        |index, result| {
            let data = result.data;
            if !collector.charge(index, data.len()) {
                return false;
            }
            let output = &mut collector.out[index];
            // Keep partial replies right-sized. Preserve the large-chunk
            // allocation/copy layout too: adopting 1 MiB first chunks regressed
            // large warm/cold reads in matched benchmarks. Small full chunks
            // avoid a copy without changing that proven large-file path.
            if output.is_empty() && data.len() <= 64 * 1024 && data.capacity() == data.len() {
                *output = data;
            } else {
                output.extend_from_slice(&data);
            }
            true
        },
    )?;
    collector.finish()
}

struct ReadCollector {
    out: Vec<Vec<u8>>,
    total: usize,
    limit: usize,
    limit_error: Option<usize>,
}

impl ReadCollector {
    fn new(files: usize, options: ReadAllOptions) -> Self {
        Self {
            out: (0..files).map(|_| Vec::new()).collect(),
            total: 0,
            limit: options.max_total_bytes(),
            limit_error: None,
        }
    }

    fn charge(&mut self, index: usize, bytes: usize) -> bool {
        match self.total.checked_add(bytes) {
            Some(total) if total <= self.limit => {
                self.total = total;
                true
            }
            _ => {
                self.limit_error = Some(index);
                false
            }
        }
    }

    fn push(&mut self, index: usize, data: &[u8]) -> bool {
        if !self.charge(index, data.len()) {
            return false;
        }
        self.out[index].extend_from_slice(data);
        true
    }

    fn finish(self) -> VfResult<Vec<Vec<u8>>> {
        match self.limit_error {
            Some(index) => Err(VfError::errno(index, libc::EFBIG as u32)),
            None => Ok(self.out),
        }
    }
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
        |reads| backend.vread_impl(reads),
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
