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

/// One attribute update for a path or opened object.
///
/// Paths can be supplied directly. Use [`super::Target`] for handles
/// or mixed path/handle batches. Unspecified fields are unchanged; construction
/// performs no I/O. Final symlinks are followed by default.
#[derive(Clone, Copy, Debug)]
pub struct SetAttrsOp<T> {
    target: T,
    permissions: Option<crate::Permissions>,
    uid: Option<u32>,
    gid: Option<u32>,
    len: Option<u64>,
    accessed: Option<std::time::SystemTime>,
    modified: Option<std::time::SystemTime>,
    flags: AttrsFlags,
}

impl<T> SetAttrsOp<T> {
    /// Prepare an update, following the final symlink for path targets.
    pub fn new(target: T) -> Self {
        Self {
            target,
            permissions: None,
            uid: None,
            gid: None,
            len: None,
            accessed: None,
            modified: None,
            flags: AttrsFlags::new(),
        }
    }

    /// Follow the final path symlink when true, or modify the link itself when
    /// false. Opened objects retain their identity regardless of this setting.
    pub fn follow_symlinks(mut self, follow: bool) -> Self {
        self.flags.set_follow(follow);
        self
    }

    /// The path or opened-object operand.
    pub fn target(&self) -> &T {
        &self.target
    }

    /// Change permissions, leaving other unspecified attributes unchanged.
    pub fn permissions(mut self, permissions: crate::Permissions) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// Change the owner. `u32::MAX` is reserved and rejected before dispatch.
    pub fn uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Change the group. `u32::MAX` is reserved and rejected before dispatch.
    pub fn gid(mut self, gid: u32) -> Self {
        self.gid = Some(gid);
        self
    }

    /// Truncate or extend the file. Zero explicitly truncates it to an empty file.
    pub fn len(mut self, len: u64) -> Self {
        self.len = Some(len);
        self
    }

    /// Set the access timestamp.
    pub fn accessed(mut self, accessed: std::time::SystemTime) -> Self {
        self.accessed = Some(accessed);
        self
    }

    /// Set the modification timestamp.
    pub fn modified(mut self, modified: std::time::SystemTime) -> Self {
        self.modified = Some(modified);
        self
    }

    /// Reuse all requested changes and the symlink policy with another target.
    /// This constructs an operation without I/O or cloning the original target.
    pub fn with_target<U>(&self, target: U) -> SetAttrsOp<U> {
        SetAttrsOp {
            target,
            permissions: self.permissions,
            uid: self.uid,
            gid: self.gid,
            len: self.len,
            accessed: self.accessed,
            modified: self.modified,
            flags: self.flags,
        }
    }

    /// Requested permission change, or `None` to leave permissions unchanged.
    pub fn requested_permissions(&self) -> Option<crate::Permissions> {
        self.permissions
    }
    /// Requested owner change, or `None` to leave the owner unchanged.
    pub fn requested_uid(&self) -> Option<u32> {
        self.uid
    }
    /// Requested group change, or `None` to leave the group unchanged.
    pub fn requested_gid(&self) -> Option<u32> {
        self.gid
    }
    /// Requested length, including zero, or `None` to leave the length unchanged.
    pub fn requested_len(&self) -> Option<u64> {
        self.len
    }
    /// Requested access timestamp, or `None` to leave it unchanged.
    pub fn requested_accessed(&self) -> Option<std::time::SystemTime> {
        self.accessed
    }
    /// Requested modification timestamp, or `None` to leave it unchanged.
    pub fn requested_modified(&self) -> Option<std::time::SystemTime> {
        self.modified
    }

    /// Whether the final path symlink is followed.
    pub fn follows_symlinks(&self) -> bool {
        self.flags.follow()
    }
}

impl<'a, F> SetAttrsOp<super::Target<'a, F>> {
    /// Prepare an update for an opened object without borrowing its pathname.
    /// The handle remains owned by the caller and is checked at dispatch.
    ///
    /// ```no_run
    /// use vfsi_core::api::{SetAttrsOp, Vfsi};
    /// # fn example<F: Vfsi>(fs: &F, file: &F::File) -> vfsi_core::api::Result<()> {
    /// fs.vsetattrs(&[SetAttrsOp::file(file).len(1024)])?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn file(file: &'a F) -> Self {
        Self::new(super::Target::File(file))
    }
}

#[cfg(test)]
mod option_layout_tests {
    use super::*;
    #[test]
    fn retargeting_preserves_all_changes_without_cloning_the_target() {
        struct NonClone;
        let permissions = crate::Permissions::from_mode(0o640);
        let accessed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(123);
        let modified = std::time::UNIX_EPOCH - std::time::Duration::from_secs(456);
        let empty = SetAttrsOp::new(NonClone);
        assert!(empty.follows_symlinks());
        assert_eq!(empty.requested_permissions(), None);
        assert_eq!(empty.requested_len(), None);
        assert_eq!(empty.requested_uid(), None);
        assert_eq!(empty.requested_gid(), None);
        assert_eq!(empty.requested_accessed(), None);
        assert_eq!(empty.requested_modified(), None);
        let op = empty
            .permissions(permissions)
            .len(0)
            .uid(42)
            .gid(43)
            .accessed(accessed)
            .modified(modified)
            .follow_symlinks(false);
        for path in ["/first", "/second"] {
            let rebound = op.with_target(path);
            assert_eq!(*rebound.target(), path);
            assert_eq!(rebound.requested_permissions(), Some(permissions));
            assert_eq!(rebound.requested_len(), Some(0));
            assert_eq!(rebound.requested_uid(), Some(42));
            assert_eq!(rebound.requested_gid(), Some(43));
            assert_eq!(rebound.requested_accessed(), Some(accessed));
            assert_eq!(rebound.requested_modified(), Some(modified));
            assert!(!rebound.follows_symlinks());
        }
        let replaced = op
            .with_target("/third")
            .len(10)
            .uid(44)
            .follow_symlinks(true);
        assert_eq!(replaced.requested_len(), Some(10));
        assert_eq!(replaced.requested_uid(), Some(44));
        assert!(replaced.follows_symlinks());
        assert_eq!(op.requested_len(), Some(0));
        assert_eq!(op.requested_uid(), Some(42));
        assert!(!op.follows_symlinks());
    }
    #[test]
    fn packed_follow_flag_preserves_selected_fields() {
        assert_eq!(std::mem::size_of::<AttrsFlags>(), 1);
        assert!(AttrsOptions::default().follows_symlinks());
        let op = SetAttrsOp::new("/file").len(0).uid(42);
        assert_eq!(*op.target(), "/file");
        assert!(op.follows_symlinks());
        let op = op.follow_symlinks(false);
        assert!(!op.follows_symlinks());
        assert_eq!(op.requested_len(), Some(0));
        assert_eq!(op.requested_uid(), Some(42));
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
