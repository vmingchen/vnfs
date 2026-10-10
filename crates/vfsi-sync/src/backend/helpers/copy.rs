use super::*;

pub fn vcopy_impl_default<F: VectorBackend + ?Sized>(
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
