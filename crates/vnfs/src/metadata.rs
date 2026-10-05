use crate::AttrsOptions;
pub(crate) fn metadata_backend<F, P: AsRef<std::path::Path>>(
    client: &vfsi_sync::FsClient<F>,
    paths: &[P],
    options: AttrsOptions,
) -> crate::Result<Vec<crate::Attrs>>
where
    F: vfsi_sync::Backend + vfsi_sync::Backend + vfsi_sync::Backend + 'static,
{
    client.vgetattrs_native(
        paths,
        options.requested_attributes(),
        options.follows_symlinks(),
    )
}
