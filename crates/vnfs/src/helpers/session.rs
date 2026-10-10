//! Reusable path mapping for applications that opt into direct mount access.
use crate::{Error, Result};
use std::path::{Path, PathBuf};

fn io_error(path: &Path, error: std::io::Error) -> Error {
    Error::client(0, error.raw_os_error().unwrap_or(libc::EIO) as u32)
        .with_context("mount session", path)
}

/// Resolution policy for the final component of an application operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvePath {
    /// Resolve an existing operand, including its final symlink.
    Follow,
    /// Resolve only its parent. Suitable for links, removal and new destinations.
    NoFollow,
}

/// Standalone local-to-root-relative path mapping. It owns no filesystem and
/// makes no claim that a client is rooted here. For a validated direct NFS
/// connection plus mapping, use `NfsMountSession`. Mapping is a namespace
/// convention, not race-free confinement or kernel/direct-client coherence.
#[derive(Debug)]
pub struct PathMapper {
    local_root: PathBuf,
}
impl PathMapper {
    pub fn new(local_root: impl AsRef<Path>) -> Result<Self> {
        let requested = local_root.as_ref();
        let local_root =
            std::fs::canonicalize(requested).map_err(|error| io_error(requested, error))?;
        if !local_root.is_dir() {
            return Err(Error::client(0, libc::ENOTDIR as u32));
        }
        Ok(Self { local_root })
    }
    pub fn local_root(&self) -> &Path {
        &self.local_root
    }
    /// Translate a local operand without UTF-8 conversion. Relative operands
    /// use the process working directory; backend paths are always root-relative.
    pub fn map(&self, path: impl AsRef<Path>, policy: ResolvePath) -> Result<PathBuf> {
        let path = path.as_ref();
        let absolute = match policy {
            ResolvePath::Follow => {
                std::fs::canonicalize(path).map_err(|error| io_error(path, error))?
            }
            ResolvePath::NoFollow => {
                // Reject dot/dot-dot terminal operands instead of normalizing
                // destructive requests into a different object.
                let spelling = path.as_os_str().as_encoded_bytes();
                if spelling.ends_with(b"/.")
                    || spelling.ends_with(b"/..")
                    || spelling.ends_with(b"/")
                {
                    return Err(Error::client(0, libc::EINVAL as u32));
                }
                let name = path
                    .file_name()
                    .ok_or_else(|| Error::client(0, libc::EINVAL as u32))?;
                if path.components().next_back() != Some(std::path::Component::Normal(name)) {
                    return Err(Error::client(0, libc::EINVAL as u32));
                }
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                std::fs::canonicalize(parent)
                    .map_err(|error| io_error(parent, error))?
                    .join(name)
            }
        };
        let suffix = absolute
            .strip_prefix(&self.local_root)
            .map_err(|_| Error::client(0, libc::EXDEV as u32))?;
        Ok(Path::new("/").join(suffix))
    }
    /// Restore the absolute local spelling of a backend path. Does not stat it.
    pub fn local_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::client(0, libc::EINVAL as u32));
        }
        Ok(self.local_root.join(path.strip_prefix("/").unwrap_or(path)))
    }
}

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
    mount: crate::NfsMount,
    fs: crate::NfsClient,
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
        configure: impl FnOnce(crate::NfsBuilder) -> crate::NfsBuilder,
    ) -> Result<Self> {
        Self::from_discovered_with(crate::Nfs::discover_mount(directory)?, configure)
    }

    /// Connect using mount information already returned by
    /// [`crate::Nfs::discover_mount`]. This avoids repeating system mount-table
    /// discovery when an application first classifies or groups an operand.
    pub fn from_discovered(mount: crate::NfsMount) -> Result<Self> {
        Self::from_discovered_with(mount, |builder| builder)
    }

    /// Connect from existing mount information after customizing the builder.
    ///
    /// The callback must retain the supplied builder's mount binding, as in
    /// [`Self::from_mount_with`]. Replacement builders are rejected before any
    /// connection is attempted.
    pub fn from_discovered_with(
        mount: crate::NfsMount,
        configure: impl FnOnce(crate::NfsBuilder) -> crate::NfsBuilder,
    ) -> Result<Self> {
        let fs = mount
            .builder()?
            .configure_for_mount(mount.local_path(), configure)?
            .connect()?;
        // Discovery already canonicalized and validated this path, so avoid a
        // second host filesystem lookup while creating the mapping.
        let paths = PathMapper {
            local_root: mount.local_path().to_path_buf(),
        };
        Ok(Self { mount, fs, paths })
    }

    /// The validated direct NFS client.
    pub fn fs(&self) -> &crate::NfsClient {
        &self.fs
    }

    /// Information about the discovered kernel mount.
    pub fn mount(&self) -> &crate::NfsMount {
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
