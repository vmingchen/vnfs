//! Linux io_uring execution of independent descriptor reads, writes and fsyncs.
//!
//! [`connect`] returns a client implementing [`vfsi_core::Vfsi`] and
//! [`vfsi_core::VfsiExt`]. The local backend retains namespace resolution,
//! descriptor ownership, bounded directory paging and ordered dependent I/O.
//! Open/close, path writes, append, overlapping writes, and namespace operations
//! use ordinary syscalls. This is synchronous batching, not an async runtime,
//! direct I/O or zero-copy API. Completion order never changes result order.
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

/// Bounds both in-flight SQEs and owned scratch storage. No SQPOLL thread is
/// started. A large operation is split into bounded contiguous chunks.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub(crate) queue_depth: NonZeroU32,
    pub(crate) max_batch_bytes: NonZeroUsize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            queue_depth: NonZeroU32::new(256).unwrap(),
            max_batch_bytes: NonZeroUsize::new(16 * 1024 * 1024).unwrap(),
        }
    }
}

impl Options {
    /// Maximum submissions in a wave (1..=4096). Oversized values fail at
    /// connection rather than allocating an arbitrarily large kernel ring.
    pub fn queue_depth(mut self, depth: NonZeroU32) -> Self {
        self.queue_depth = depth;
        self
    }
    /// Maximum aggregate scratch bytes per wave, independent of caller-facing
    /// read allocation limits. Shared read limits still default to 16 MiB.
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
) -> VfResult<vfsi_sync::FsClient<vfsi_local::DummyVecFs>> {
    connect_with_telemetry(root, options).map(|(fs, _)| fs)
}

/// Like [`connect`], retaining low-cost counters for actual submissions,
/// completions, waves and peak scratch bytes. Counters do not include syscalls
/// performed by the local namespace/ordering fallback.
pub fn connect_with_telemetry(
    root: impl AsRef<Path>,
    options: Options,
) -> VfResult<(vfsi_sync::FsClient<vfsi_local::DummyVecFs>, Telemetry)> {
    let (engine, telemetry) = engine::Engine::new(options).map_err(|e| {
        VfError::client(0, e.raw_os_error().unwrap_or(libc::EIO) as u32)
            .with_context("io_uring_setup", root.as_ref())
    })?;
    let backend =
        vfsi_local::DummyVecFs::try_new(root.as_ref().to_path_buf())?.with_io_engine(engine);
    Ok((vfsi_sync::FsClient::new(backend), telemetry))
}
