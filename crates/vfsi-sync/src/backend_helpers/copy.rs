use super::*;

pub fn vcopy_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    pairs: &[ExtentPair],
    options: CopyOption,
) -> VfRes {
    if pairs.is_empty() {
        return Ok(());
    }
    if !options.follows_source_symlinks() {
        return Err(VfError::unsupported(0));
    }
    backend.vcopy_data_impl(pairs)
}

pub fn native_copy_impl_default<F: Backend + ?Sized>(
    backend: &mut F,
    source: &std::path::Path,
    destination: &std::path::Path,
) -> VfResult<()> {
    backend
        .vcopy_impl(
            &[ExtentPair::from_os_paths(source, 0, destination, 0, None)],
            vfsi_core::CopyOption::new(),
        )
        .map_err(|error| error.with_context("copy", source))
}
