//! Backend-independent mapping from local paths into a rooted client namespace.

use crate::{Error, Result};
use std::collections::hash_map::Entry;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use vfsi_core::IndexedError;

fn io_error(path: &Path, error: std::io::Error) -> Error {
    vfsi_core::internal::errors::with_context(error, "path mapper", path)
}

/// Resolution policy for the final component of an application operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvePath {
    /// Resolve an existing operand, including its final symlink.
    Follow,
    /// Resolve only its parent. Suitable for links, removal and new destinations.
    NoFollow,
}

fn validate_terminal(path: &Path) -> Result<()> {
    let spelling = path.as_os_str().as_encoded_bytes();
    if spelling.ends_with(b"/.")
        || spelling.ends_with(b"/..")
        || spelling.ends_with(b"/")
        || !matches!(
            path.components().next_back(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    Ok(())
}

fn terminal(path: &Path) -> Result<(&Path, &OsStr)> {
    validate_terminal(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok((parent, name))
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
            return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
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
        match policy {
            ResolvePath::Follow => {
                let absolute =
                    std::fs::canonicalize(path).map_err(|error| io_error(path, error))?;
                Ok(Path::new("/").join(self.suffix(&absolute)?))
            }
            ResolvePath::NoFollow => {
                let (parent, name) = terminal(path)?;
                let resolved =
                    std::fs::canonicalize(parent).map_err(|error| io_error(parent, error))?;
                self.map_terminal(&resolved, name)
            }
        }
    }

    fn suffix<'p>(&self, absolute: &'p Path) -> Result<&'p Path> {
        absolute
            .strip_prefix(&self.local_root)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EXDEV))
    }

    fn map_terminal(&self, resolved: &Path, name: &OsStr) -> Result<PathBuf> {
        // A no-follow operand naming the root itself has a parent outside it.
        if self.local_root.file_name() == Some(name) && self.local_root.parent() == Some(resolved) {
            return Ok(PathBuf::from("/"));
        }
        let suffix = self.suffix(resolved)?;
        let mut result = PathBuf::with_capacity(suffix.as_os_str().len() + name.len() + 2);
        result.push("/");
        result.push(suffix);
        result.push(name);
        Ok(result)
    }
    /// Map a cohort, resolving each distinct parent once for no-follow operands.
    /// Follow mode retains ordinary per-operand canonicalization. This is a
    /// bounded caller-owned cohort, not a persistent path or metadata cache.
    pub fn map_many<P: AsRef<Path>>(
        &self,
        paths: &[P],
        policy: ResolvePath,
    ) -> std::result::Result<Vec<PathBuf>, IndexedError> {
        let mut parents = std::collections::HashMap::new();
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let path = path.as_ref();
                let result = (|| {
                    if policy == ResolvePath::Follow {
                        return self.map(path, policy);
                    }
                    let (parent, name) = terminal(path)?;
                    let resolved = match parents.entry(parent) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => entry.insert(
                            std::fs::canonicalize(parent).map_err(|e| io_error(parent, e))?,
                        ),
                    };
                    self.map_terminal(resolved, name)
                })();
                result.map_err(|error: Error| IndexedError::at(index, error))
            })
            .collect()
    }

    /// Restore the absolute local spelling of a backend path. Does not stat it.
    pub fn local_path(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
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
            crate::test_native_code(&(PathMapper::new(&root).unwrap_err())),
            libc::ENOENT as u32
        );
    }
}
