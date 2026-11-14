use super::*;

pub fn vf_path_default<F: HandleBackend + ?Sized>(
    backend: &F,
    file: &FileOperand,
) -> VfResult<PathBuf> {
    match file {
        FileOperand::Path {
            base: VfPathBase::Abs,
            path,
        } => Ok(backend.abs_path(&Path::new("/").join(path))),
        FileOperand::Path {
            base: VfPathBase::Cwd,
            path,
        }
        | FileOperand::CwdPath(path) => Ok(backend.abs_path(path)),
        FileOperand::Cwd => Ok(backend.abs_path(Path::new(""))),
        FileOperand::Fd(_) | FileOperand::Saved => {
            Err(std::io::Error::from(std::io::ErrorKind::InvalidInput))
        }
        _ => Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)),
    }
}

pub fn open_raw_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    pathname: &Path,
    flags: i32,
    mode: u32,
) -> VfResult<FileOperand> {
    backend.open_path_impl(VfPathBase::Cwd, pathname, flags, mode)
}

pub fn read_raw_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    file: &FileOperand,
    offset: u64,
    length: usize,
) -> VfResult<Vec<u8>> {
    let request = ReadOp::at(file.clone(), offset, length);
    let result = backend.read_impl(&request)?;
    validate_read_results(
        "read_raw",
        std::slice::from_ref(&request),
        std::slice::from_ref(&result),
    )
    .map_err(vfsi_core::IndexedError::into_cause)?;
    Ok(result.data)
}

pub fn write_raw_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    file: &FileOperand,
    offset: u64,
    data: &[u8],
) -> VfResult<usize> {
    let request = WriteOp::new(file, VfOffset::At(offset), data);
    let result = backend.write_impl(request)?;
    validate_write_results(
        "write_raw",
        std::slice::from_ref(&request),
        std::slice::from_ref(&result),
    )
    .map_err(vfsi_core::IndexedError::into_cause)?;
    Ok(result.bytes_written)
}

pub fn native_open_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    request: &OpenOp,
) -> VfResult<FileOperand> {
    backend
        .open_raw_impl(
            request.path(),
            vfsi_core::open_flags_to_libc(request.flags())?,
            request.creation_mode(),
        )
        .map_err(|error| vfsi_core::internal::errors::with_context(error, "open", request.path()))
}

pub fn native_seek_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    file: &FileOperand,
    position: std::io::SeekFrom,
) -> VfResult<u64> {
    let (offset, whence) = match position {
        std::io::SeekFrom::Start(offset) => (
            i64::try_from(offset)
                .map_err(|_| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?,
            SeekFrom::Set,
        ),
        std::io::SeekFrom::End(offset) => (offset, SeekFrom::End),
        std::io::SeekFrom::Current(offset) => (offset, SeekFrom::Cur),
    };
    u64::try_from(backend.seek_raw_impl(file, offset, whence)?)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

pub fn native_set_attributes_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    update: &SetAttrsOp<Target<'_, FileOperand>>,
) -> VfResult<()> {
    let path = match update.target() {
        Target::Path(path) => Some(*path),
        Target::File(file) => file.path(),
    };
    let result = vfsi_core::internal::collect_vec_result(
        backend.vsetattrs_impl(std::slice::from_ref(update)),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)
    .map(|_| ());
    result.map_err(|error| match path {
        Some(path) => vfsi_core::internal::errors::with_context(error, "set_attributes", path),
        None => error,
    })
}

pub fn read_file_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    file: &FileOperand,
    max_bytes: usize,
) -> VfResult<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let remaining = max_bytes.saturating_sub(output.len());
        let request = ReadOp::new(
            file.clone(),
            VfOffset::At(output.len() as u64),
            remaining.clamp(1, 1024 * 1024),
        );
        let result = backend.read_impl(&request)?;
        validate_read_results(
            "read_file",
            std::slice::from_ref(&request),
            std::slice::from_ref(&result),
        )
        .map_err(vfsi_core::IndexedError::into_cause)?;
        if result.data.len() > remaining {
            return Err(std::io::Error::from_raw_os_error(libc::EFBIG));
        }
        if result.data.is_empty() && !result.eof {
            return Err(std::io::Error::from_raw_os_error((ERR_IO) as i32));
        }
        output.extend_from_slice(&result.data);
        if result.eof {
            return Ok(output);
        }
    }
}

pub fn read_into_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
    buffer: &mut [u8],
) -> VfResult<ReadIntoResult> {
    if request.length != buffer.len() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let result = backend.read_impl(request)?;
    if result.data.len() > buffer.len() {
        return Err(std::io::Error::from_raw_os_error((ERR_IO) as i32));
    }
    validate_read_results(
        "read_into_impl",
        std::slice::from_ref(request),
        std::slice::from_ref(&result),
    )
    .map_err(vfsi_core::IndexedError::into_cause)?;
    buffer[..result.data.len()].copy_from_slice(&result.data);
    Ok(ReadIntoResult {
        file: result.file,
        offset: result.offset,
        bytes_read: result.data.len(),
        eof: result.eof,
    })
}

pub fn vsetattrs_impl_default<F: HandleBackend + ?Sized>(
    backend: &mut F,
    updates: &[SetAttrsOp<Target<'_, FileOperand>>],
) -> vfsi_core::VecResult<()> {
    match updates.len() {
        0 => Ok(Vec::new()),
        1 => Ok(vec![backend.set_attributes_impl(&updates[0]).map_err(
            |cause| {
                if vfsi_core::transport_kind(&(cause)).is_some() {
                    vfsi_core::OpError::unknown(cause, None)
                } else {
                    vfsi_core::OpError::failed(cause, None)
                }
            },
        )]),
        _ => Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
    }
}
