//! Private write error translation.
// Keep low-level completion machinery private without leaking its historical
// operation name through application errors. Preserve status, index, and path.
pub(crate) fn public_write_error(error: crate::Error) -> crate::Error {
    if error.operation() == Some("vwrite_all_native")
        && let Some(path) = error.path().map(std::path::Path::to_path_buf)
    {
        error.with_context("vwrite_native", path)
    } else {
        error
    }
}
