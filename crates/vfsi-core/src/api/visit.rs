use crate::api::{Attributes, DepthLimit, ResourceLimits};
use std::num::NonZeroUsize;

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct VisitFlags {
    recursive: bool,
    truncate: bool,
    entries_unlimited: bool,
    bytes_unlimited: bool,
    no_follow: bool,
    #[bits(3)]
    _reserved: u8,
}

/// Shared directory traversal policy for collection and paged visiting.
/// Defaults to immediate children only. Collection retains native batching;
/// visiting delivers owned directory pages incrementally through callbacks.
/// Unspecified budgets inherit the client's resource limits.
/// Require no symlink components when false; metadata's final-component
/// option has a deliberately different name.
///
/// ```compile_fail
/// use vfsi_core::api::ListDirOptions;
/// ListDirOptions::new().with_follow_symlinks(false);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use = "configuration and requests have no effect until used"]
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
    pub const fn with_recursive(mut self, value: bool) -> Self {
        self.flags.set_recursive(value);
        self
    }
    /// Whether resolving an input directory path may follow symlinks (default:
    /// true). False requires no-follow resolution, including parent components;
    /// backends unable to guarantee it return Unsupported. Recursive entry
    /// symlinks are never traversed regardless of this setting. Buffered
    /// recursive/lifecycle `listdir` enforces false when reopening directories.
    pub const fn with_allow_symlink_components(mut self, value: bool) -> Self {
        self.flags.set_no_follow(!value);
        self
    }
    /// Shared entry budget across every input root.
    pub const fn with_max_entries(mut self, value: usize) -> Self {
        self.entries = match value.checked_add(1) {
            Some(encoded) => NonZeroUsize::new(encoded),
            None => None,
        };
        self.flags.set_entries_unlimited(value == usize::MAX);
        self
    }
    /// Shared path-byte budget. Recursive visiting also charges root paths.
    pub const fn with_max_path_bytes(mut self, value: usize) -> Self {
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
    /// Use a checked finite depth or explicitly select an unlimited depth.
    pub const fn with_max_depth(mut self, value: DepthLimit) -> Self {
        self.depth = Some(value);
        self
    }
    /// Intentionally stop descending at the depth limit instead of failing.
    pub const fn with_truncate_at_max_depth(mut self, value: bool) -> Self {
        self.flags.set_truncate(value);
        self
    }
    /// Select metadata returned with directory pages, without per-entry stat
    /// calls. Defaults to common stat fields. MODE is always requested for
    /// traversal; backends may return additional fields or omit unsupported ones.
    pub const fn with_fields(mut self, value: Attributes) -> Self {
        self.fields = Some(value);
        self
    }
    pub const fn recursive(self) -> bool {
        self.flags.recursive()
    }
    pub const fn allow_symlink_components(self) -> bool {
        !self.flags.no_follow()
    }
    pub fn fields(self) -> Attributes {
        self.fields.unwrap_or_else(Attributes::stat) | Attributes::MODE
    }
    /// Configured override; `None` inherits the client's entry budget.
    pub fn max_entries(self) -> Option<usize> {
        if self.flags.entries_unlimited() {
            Some(usize::MAX)
        } else {
            self.entries.map(|v| v.get() - 1)
        }
    }
    /// Configured override; `None` inherits the client's path-byte budget.
    pub fn max_path_bytes(self) -> Option<usize> {
        if self.flags.bytes_unlimited() {
            Some(usize::MAX)
        } else {
            self.bytes.map(|v| v.get() - 1)
        }
    }
    /// Configured override; `None` inherits the client's descent limit.
    pub const fn max_depth(self) -> Option<DepthLimit> {
        self.depth
    }
    pub fn max_entries_or(self, default: usize) -> usize {
        self.max_entries().unwrap_or(default)
    }
    pub fn max_path_bytes_or(self, default: usize) -> usize {
        self.max_path_bytes().unwrap_or(default)
    }
    pub fn max_depth_or(self, default: DepthLimit) -> DepthLimit {
        self.max_depth().unwrap_or(default)
    }
    pub const fn truncate_at_max_depth(self) -> bool {
        self.flags.truncate()
    }
    /// Apply client defaults while preserving explicit zero and unlimited budgets.
    pub fn resolve(mut self, limits: ResourceLimits) -> Self {
        let entries = self.max_entries_or(limits.max_directory_entries());
        let bytes = self.max_path_bytes_or(limits.max_directory_path_bytes());
        self.depth = Some(self.max_depth_or(limits.max_walk_depth()));
        self.with_max_entries(entries).with_max_path_bytes(bytes)
    }
    /// Resolve a recursive traversal against the client's defaults.
    pub fn walk_options(self, limits: ResourceLimits) -> Self {
        self.with_recursive(true).resolve(limits)
    }
    /// Resolve a shallow listing against the client's defaults.
    pub fn directory_options(self, limits: ResourceLimits) -> Self {
        self.with_recursive(false).resolve(limits)
    }
}

/// Event-walk presentation policy, separate from native directory-page options.
/// Sorting and lifecycle events buffer complete bounded directory listings.
///
/// ```compile_fail
/// use vfsi_core::api::ListDirOptions;
/// ListDirOptions::new().with_sort_by_name(true);
/// ```
/// ```compile_fail
/// use vfsi_core::api::{Vfsi, WalkOptions, ListDirOptions};
/// fn invalid(fs: &impl Vfsi) {
///     fs.vlistdirs(&["/"], WalkOptions::new(ListDirOptions::new()), |_, _| {
///         Ok(std::ops::ControlFlow::Continue(()))
///     }).unwrap();
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use = "configuration and requests have no effect until used"]
pub struct WalkOptions {
    listing: ListDirOptions,
    enter_leave: bool,
    sort_by_name: bool,
}
impl WalkOptions {
    pub const fn new(listing: ListDirOptions) -> Self {
        Self {
            listing,
            enter_leave: false,
            sort_by_name: false,
        }
    }
    /// Replace shared listing policy without changing presentation settings.
    pub const fn with_listing_options(mut self, listing: ListDirOptions) -> Self {
        self.listing = listing;
        self
    }
    pub const fn listing_options(self) -> ListDirOptions {
        self.listing
    }
    /// Include directory Enter/Leave events, including the root.
    pub const fn with_enter_leave(mut self, value: bool) -> Self {
        self.enter_leave = value;
        self
    }
    /// Buffer and sort each directory by filename before emitting its events.
    pub const fn with_sort_by_name(mut self, value: bool) -> Self {
        self.sort_by_name = value;
        self
    }
    pub const fn enter_leave(self) -> bool {
        self.enter_leave
    }
    pub const fn sort_by_name(self) -> bool {
        self.sort_by_name
    }
}
impl From<ListDirOptions> for WalkOptions {
    fn from(listing: ListDirOptions) -> Self {
        Self::new(listing)
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
        let limits = ResourceLimits::new()
            .with_max_directory_entries(usize::MAX)
            .with_max_directory_path_bytes(13)
            .with_max_walk_depth(crate::api::DepthLimit::new(3).expect("finite depth"));
        let inherited = ListDirOptions::new().walk_options(limits);
        assert!(ListDirOptions::new().allow_symlink_components());
        assert_eq!(
            inherited.max_entries_or(crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
            limits
                .walk_options()
                .max_entries_or(crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES)
        );
        assert_eq!(
            inherited.max_depth_or(Default::default()).get(),
            limits.walk_options().max_depth_or(Default::default()).get()
        );
        assert_eq!(
            inherited.max_path_bytes_or(crate::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
            13
        );
        for value in [0, 1, 200, usize::MAX] {
            let options = ListDirOptions::new()
                .with_max_entries(value)
                .with_max_path_bytes(value)
                .with_max_depth(if value == usize::MAX {
                    crate::api::DepthLimit::unlimited()
                } else {
                    crate::api::DepthLimit::new(value).expect("finite depth")
                })
                .with_recursive(true)
                .with_allow_symlink_components(false)
                .with_truncate_at_max_depth(true)
                .with_fields(Attributes::SIZE);
            let walk = options.walk_options(limits);
            assert_eq!(
                walk.max_entries_or(crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
                value
            );
            assert_eq!(walk.fields(), options.fields());
            assert!(!walk.allow_symlink_components());
            assert_eq!(
                walk.max_path_bytes_or(crate::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
                value
            );
            assert_eq!(walk.max_depth_or(Default::default()).get(), value);
            assert!(walk.truncate_at_max_depth());
            assert!(options.recursive());
            assert!(!options.allow_symlink_components());
            assert_eq!(options.fields(), Attributes::SIZE | Attributes::MODE);
            let changed = options
                .with_recursive(false)
                .with_truncate_at_max_depth(false);
            let changed = changed.with_allow_symlink_components(true);
            assert!(!changed.recursive());
            assert!(changed.allow_symlink_components());
            assert!(!changed.walk_options(limits).truncate_at_max_depth());
            assert_eq!(
                changed
                    .directory_options(limits)
                    .max_entries_or(crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
                value
            );
            let zero = changed
                .with_max_entries(0)
                .with_max_path_bytes(0)
                .with_max_depth(crate::api::DepthLimit::new(0).expect("finite depth"));
            assert_eq!(
                zero.walk_options(limits)
                    .max_entries_or(crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
                0
            );
            assert_eq!(
                zero.walk_options(limits)
                    .max_path_bytes_or(crate::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
                0
            );
            assert_eq!(
                zero.walk_options(limits)
                    .max_depth_or(Default::default())
                    .get(),
                0
            );
        }
        assert_eq!(
            ListDirOptions::unlimited()
                .walk_options(limits)
                .max_depth_or(Default::default())
                .get(),
            usize::MAX
        );
        eprintln!(
            "ListDirOptions: {} -> {} bytes",
            std::mem::size_of::<PreviousLayout>(),
            std::mem::size_of::<ListDirOptions>()
        );
    }
}
