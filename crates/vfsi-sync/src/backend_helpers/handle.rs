use super::*;

pub fn vf_path_default<F: FileSystem + ?Sized>(backend: &F, file: &VfFile) -> VfResult<PathBuf> {
    match file {
        VfFile::Path {
            base: VfPathBase::Abs,
            path,
        } => Ok(backend.abs_path(&Path::new("/").join(path))),
        VfFile::Path {
            base: VfPathBase::Cwd,
            path,
        }
        | VfFile::CwdPath(path) => Ok(backend.abs_path(path)),
        VfFile::Cwd => Ok(backend.abs_path(Path::new(""))),
        VfFile::Descriptor(_) | VfFile::Saved => Err(VfError::failure(0, ERR_INVAL)),
        _ => Err(VfError::failure(0, ERR_INVAL)),
    }
}

pub fn open_raw_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    pathname: &Path,
    flags: i32,
    mode: u32,
) -> VfResult<VfFile> {
    backend.open_path_impl(VfPathBase::Cwd, pathname, flags, mode)
}

pub fn read_raw_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    offset: u64,
    length: usize,
) -> VfResult<Vec<u8>> {
    let request = ReadOp::at(file.clone(), offset, length);
    let result = backend.read_impl(&request)?;
    validate_read_results(
        "read_raw",
        std::slice::from_ref(&request),
        std::slice::from_ref(&result),
    )?;
    Ok(result.data)
}

pub fn write_raw_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    offset: u64,
    data: &[u8],
) -> VfResult<usize> {
    let request = WriteOp::new(file, VfOffset::At(offset), data);
    let result = backend.write_impl(request)?;
    validate_write_results(
        "write_raw",
        std::slice::from_ref(&request),
        std::slice::from_ref(&result),
    )?;
    Ok(result.written)
}

pub fn native_open_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    request: &OpenOp,
) -> VfResult<VfFile> {
    backend
        .open_raw_impl(
            request.path(),
            vfsi_core::open_flags_to_libc(request.flags())?,
            request.creation_mode(),
        )
        .map_err(|error| error.with_context("open", request.path()))
}

pub fn native_seek_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
    position: std::io::SeekFrom,
) -> VfResult<u64> {
    let (offset, whence) = match position {
        std::io::SeekFrom::Start(offset) => (
            i64::try_from(offset).map_err(|_| VfError::failure(0, libc::EOVERFLOW as u32))?,
            SeekFrom::Set,
        ),
        std::io::SeekFrom::End(offset) => (offset, SeekFrom::End),
        std::io::SeekFrom::Current(offset) => (offset, SeekFrom::Cur),
    };
    u64::try_from(backend.seek_raw_impl(file, offset, whence)?)
        .map_err(|_| VfError::failure(0, ERR_INVAL))
}

pub fn native_set_attributes_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    update: &SetAttrsOp<Target<'_, VfFile>>,
) -> VfResult<()> {
    let path = match update.target() {
        Target::Path(path) => Some(*path),
        Target::File(file) => file.path(),
    };
    let result = backend.vsetattrs_impl(std::slice::from_ref(update));
    result.map_err(|error| match path {
        Some(path) => error.with_context("set_attributes", path),
        None => error,
    })
}

pub fn read_file_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    file: &VfFile,
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
        )?;
        if result.data.len() > remaining {
            return Err(VfError::client(0, libc::EFBIG as u32));
        }
        if result.data.is_empty() && !result.eof {
            return Err(VfError::client(0, ERR_IO));
        }
        output.extend_from_slice(&result.data);
        if result.eof {
            return Ok(output);
        }
    }
}

pub fn read_into_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    request: &ReadOp,
    buffer: &mut [u8],
) -> VfResult<ReadIntoResult> {
    if request.length != buffer.len() {
        return Err(VfError::client(0, ERR_INVAL));
    }
    let result = backend.read_impl(request)?;
    if result.data.len() > buffer.len() {
        return Err(VfError::client(0, ERR_IO));
    }
    validate_read_results(
        "read_into_impl",
        std::slice::from_ref(request),
        std::slice::from_ref(&result),
    )?;
    buffer[..result.data.len()].copy_from_slice(&result.data);
    Ok(ReadIntoResult {
        file: result.file,
        offset: result.offset,
        read: result.data.len(),
        eof: result.eof,
    })
}

pub fn vsetattrs_impl_default<F: FileSystem + ?Sized>(
    backend: &mut F,
    updates: &[SetAttrsOp<Target<'_, VfFile>>],
) -> VfResult<()> {
    match updates.len() {
        0 => Ok(()),
        1 => backend.set_attributes_impl(&updates[0]),
        _ => Err(VfError::unsupported(0)),
    }
}
