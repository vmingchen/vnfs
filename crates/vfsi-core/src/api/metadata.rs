//! Options shared by scalar convenience and vector metadata queries.

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct AttrsFlags {
    #[bits(default = true)]
    follow: bool,
    #[bits(7)]
    _reserved: u8,
}

/// Select metadata fields and whether to follow the final symlink.
/// Ancestor symlinks use ordinary backend path-resolution semantics.
#[derive(Clone, Copy, Debug)]
pub struct AttrsOptions {
    fields: crate::api::Attributes,
    flags: AttrsFlags,
}
impl Default for AttrsOptions {
    fn default() -> Self {
        Self {
            fields: crate::api::Attributes::stat(),
            flags: AttrsFlags::new(),
        }
    }
}
impl AttrsOptions {
    /// Standard stat fields, following the final symlink.
    pub fn new() -> Self {
        Self::default()
    }
    /// Select requested attributes. Object type may additionally be fetched.
    pub fn fields(mut self, fields: crate::api::Attributes) -> Self {
        self.fields = fields;
        self
    }
    /// Follow the final symlink when true; otherwise describe the link itself.
    pub fn follow_symlinks(mut self, follow: bool) -> Self {
        self.flags.set_follow(follow);
        self
    }
    /// Requested attributes; unavailable optional attributes remain absent.
    pub fn requested_attributes(self) -> crate::api::Attributes {
        self.fields
    }
    /// Whether to follow the final component if it is a symlink.
    pub fn follows_symlinks(self) -> bool {
        self.flags.follow()
    }
}
#[cfg(test)]
mod option_layout_tests {
    use super::*;
    #[test]
    fn packed_follow_flag_preserves_selected_fields() {
        assert_eq!(std::mem::size_of::<AttrsFlags>(), 1);
        assert!(AttrsOptions::default().follows_symlinks());
        let fields = crate::api::Attributes::SIZE | crate::api::Attributes::CHANGE;
        let options = AttrsOptions::new().fields(fields);
        for follow in [false, true, false] {
            let changed = options.follow_symlinks(follow);
            assert_eq!(changed.follows_symlinks(), follow);
            assert_eq!(changed.requested_attributes(), fields);
            assert!(options.follows_symlinks());
        }
    }
}
