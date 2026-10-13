//! Backend-independent mapping from local paths into a rooted client namespace.

use crate::{Error, Result};
use std::path::{Path, PathBuf};

fn io_error(path: &Path, error: std::io::Error) -> Error {
    Error::client(0, error.raw_os_error().unwrap_or(libc::EIO) as u32)
        .with_context("path mapper", path)
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
/// makes no claim that a client is rooted here. This maps namespace spellings;
/// it is not race-free confinement or kernel/direct-client coherence.
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
        Ok(Self::from_validated_root(local_root))
    }
    /// Use a canonical directory already validated by mount discovery or `new`.
    /// This deliberately performs no further host filesystem lookup.
    pub(crate) fn from_validated_root(local_root: PathBuf) -> Self {
        Self { local_root }
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

#[cfg(test)]
mod tests {
    use super::PathMapper;

    #[test]
    fn validated_root_constructor_performs_no_filesystem_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        directory.close().unwrap();

        let mapper = PathMapper::from_validated_root(root.clone());
        assert_eq!(mapper.local_root(), root);
        assert_eq!(mapper.local_path("child").unwrap(), root.join("child"));
        assert_eq!(
            PathMapper::new(&root).unwrap_err().err_no(),
            libc::ENOENT as u32
        );
    }
}
