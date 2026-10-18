//! Linux NFS mount discovery and local-to-remote path mapping.
use crate::Result;
use crate::helpers::{PathMapper, ResolvePath};
pub use crate::native_nfs::NfsMount;
use std::path::{Path, PathBuf};

/// A direct NFS connection and the local-to-remote namespace mapping for one
/// discovered Linux mount directory.
///
/// Use this when an application needs both mount details (for grouping or
/// diagnostics) and a direct NFS client. It discovers the mount once, opens
/// from that pinned configuration, and maps operands relative to the supplied
/// local directory. Access through this session does not share the kernel NFS
/// client's cache or state.
#[cfg(all(feature = "nfs", target_os = "linux"))]
#[derive(Debug)]
pub struct NfsMountSession {
    mount: NfsMount,
    fs: super::NfsClient,
    paths: PathMapper,
}

#[cfg(all(feature = "nfs", target_os = "linux"))]
impl NfsMountSession {
    /// Discover and connect using an absolute directory on a supported NFSv4
    /// mount. Unsupported, ambiguous, stale, or insecurely configured mounts
    /// are errors; there is no AUTH_SYS downgrade or kernel fallback.
    pub fn from_mount(directory: impl AsRef<Path>) -> Result<Self> {
        super::NfsBuilder::from_mount(directory)?.connect_session()
    }

    /// Connect from pinned discovery information with default tuning.
    /// To customize, use `mount.builder()?.request_timeout(timeout).connect_session()`.
    pub fn from_discovered(mount: NfsMount) -> Result<Self> {
        mount.builder()?.connect_session()
    }

    pub(crate) fn from_connected(mount: NfsMount, fs: super::NfsClient) -> Self {
        let paths = PathMapper::from_validated_root(mount.local_path().to_path_buf());
        Self { mount, fs, paths }
    }

    /// The validated direct NFS client.
    pub fn fs(&self) -> &super::NfsClient {
        &self.fs
    }

    /// Information about the discovered kernel mount.
    pub fn mount(&self) -> &NfsMount {
        &self.mount
    }

    /// The local directory whose contents form this session's namespace root.
    pub fn local_root(&self) -> &Path {
        self.paths.local_root()
    }

    /// Map a local operand into this client's root-relative namespace.
    pub fn map(&self, path: impl AsRef<Path>, policy: ResolvePath) -> Result<PathBuf> {
        self.paths.map(path, policy)
    }

    /// Map a root-relative backend path back into the local namespace.
    pub fn local_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        self.paths.local_path(path)
    }
}
