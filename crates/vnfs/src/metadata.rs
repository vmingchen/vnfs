use crate::MetadataOptions;
pub(crate) fn metadata_backend<F, P: AsRef<std::path::Path>>(
    client: &vfsi_sync::FsClient<F>,
    paths: &[P],
    options: MetadataOptions,
) -> crate::Result<Vec<crate::Metadata>>
where
    F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::Backend + 'static,
{
    client.vgetattrs_native(
        paths,
        options.requested_fields(),
        options.follows_symlinks(),
    )
}
