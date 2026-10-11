//! Linux io_uring execution of independent descriptor reads, writes and fsyncs.
//!
//! [`connect`] returns a client implementing [`vfsi_core::Vfsi`] and
//! [`vfsi_core::VfsiExt`]. The local backend retains namespace resolution,
//! descriptor ownership, bounded directory paging and ordered dependent I/O.
//! Open/close, path writes, append, overlapping writes, and namespace operations
//! use ordinary syscalls. This is synchronous batching, not an async runtime,
//! direct I/O or zero-copy API. Completion order never changes result order.
//! Opt-in cache-read and syscall-write paths avoid ring overhead for warm
//! buffered I/O. Their counters are separate from actual SQEs and CQEs.
//! Owned read cohorts fitting the configured byte window use their final result
//! allocations directly. Borrowed destinations and larger cohorts retain
//! bounded scratch storage; neither path is kernel zero-copy.
//!
//! Ring setup failures (including sandbox/seccomp restrictions) are returned;
//! there is no silent fallback to a different backend.
#![cfg(target_os = "linux")]

mod engine;

use std::{
    num::{NonZeroU32, NonZeroUsize},
    path::Path,
};
use vfsi_core::{VfError, VfResult};

#[bitfields::bitfield(u8)]
struct Flags {
    cached_reads: bool,
    syscall_writes: bool,
    #[bits(6)]
    _reserved: u8,
}

/// Bounds both in-flight SQEs and the reusable owned scratch arena. No SQPOLL
/// thread is started. A large operation is split into bounded contiguous chunks.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub(crate) queue_depth: NonZeroU32,
    pub(crate) max_batch_bytes: NonZeroUsize,
    pub(crate) flags: Flags,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            queue_depth: NonZeroU32::new(256).unwrap(),
            max_batch_bytes: NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
            flags: Flags::new(),
        }
    }
}

impl Options {
    /// Execute buffered positional writes with ordered syscalls, avoiding
    /// worker scheduling and the kernel-facing scratch copy. Disabled by
    /// default; reads and fsyncs still use the ring unless separately configured.
    pub fn syscall_writes(mut self, enabled: bool) -> Self {
        self.flags.set_syscall_writes(enabled);
        self
    }
    /// Require two kernel-page-cache probe hits per cohort of 16 small reads
    /// (<=64 KiB) before selecting ordinary positional reads. A miss switches
    /// the rest of the call to bounded ring batching. A mixed/evicted cohort may
    /// block for at most 14 unprobed reads before checking again. Large reads
    /// always use the ring; partial or zero probe replies never imply EOF.
    /// Disabled by default; unsupported filesystems retain ordinary ring reads.
    pub fn cached_reads(mut self, enabled: bool) -> Self {
        self.flags.set_cached_reads(enabled);
        self
    }
    /// Maximum submissions in a wave (1..=4096). Oversized values fail at
    /// connection rather than allocating an arbitrarily large kernel ring.
    pub fn queue_depth(mut self, depth: NonZeroU32) -> Self {
        self.queue_depth = depth;
        self
    }
    /// Maximum aggregate transfer bytes per wave, also bounding scratch storage,
    /// independent of caller-facing
    /// read allocation limits. Shared read limits still default to 16 MiB.
    /// Defaults to 2 MiB to limit copy working sets while preserving many-file
    /// concurrency; tune upward for storage that benefits from larger windows.
    pub fn max_batch_bytes(mut self, bytes: NonZeroUsize) -> Self {
        self.max_batch_bytes = bytes;
        self
    }
}

pub use engine::{Stats, Telemetry};

/// Construct a rooted VFSI client. Roots follow the local backend's semantics
/// (including creating a missing root); configure [`vfsi_core::api::ResourceLimits`]
/// on the returned client to change application allocation/traversal budgets.
pub fn connect(
    root: impl AsRef<Path>,
    options: Options,
) -> VfResult<vfsi_sync::FsClient<vfsi_local::LocalBackend>> {
    connect_with_telemetry(root, options).map(|(fs, _)| fs)
}

/// Like [`connect`], retaining low-cost counters for actual submissions,
/// completions, waves, ring entries, arena growths and peak scratch bytes.
/// Ring counters do not include syscalls. Opt-in cache probes and descriptor
/// syscall requests have separate counters; namespace/dependency fallbacks
/// remain excluded.
pub fn connect_with_telemetry(
    root: impl AsRef<Path>,
    options: Options,
) -> VfResult<(vfsi_sync::FsClient<vfsi_local::LocalBackend>, Telemetry)> {
    let (engine, telemetry) = engine::Engine::new(options).map_err(|e| {
        VfError::client(0, e.raw_os_error().unwrap_or(libc::EIO) as u32)
            .with_context("io_uring_setup", root.as_ref())
    })?;
    let backend = vfsi_local::LocalBackend::new(root.as_ref().to_path_buf(), engine)?;
    Ok((vfsi_sync::FsClient::new(backend), telemetry))
}
