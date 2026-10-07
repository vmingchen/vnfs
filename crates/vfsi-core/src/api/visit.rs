use crate::api::{Attributes, DepthLimit, ReadDirOptions, ResourceLimits, WalkOptions};
use std::num::NonZeroUsize;

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct VisitFlags {
    recursive: bool,
    truncate: bool,
    entries_unlimited: bool,
    bytes_unlimited: bool,
    enter_leave: bool,
    sort_by_name: bool,
    no_follow: bool,
    #[bits(1)]
    _reserved: u8,
}

/// Shared directory traversal policy for collection and paged visiting.
/// Defaults to immediate children only. Collection retains native batching;
/// visiting delivers owned directory pages incrementally through callbacks.
/// Unspecified budgets inherit the client's resource limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListDirOptions {
    flags: VisitFlags,
    entries: Option<NonZeroUsize>,
    bytes: Option<NonZeroUsize>,
    depth: Option<DepthLimit>,
    fields: Option<Attributes>,
}

impl ListDirOptions {
    /// Explicitly disable entry, path-byte and depth limits. This can permit
    /// unbounded collection or traversal; prefer finite application budgets.
    pub const fn unlimited() -> Self {
        let mut flags = VisitFlags::new();
        flags.set_entries_unlimited(true);
        flags.set_bytes_unlimited(true);
        Self {
            flags,
            entries: None,
            bytes: None,
            depth: Some(DepthLimit::unlimited()),
            fields: None,
        }
    }
    pub const fn new() -> Self {
        Self {
            flags: VisitFlags::new(),
            entries: None,
            bytes: None,
            depth: None,
            fields: None,
        }
    }
    /// Descend into directories, without following entry symlinks.
    pub const fn recursive(mut self, value: bool) -> Self {
        self.flags.set_recursive(value);
        self
    }
    /// Emit directory Enter/Leave events, including the root, in
    /// [`super::VfsiExt::listdir`]. Otherwise only child Entry events are emitted.
    /// Lifecycle traversal uses bounded directory buffers for depth-first order.
    /// This does not change the page events produced by [`super::Vfsi::vlistdirs`].
    pub const fn enter_leave(mut self, value: bool) -> Self {
        self.flags.set_enter_leave(value);
        self
    }
    /// Sort siblings by filename in [`super::VfsiExt::listdir`]. Sorting
    /// requires a complete, bounded directory listing before delivering entries.
    /// Page primitives and collecting helpers retain backend-defined ordering.
    pub const fn sort_by_name(mut self, value: bool) -> Self {
        self.flags.set_sort_by_name(value);
        self
    }
    /// Whether resolving an input directory path may follow symlinks (default:
    /// true). False requires no-follow resolution, including parent components;
    /// backends unable to guarantee it return Unsupported. Recursive entry
    /// symlinks are never traversed regardless of this setting. Buffered
    /// recursive/lifecycle `listdir` enforces false when reopening directories.
    pub const fn follow_symlinks(mut self, value: bool) -> Self {
        self.flags.set_no_follow(!value);
        self
    }
    /// Shared entry budget across every input root.
    pub const fn max_entries(mut self, value: usize) -> Self {
        self.entries = match value.checked_add(1) {
            Some(encoded) => NonZeroUsize::new(encoded),
            None => None,
        };
        self.flags.set_entries_unlimited(value == usize::MAX);
        self
    }
    /// Shared path-byte budget. Recursive visiting also charges root paths.
    pub const fn max_path_bytes(mut self, value: usize) -> Self {
        self.bytes = match value.checked_add(1) {
            Some(encoded) => NonZeroUsize::new(encoded),
            None => None,
        };
        self.flags.set_bytes_unlimited(value == usize::MAX);
        self
    }
    /// Root listing is depth 0; depth 1 also lists immediate subdirectories.
    /// In recursive mode, encountering a directory beyond this limit is an
    /// error unless `truncate_at_max_depth(true)` is selected. Shallow mode
    /// ignores depth settings and intentionally visits only immediate children.
    /// Finite depths range from 0 through 200; any larger value disables
    /// the depth limit. Entry and path-byte budgets remain in effect.
    pub const fn max_depth(mut self, value: usize) -> Self {
        self.depth = Some(DepthLimit::new(value));
        self
    }
    /// Intentionally stop descending at the depth limit instead of failing.
    pub const fn truncate_at_max_depth(mut self, value: bool) -> Self {
        self.flags.set_truncate(value);
        self
    }
    /// Select metadata returned with directory pages, without per-entry stat
    /// calls. Defaults to common stat fields. MODE is always requested for
    /// traversal; backends may return additional fields or omit unsupported ones.
    pub const fn fields(mut self, value: Attributes) -> Self {
        self.fields = Some(value);
        self
    }
    pub const fn is_recursive(self) -> bool {
        self.flags.recursive()
    }
    pub const fn emits_enter_leave(self) -> bool {
        self.flags.enter_leave()
    }
    pub const fn sorts_by_name(self) -> bool {
        self.flags.sort_by_name()
    }
    pub const fn follows_symlinks(self) -> bool {
        !self.flags.no_follow()
    }
    pub fn attributes(self) -> Attributes {
        self.fields.unwrap_or_else(|| {
            Attributes::stat()
                | Attributes::UID
                | Attributes::GID
                | Attributes::ATIME
                | Attributes::MTIME
                | Attributes::CTIME
                | Attributes::CHANGE
        }) | Attributes::MODE
    }
    /// Resolve recursive traversal limits against the client's defaults.
    pub fn walk_options(self, limits: ResourceLimits) -> WalkOptions {
        let mut options = limits.walk_options();
        if self.flags.entries_unlimited() {
            options = options.max_entries(usize::MAX);
        } else if let Some(encoded) = self.entries {
            options = options.max_entries(encoded.get() - 1);
        }
        if self.flags.bytes_unlimited() {
            options = options.max_path_bytes(usize::MAX);
        } else if let Some(encoded) = self.bytes {
            options = options.max_path_bytes(encoded.get() - 1);
        }
        if let Some(depth) = self.depth {
            options = options.max_depth(depth.get());
        }
        options.truncate_at_max_depth(self.flags.truncate())
    }
    /// Resolve shallow listing limits against the client's defaults.
    pub fn directory_options(self, limits: ResourceLimits) -> ReadDirOptions {
        let mut options = limits.directory_options();
        if self.flags.entries_unlimited() {
            options = options.max_entries(usize::MAX);
        } else if let Some(encoded) = self.entries {
            options = options.max_entries(encoded.get() - 1);
        }
        if self.flags.bytes_unlimited() {
            options = options.max_path_bytes(usize::MAX);
        } else if let Some(encoded) = self.bytes {
            options = options.max_path_bytes(encoded.get() - 1);
        }
        options
    }
}

impl From<ReadDirOptions> for ListDirOptions {
    fn from(options: ReadDirOptions) -> Self {
        Self::new()
            .max_entries(options.entry_limit())
            .max_path_bytes(options.path_byte_limit())
    }
}
impl From<WalkOptions> for ListDirOptions {
    fn from(options: WalkOptions) -> Self {
        Self::new()
            .recursive(true)
            .max_entries(options.entry_limit())
            .max_path_bytes(options.path_byte_limit())
            .max_depth(options.depth_limit())
            .truncate_at_max_depth(options.truncates_at_depth_limit())
    }
}

#[cfg(test)]
mod option_layout_tests {
    use super::*;
    #[allow(dead_code)]
    struct PreviousLayout {
        recursive: bool,
        entries: Option<usize>,
        bytes: Option<usize>,
        depth: Option<usize>,
        truncate: bool,
        fields: Option<Attributes>,
    }
    #[test]
    fn compact_traversal_options_preserve_zero_overrides_and_independent_bits() {
        assert_eq!(std::mem::size_of::<VisitFlags>(), 1);
        assert!(std::mem::size_of::<ListDirOptions>() < std::mem::size_of::<PreviousLayout>());
        let limits = ResourceLimits::default();
        let inherited = ListDirOptions::new().walk_options(limits);
        assert!(!ListDirOptions::new().emits_enter_leave());
        assert!(!ListDirOptions::new().sorts_by_name());
        assert!(ListDirOptions::new().follows_symlinks());
        assert_eq!(inherited.entry_limit(), limits.walk_options().entry_limit());
        assert_eq!(inherited.depth_limit(), limits.walk_options().depth_limit());
        for value in [0, 1, 200, usize::MAX] {
            let options = ListDirOptions::new()
                .max_entries(value)
                .max_path_bytes(value)
                .max_depth(value)
                .recursive(true)
                .enter_leave(true)
                .sort_by_name(true)
                .follow_symlinks(false)
                .truncate_at_max_depth(true)
                .fields(Attributes::SIZE);
            let walk = options.walk_options(limits);
            assert_eq!(walk.entry_limit(), value);
            assert_eq!(walk.path_byte_limit(), value);
            assert_eq!(walk.depth_limit(), value);
            assert!(walk.truncates_at_depth_limit());
            assert!(options.is_recursive());
            assert!(options.emits_enter_leave());
            assert!(options.sorts_by_name());
            assert!(!options.follows_symlinks());
            assert_eq!(options.attributes(), Attributes::SIZE | Attributes::MODE);
            let changed = options
                .recursive(false)
                .truncate_at_max_depth(false)
                .enter_leave(false)
                .sort_by_name(false);
            let changed = changed.follow_symlinks(true);
            assert!(!changed.is_recursive());
            assert!(!changed.emits_enter_leave());
            assert!(!changed.sorts_by_name());
            assert!(changed.follows_symlinks());
            assert!(!changed.walk_options(limits).truncates_at_depth_limit());
            assert_eq!(changed.directory_options(limits).entry_limit(), value);
            let zero = changed.max_entries(0).max_path_bytes(0).max_depth(0);
            assert_eq!(zero.walk_options(limits).entry_limit(), 0);
            assert_eq!(zero.walk_options(limits).path_byte_limit(), 0);
            assert_eq!(zero.walk_options(limits).depth_limit(), 0);
        }
        assert_eq!(
            ListDirOptions::unlimited()
                .walk_options(limits)
                .depth_limit(),
            usize::MAX
        );
        eprintln!(
            "ListDirOptions: {} -> {} bytes",
            std::mem::size_of::<PreviousLayout>(),
            std::mem::size_of::<ListDirOptions>()
        );
    }
}
