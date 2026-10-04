use super::*;

pub fn unlink_impl_default<F: Backend + ?Sized>(backend: &mut F, pathname: &Path) -> VfResult<()> {
    backend.vremove_impl(&[VfFile::from_os_path(pathname)])
}

pub fn vunlink_impl_default<F: Backend + ?Sized>(backend: &mut F, pathnames: &[&Path]) -> VfRes {
    let files: Vec<VfFile> = pathnames.iter().map(|p| VfFile::from_os_path(p)).collect();
    backend.vremove_impl(&files)
}

pub fn mkdir_raw_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &Path,
    mode: u32,
) -> VfResult<()> {
    let a = VfAttrs {
        file: VfFile::from_os_path(path),
        masks: AttrMask::MODE,
        mode,
        ..VfAttrs::default()
    };
    backend.vmkdir_impl(std::slice::from_ref(&a))
}

pub fn ensure_dir_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    dir: &Path,
    mode: u32,
) -> VfResult<()> {
    use std::path::Component;
    let mut so_far = PathBuf::new();
    for comp in backend.abs_path(dir).components() {
        if let Component::Normal(part) = comp {
            so_far.push(part);
            let full = Path::new("/").join(&so_far);
            match backend.mkdir_raw_impl(&full, mode) {
                Ok(()) => {}
                Err(e) if e.err_no() == ERR_EXIST => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

pub fn native_create_dir_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
    mode: u32,
) -> VfResult<()> {
    backend
        .mkdir_raw_impl(path, mode)
        .map_err(|error| error.with_context("create_dir", path))
}

pub fn native_rename_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    from: &std::path::Path,
    to: &std::path::Path,
) -> VfResult<()> {
    backend
        .vrename_impl(&[(VfFile::from_os_path(from), VfFile::from_os_path(to))])
        .map_err(|error| error.with_context("rename", from))
}
