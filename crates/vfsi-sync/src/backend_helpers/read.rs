use super::*;

pub fn vread_all_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    files: &[VfFile],
) -> VfResult<Vec<Vec<u8>>> {
    backend.vread_all_with_options_impl(files, ReadAllOptions::default())
}

pub fn vread_all_with_options_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    files: &[VfFile],
    options: ReadAllOptions,
) -> VfResult<Vec<Vec<u8>>> {
    let mut out: Vec<Vec<u8>> = files.iter().map(|_| Vec::new()).collect();
    let mut total = 0usize;
    let mut limit_error = None;
    let stream_budget = options
        .total_byte_limit()
        .clamp(1, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    let chunk_size = stream_budget.min(1024 * 1024);
    backend.vstream_impl(
        files,
        chunk_size,
        stream_budget,
        &mut |index, _, data, _| {
            let Some(next_total) = total.checked_add(data.len()) else {
                limit_error = Some(index);
                return false;
            };
            if next_total > options.total_byte_limit() {
                limit_error = Some(index);
                return false;
            }
            out[index].extend_from_slice(data);
            total = next_total;
            true
        },
    )?;
    if let Some(index) = limit_error {
        return Err(VfError::failure(index, libc::EFBIG as u32));
    }
    Ok(out)
}

pub fn vstream_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    files: &[VfFile],
    chunk_size: usize,
    memory_limit: usize,
    cb: &mut ReadStreamCallback<'_>,
) -> VfRes {
    use std::collections::VecDeque;

    if chunk_size == 0 || memory_limit == 0 {
        return Err(VfError::failure(0, ERR_INVAL));
    }
    let mut pending: VecDeque<usize> = (0..files.len()).collect();
    let mut offsets = vec![0u64; files.len()];
    while !pending.is_empty() {
        let mut budget = memory_limit;
        let mut batch_indices = Vec::new();
        let mut reads = Vec::new();
        while budget > 0 && !pending.is_empty() {
            let index = pending.pop_front().expect("pending was non-empty");
            let length = chunk_size.min(budget);
            reads.push(ReadOp::at(files[index].clone(), offsets[index], length));
            batch_indices.push(index);
            budget -= length;
        }
        let results = backend.vread_impl(&reads).map_err(|error| {
            error.index().map_or(error.clone(), |local_index| {
                batch_indices
                    .get(local_index)
                    .copied()
                    .map_or(error.clone(), |index| error.with_index(index))
            })
        })?;
        validate_read_results("vstream_impl", &reads, &results).map_err(|error| {
            error.index().map_or(error.clone(), |local_index| {
                batch_indices
                    .get(local_index)
                    .copied()
                    .map_or(error.clone(), |index| error.with_index(index))
            })
        })?;
        for (batch_index, result) in results.into_iter().enumerate() {
            let index = batch_indices[batch_index];
            let offset = offsets[index];
            let eof = result.eof;
            offsets[index] = offset
                .checked_add(result.data.len() as u64)
                .ok_or_else(|| VfError::failure(index, libc::EOVERFLOW as u32))?;
            if !cb(index, offset, &result.data, eof) {
                return Ok(());
            }
            if !eof {
                pending.push_back(index);
            }
        }
    }
    Ok(())
}

pub fn native_read_file_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    max_bytes: usize,
) -> VfResult<Vec<u8>> {
    backend
        .vread_all_with_options_impl(
            std::slice::from_ref(file),
            ReadAllOptions::new().max_total_bytes(max_bytes),
        )
        .and_then(|mut results| {
            if results.len() != 1 {
                return Err(VfError::transport(
                    None,
                    "read_file backend returned an invalid result count",
                ));
            }
            let data = results.pop().expect("validated result count");
            if data.len() > max_bytes {
                return Err(VfError::client(0, libc::EFBIG as u32));
            }
            Ok(data)
        })
}
