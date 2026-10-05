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

/// An owned filesystem and its corresponding local namespace root.
///
/// Reuse a session across operands rather than reconnecting for each call.
/// The filesystem must be rooted at `local_root`. Mapping is a namespace
/// convention, not race-free confinement or kernel/direct-client coherence.
#[derive(Debug)]
pub struct MountSession<F> {
    fs: F,
    local_root: PathBuf,
}
impl<F> MountSession<F> {
    pub fn new(fs: F, local_root: impl AsRef<Path>) -> Result<Self> {
        let requested = local_root.as_ref();
        let local_root =
            std::fs::canonicalize(requested).map_err(|error| io_error(requested, error))?;
        if !local_root.is_dir() {
            return Err(Error::client(0, libc::ENOTDIR as u32));
        }
        Ok(Self { fs, local_root })
    }
    pub fn fs(&self) -> &F {
        &self.fs
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

#[cfg(all(feature = "nfs", target_os = "linux"))]
impl MountSession<crate::NfsClient> {
    /// Infer the supported mount configuration and open one direct connection
    /// rooted at `directory`. Security/version/port validation is delegated to
    /// the existing discovery API; unsupported mounts are errors, not downgrades.
    pub fn from_mount(directory: impl AsRef<Path>) -> Result<Self> {
        let fs = crate::Nfs::from_mount(directory.as_ref())?;
        Self::new(fs, directory)
    }
}
