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
        Self::from_mount_with(directory, |builder| builder)
    }

    /// Discover a mount and customize its builder before connecting.
    ///
    /// Adjust or clone the supplied builder; returning an independently
    /// constructed builder is an error. The mount root, authentication and
    /// protocol version must remain unchanged.
    pub fn from_mount_with(
        directory: impl AsRef<Path>,
        configure: impl FnOnce(super::NfsBuilder) -> super::NfsBuilder,
    ) -> Result<Self> {
        Self::from_discovered_with(super::Nfs::discover_mount(directory)?, configure)
    }

    /// Connect using mount information already returned by
    /// [`super::Nfs::discover_mount`]. This avoids repeating system mount-table
    /// discovery when an application first classifies or groups an operand.
    pub fn from_discovered(mount: NfsMount) -> Result<Self> {
        Self::from_discovered_with(mount, |builder| builder)
    }

    /// Connect from existing mount information after customizing the builder.
    ///
    /// The callback must retain the supplied builder's mount binding, as in
    /// [`Self::from_mount_with`]. Replacement builders are rejected before any
    /// connection is attempted.
    pub fn from_discovered_with(
        mount: NfsMount,
        configure: impl FnOnce(super::NfsBuilder) -> super::NfsBuilder,
    ) -> Result<Self> {
        let fs = mount
            .builder()?
            .configure_for_mount(mount.local_path(), configure)?
            .connect()?;
        // Discovery already canonicalized and validated this path, so avoid a
        // second host filesystem lookup while creating the mapping.
        let paths = PathMapper::from_validated_root(mount.local_path().to_path_buf());
        Ok(Self { mount, fs, paths })
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
