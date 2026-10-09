use super::*;
use std::path::PathBuf;

/// Default payload limit for APIs that allocate and return complete contents.
pub const DEFAULT_READ_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Default size of each chunk delivered by the bounded single-file stream API.
pub const DEFAULT_READ_STREAM_CHUNK_BYTES: usize = 1024 * 1024;

/// Default aggregate payload limit for complete-file vector reads.
pub const DEFAULT_READ_ALLV_MAX_TOTAL_BYTES: usize = DEFAULT_READ_MAX_BYTES;

/// Tuning options for bounded single-file streaming reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    chunk_size: usize,
}

impl StreamOptions {
    pub const fn new() -> Self {
        Self {
            chunk_size: DEFAULT_READ_STREAM_CHUNK_BYTES,
        }
    }

    /// Set the maximum bytes delivered to the callback at once.
    ///
    /// NFS and other backends may return smaller chunks due to negotiated
    /// protocol limits. A larger setting does not bypass those limits.
    pub const fn chunk_size(mut self, bytes: usize) -> Self {
        self.chunk_size = bytes;
        self
    }

    pub const fn chunk_size_bytes(self) -> usize {
        self.chunk_size
    }
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Default maximum number of entries returned by allocating directory APIs.
pub const DEFAULT_DIRECTORY_MAX_ENTRIES: usize = 100_000;

/// Default combined path-storage budget for allocating directory APIs.
pub const DEFAULT_DIRECTORY_MAX_PATH_BYTES: usize = 16 * 1024 * 1024;

/// Default recursion depth for recursive walks.
pub const DEFAULT_WALK_MAX_DEPTH: usize = 128;

/// Compact traversal depth: 0 through 200 are finite, larger inputs are unlimited.
/// The root has depth zero. Unlimited depth does not disable entry/byte budgets.
/// Both this type and `Option<DepthLimit>` occupy one byte.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepthLimit(std::num::NonZeroU8);

impl DepthLimit {
    /// Normalize a requested depth. Every value above 200 means unlimited.
    pub const fn new(depth: usize) -> Self {
        let encoded = if depth <= 200 { (depth + 1) as u8 } else { 202 };
        Self(std::num::NonZeroU8::new(encoded).expect("depth encoding is nonzero"))
    }
    /// Disable the depth limit while retaining other traversal budgets.
    pub const fn unlimited() -> Self {
        Self::new(usize::MAX)
    }
    /// Effective finite limit, or `usize::MAX` for unlimited traversal.
    pub const fn get(self) -> usize {
        if self.is_unlimited() {
            usize::MAX
        } else {
            self.0.get() as usize - 1
        }
    }
    /// Whether traversal has no depth limit.
    pub const fn is_unlimited(self) -> bool {
        self.0.get() == 202
    }
}

/// Default aggregate payload limit for owned vector reads.
pub const DEFAULT_READV_MAX_TOTAL_BYTES: usize = DEFAULT_READ_MAX_BYTES;

/// Application result for one positional vector read, in request order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsReadResult {
    pub offset: u64,
    pub data: Vec<u8>,
    pub eof: bool,
}

/// Application result for one read into caller storage, in request order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadIntoResult {
    pub offset: u64,
    pub read: usize,
    pub eof: bool,
}

/// Application result for one vector write, in request order.
/// For completed append writes, `offset + written` is the last reported end;
/// concurrent writers can interleave between short-write waves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteResult {
    pub offset: u64,
    pub written: usize,
    pub stable: bool,
}

/// Completion of a streaming read. Stopping does not mean EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCompletion {
    Complete,
    Stopped { next_offset: u64 },
}

/// Completion of a callback-based directory traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraversalCompletion {
    Complete,
    Stopped,
}

/// Default resource policy for a client. Per-call options override it.
/// Byte limits bound logical payload, not allocator capacity or RPC overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Maximum collected scalar payload or aggregate vector read buffers.
    /// Not a process-memory cap; standard `Read::read_to_end` and explicit
    /// positional file reads remain caller-managed.
    max_read_bytes: usize,
    /// Default request size for client `read_stream`, not file `Read` or pools.
    stream_chunk_bytes: std::num::NonZeroUsize,
    /// Aggregate entries delivered/collected by a directory call or tree walk.
    max_directory_entries: usize,
    /// Aggregate logical path bytes, excluding allocator and metadata overhead.
    max_directory_path_bytes: usize,
    /// Maximum descent depth; the starting directory is depth zero.
    max_walk_depth: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: DEFAULT_READ_MAX_BYTES,
            stream_chunk_bytes: std::num::NonZeroUsize::new(DEFAULT_READ_STREAM_CHUNK_BYTES)
                .unwrap(),
            max_directory_entries: crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES,
            max_directory_path_bytes: crate::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES,
            max_walk_depth: crate::api::DEFAULT_WALK_MAX_DEPTH,
        }
    }
}

impl ResourceLimits {
    pub fn new() -> Self {
        Self::default()
    }
    /// Set the collected read budget; zero permits only empty payloads.
    pub const fn max_read_bytes(mut self, value: usize) -> Self {
        self.max_read_bytes = value;
        self
    }
    /// Read the configured policy value.
    pub const fn read_byte_limit(self) -> usize {
        self.max_read_bytes
    }
    /// Set the default streaming chunk size. Nonzero chunks guarantee progress.
    pub const fn stream_chunk_bytes(mut self, value: std::num::NonZeroUsize) -> Self {
        self.stream_chunk_bytes = value;
        self
    }
    /// Read the configured policy value.
    pub const fn stream_chunk_size(self) -> usize {
        self.stream_chunk_bytes.get()
    }
    /// Set the aggregate directory entry budget; zero is a valid budget.
    pub const fn max_directory_entries(mut self, value: usize) -> Self {
        self.max_directory_entries = value;
        self
    }
    /// Read the configured policy value.
    pub const fn directory_entry_limit(self) -> usize {
        self.max_directory_entries
    }
    /// Set the aggregate path byte budget; zero is a valid budget.
    pub const fn max_directory_path_bytes(mut self, value: usize) -> Self {
        self.max_directory_path_bytes = value;
        self
    }
    /// Read the configured policy value.
    pub const fn directory_path_byte_limit(self) -> usize {
        self.max_directory_path_bytes
    }
    /// Set maximum descent depth; zero includes only the starting directory.
    pub const fn max_walk_depth(mut self, value: usize) -> Self {
        self.max_walk_depth = value;
        self
    }
    /// Read the configured policy value.
    pub const fn walk_depth_limit(self) -> usize {
        self.max_walk_depth
    }

    pub fn directory_options(self) -> ListDirOptions {
        ListDirOptions::new()
            .max_entries(self.max_directory_entries)
            .max_path_bytes(self.max_directory_path_bytes)
    }

    pub fn walk_options(self) -> crate::api::ListDirOptions {
        crate::api::ListDirOptions::new()
            .recursive(true)
            .max_entries(self.max_directory_entries)
            .max_path_bytes(self.max_directory_path_bytes)
            .max_depth(self.max_walk_depth)
    }
}

/// One directory and its entries, with attributes fetched during enumeration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryListing {
    pub path: PathBuf,
    pub entries: Vec<DirEntry>,
}
