use super::*;

pub fn symlink_raw_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    oldpath: &Path,
    newpath: &Path,
) -> VfResult<()> {
    backend.vsymlink_impl(
        std::slice::from_ref(&oldpath),
        std::slice::from_ref(&newpath),
    )
}

pub fn readlink_raw_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &Path,
) -> VfResult<Vec<u8>> {
    take_single_result(
        "readlink_raw_impl",
        backend.vreadlink_impl(std::slice::from_ref(&path))?,
    )
}

pub fn native_symlink_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    target: &std::path::Path,
    link: &std::path::Path,
) -> VfResult<()> {
    backend
        .symlink_raw_impl(target, link)
        .map_err(|error| error.with_context("symlink", link))
}

pub fn native_hard_link_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    source: &std::path::Path,
    link: &std::path::Path,
) -> VfResult<()> {
    backend
        .vhardlink_impl(&[source], &[link])
        .map_err(|error| error.with_context("hard_link", link))
}

pub fn native_read_link_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    path: &std::path::Path,
) -> VfResult<std::path::PathBuf> {
    backend
        .readlink_raw_impl(path)
        .map(bytes_to_path)
        .map_err(|error| error.with_context("read_link", path))
}
