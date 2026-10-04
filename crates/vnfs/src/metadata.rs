//! Options shared by scalar convenience and vector metadata queries.

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct MetadataFlags {
    #[bits(default = true)]
    follow: bool,
    #[bits(7)]
    _reserved: u8,
}

/// Select metadata fields and whether to follow the final symlink.
/// Ancestor symlinks use ordinary backend path-resolution semantics.
#[derive(Clone, Copy, Debug)]
pub struct MetadataOptions {
    fields: crate::MetadataFields,
    flags: MetadataFlags,
}
impl Default for MetadataOptions {
    fn default() -> Self {
        Self {
            fields: crate::MetadataFields::stat(),
            flags: MetadataFlags::new(),
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
        self.flags.set_follow(follow);
        self
    }
    /// Requested attributes; unavailable optional attributes remain absent.
    pub fn requested_fields(self) -> crate::MetadataFields {
        self.fields
    }
    /// Whether to follow the final component if it is a symlink.
    pub fn follows_symlinks(self) -> bool {
        self.flags.follow()
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

#[cfg(test)]
mod option_layout_tests {
    use super::*;
    #[test]
    fn packed_follow_flag_preserves_selected_fields() {
        assert_eq!(std::mem::size_of::<MetadataFlags>(), 1);
        assert!(MetadataOptions::default().follows_symlinks());
        let fields = crate::MetadataFields::SIZE | crate::MetadataFields::CHANGE;
        let options = MetadataOptions::new().fields(fields);
        for follow in [false, true, false] {
            let changed = options.follow_symlinks(follow);
            assert_eq!(changed.follows_symlinks(), follow);
            assert_eq!(changed.requested_fields(), fields);
            assert!(options.follows_symlinks());
        }
    }
}
