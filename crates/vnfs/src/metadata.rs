//! Options shared by scalar convenience and vector metadata queries.

/// Select metadata fields and whether to follow the final symlink.
/// Ancestor symlinks use ordinary backend path-resolution semantics.
#[derive(Clone, Copy, Debug)]
pub struct MetadataOptions {
    fields: crate::MetadataFields,
    follow: bool,
}
impl Default for MetadataOptions {
    fn default() -> Self {
        Self {
            fields: crate::MetadataFields::stat(),
            follow: true,
        }
    }
}
impl MetadataOptions {
    /// Standard stat fields, following the final symlink.
    pub fn new() -> Self {
        Self::default()
    }
    /// Select requested attributes. Object type may additionally be fetched.
    pub fn fields(mut self, fields: crate::MetadataFields) -> Self {
        self.fields = fields;
        self
    }
    /// Follow the final symlink when true; otherwise describe the link itself.
    pub fn follow_symlinks(mut self, follow: bool) -> Self {
        self.follow = follow;
        self
    }
    /// Requested attributes; unavailable optional attributes remain absent.
    pub fn requested_fields(self) -> crate::MetadataFields {
        self.fields
    }
    /// Whether to follow the final component if it is a symlink.
    pub fn follows_symlinks(self) -> bool {
        self.follow
    }
}
pub(crate) fn metadata_backend<F, P: AsRef<std::path::Path>>(
    client: &vfsi_sync::FsClient<F>,
    paths: &[P],
    options: MetadataOptions,
) -> crate::Result<Vec<crate::Metadata>>
where
    F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static,
{
    client.metadata_many(
        paths,
        options.requested_fields(),
        options.follows_symlinks(),
    )
}
