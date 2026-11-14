use super::*;

pub fn unlink_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    pathname: &Path,
) -> VfResult<()> {
    vfsi_core::internal::collect_vec_result(
        backend.vremove_impl(&[FileOperand::from_os_path(pathname)]),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)
    .map(|_| ())
}

pub fn vunlink_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    pathnames: &[&Path],
) -> VfRes {
    let files: Vec<_> = pathnames
        .iter()
        .map(|path| FileOperand::from_os_path(path))
        .collect();
    vfsi_core::internal::collect_vec_result(backend.vremove_impl(&files), files.len())
        .map_err(vfsi_core::IndexedError::into_cause)
        .map(|_| ())
}

pub fn mkdir_raw_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &Path,
    mode: u32,
) -> VfResult<()> {
    let a = VfAttrs {
        file: FileOperand::from_os_path(path),
        masks: AttrMask::MODE,
        mode,
        ..VfAttrs::default()
    };
    vfsi_core::internal::collect_vec_result(backend.vmkdir_impl(std::slice::from_ref(&a)), 1)
        .map_err(vfsi_core::IndexedError::into_cause)
        .map(|_| ())
}

pub fn ensure_dir_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    dir: &Path,
    mode: u32,
) -> VfResult<()> {
    use std::path::Component;
    let target = Path::new("/").join(backend.abs_path(dir));
    let mut so_far = PathBuf::from("/");
    for comp in target.components() {
        if let Component::Normal(part) = comp {
            so_far.push(part);
            match backend.mkdir_raw_impl(&so_far, mode) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Later mkdir calls validate ancestors. Only the existing
                    // final component needs a separate, following type check.
                    if so_far == target && backend.stat_impl(&so_far)?.ftype != VfType::Directory {
                        return Err(std::io::Error::from_raw_os_error((ERR_NOTDIR) as i32));
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

pub fn native_create_dir_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    mode: u32,
) -> VfResult<()> {
    backend
        .mkdir_raw_impl(path, mode)
        .map_err(|error| vfsi_core::internal::errors::with_context(error, "create_dir", path))
}

pub fn native_rename_impl_default<F: VectorBackend + ?Sized>(
    backend: &mut F,
    from: &std::path::Path,
    to: &std::path::Path,
) -> VfResult<()> {
    vfsi_core::internal::collect_vec_result(
        backend.vrename_impl(&[(
            FileOperand::from_os_path(from),
            FileOperand::from_os_path(to),
        )]),
        1,
    )
    .map_err(vfsi_core::IndexedError::into_cause)
    .map(|_| ())
    .map_err(|error| vfsi_core::internal::errors::with_context(error, "rename", from))
}
