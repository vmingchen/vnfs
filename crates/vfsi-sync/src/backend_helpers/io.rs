use super::*;

pub fn vread_into_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    reads: &[ReadOp],
    buffers: &mut [&mut [u8]],
) -> VfResult<Vec<ReadIntoResult>> {
    if reads.len() != buffers.len() {
        return Err(VfError::client(0, ERR_INVAL));
    }
    for (index, (request, buffer)) in reads.iter().zip(buffers.iter()).enumerate() {
        if request.length != buffer.len() {
            return Err(VfError::client(index, ERR_INVAL));
        }
    }
    let results = backend.vread_impl(reads)?;
    validate_read_results("vread_into_impl", reads, &results)?;
    Ok(results
        .into_iter()
        .zip(buffers.iter_mut())
        .map(|(result, buffer)| {
            buffer[..result.data.len()].copy_from_slice(&result.data);
            ReadIntoResult {
                file: result.file,
                offset: result.offset,
                read: result.data.len(),
                eof: result.eof,
            }
        })
        .collect())
}

pub fn vopen_outcomes_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: &[i32],
    modes: &[u32],
) -> VfResult<ManyResults<VfFile>> {
    if paths.len() != flags.len() || paths.len() != modes.len() {
        return Err(VfError::failure(0, ERR_INVAL));
    }
    let mut results = Vec::with_capacity(paths.len());
    for ((path, flag), mode) in paths.iter().zip(flags).zip(modes) {
        match backend.open_raw_impl(path, *flag, *mode) {
            Ok(file) => results.push(Ok(file)),
            Err(error) => {
                results.push(Err(error));
                break;
            }
        }
    }
    Ok(ManyResults::new(paths.len(), results))
}

pub fn before_open_cleanup_default<F: Backend + ?Sized>(
    _backend: &mut F,
    _index: usize,
    _file: &VfFile,
) -> VfResult<()> {
    Ok(())
}

pub fn vopen_raw_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: &[i32],
    modes: &[u32],
) -> VfResult<Vec<VfFile>> {
    let results = backend.vopen_outcomes_impl(paths, flags, modes)?;
    results.try_collect_with_cleanup(paths.len(), |index, file| {
        let injected = backend.before_open_cleanup(index, file);
        let closed = backend.close_impl(file);
        injected.and(closed)
    })
}

pub fn vopen_raw_simple_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    paths: &[&Path],
    flags: i32,
    mode: u32,
) -> VfResult<Vec<VfFile>> {
    let flags_v = vec![flags; paths.len()];
    let modes_v = vec![mode; paths.len()];
    backend.vopen_raw_impl(paths, &flags_v, &modes_v)
}

pub fn vclose_impl_default<F: Backend + ?Sized>(backend: &mut F, files: &[VfFile]) -> VfRes {
    for (i, f) in files.iter().enumerate() {
        backend.close_impl(f).map_err(|e| e.with_index(i))?;
    }
    Ok(())
}

pub fn vopen_typed_default<F: Backend + ?Sized>(
    backend: &mut F,
    requests: &[OpenOp],
) -> VfResult<Vec<VfFile>> {
    let paths: Vec<&std::path::Path> = requests
        .iter()
        .map(|request| request.path.as_path())
        .collect();
    let flags = translate_open_flags(requests)?;
    let modes: Vec<u32> = requests.iter().map(|request| request.mode).collect();
    backend.vopen_raw_impl(&paths, &flags, &modes)
}

pub fn native_read_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
) -> VfResult<ReadResult> {
    let mut results = backend.vread_impl(std::slice::from_ref(request))?;
    validate_read_results("read_impl", std::slice::from_ref(request), &results)?;
    Ok(results.pop().expect("validated one result"))
}

pub fn native_read_into_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
    buffer: &mut [u8],
) -> VfResult<ReadIntoResult> {
    let mut results = backend.vread_into_impl(std::slice::from_ref(request), &mut [buffer])?;
    validate_read_into_results("read_into_impl", std::slice::from_ref(request), &results)?;
    Ok(results.pop().expect("validated one result"))
}

pub fn native_write_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    request: WriteOp<&VfFile, &[u8]>,
) -> VfResult<WriteResult> {
    let mut results = backend.vwrite_impl(std::slice::from_ref(&request))?;
    validate_write_results("write_impl", std::slice::from_ref(&request), &results)?;
    Ok(results.pop().expect("validated one result"))
}
