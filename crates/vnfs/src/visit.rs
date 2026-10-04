use crate::{MetadataFields, ReadDirOptions, ResourceLimits, WalkOptions};

/// Bounded, paged directory visiting. Defaults to immediate children only.
/// Unspecified budgets inherit the client's resource limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisitOptions {
    recursive: bool,
    entries: Option<usize>,
    bytes: Option<usize>,
    depth: Option<usize>,
    truncate: bool,
    fields: Option<MetadataFields>,
}

impl VisitOptions {
    pub const fn new() -> Self {
        Self {
            recursive: false,
            entries: None,
            bytes: None,
            depth: None,
            truncate: false,
            fields: None,
        }
    }
    /// Descend into directories, without following entry symlinks.
    pub const fn recursive(mut self, value: bool) -> Self {
        self.recursive = value;
        self
    }
    /// Shared entry budget across every input root.
    pub const fn max_entries(mut self, value: usize) -> Self {
        self.entries = Some(value);
        self
    }
    /// Shared path-byte budget. Recursive visiting also charges root paths.
    pub const fn max_path_bytes(mut self, value: usize) -> Self {
        self.bytes = Some(value);
        self
    }
    /// Root listing is depth 0; depth 1 also lists immediate subdirectories.
    /// In recursive mode, encountering a directory beyond this limit is an
    /// error unless `truncate_at_max_depth(true)` is selected. Shallow mode
    /// ignores depth settings and intentionally visits only immediate children.
    pub const fn max_depth(mut self, value: usize) -> Self {
        self.depth = Some(value);
        self
    }
    /// Intentionally stop descending at the depth limit instead of failing.
    pub const fn truncate_at_max_depth(mut self, value: bool) -> Self {
        self.truncate = value;
        self
    }
    /// Select metadata returned with directory pages, without per-entry stat
    /// calls. Defaults to common stat fields. MODE is always requested for
    /// traversal; backends may return additional fields or omit unsupported ones.
    pub const fn fields(mut self, value: MetadataFields) -> Self {
        self.fields = Some(value);
        self
    }
    pub const fn is_recursive(self) -> bool {
        self.recursive
    }
    pub fn metadata_fields(self) -> MetadataFields {
        self.fields.unwrap_or_else(|| {
            MetadataFields::stat()
                | MetadataFields::UID
                | MetadataFields::GID
                | MetadataFields::ATIME
                | MetadataFields::MTIME
                | MetadataFields::CTIME
                | MetadataFields::CHANGE
        }) | MetadataFields::MODE
    }
    pub(crate) fn walk_options(self, limits: ResourceLimits) -> WalkOptions {
        let mut options = limits.walk_options();
        if let Some(v) = self.entries {
            options = options.max_entries(v);
        }
        if let Some(v) = self.bytes {
            options = options.max_path_bytes(v);
        }
        if let Some(v) = self.depth {
            options = options.max_depth(v);
        }
        options.truncate_at_max_depth(self.truncate)
    }
    pub(crate) fn directory_options(self, limits: ResourceLimits) -> ReadDirOptions {
        let mut options = limits.directory_options();
        if let Some(v) = self.entries {
            options = options.max_entries(v);
        }
        if let Some(v) = self.bytes {
            options = options.max_path_bytes(v);
        }
        options
    }
}

impl From<ReadDirOptions> for VisitOptions {
    fn from(options: ReadDirOptions) -> Self {
        Self::new()
            .max_entries(options.entry_limit())
            .max_path_bytes(options.path_byte_limit())
    }
}
impl From<WalkOptions> for VisitOptions {
    fn from(options: WalkOptions) -> Self {
        Self::new()
            .recursive(true)
            .max_entries(options.entry_limit())
            .max_path_bytes(options.path_byte_limit())
            .max_depth(options.depth_limit())
            .truncate_at_max_depth(options.truncates_at_depth_limit())
    }
}
