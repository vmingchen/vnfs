use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::{Error, OpenFlags, OpenRequest, Result, WriteOp};

/// A successfully created tree. Dropping this value does not delete files.
/// Use the owning client's `remove_dir_all(tree.root())` for explicit cleanup.
#[derive(Debug)]
pub struct Tree {
    root: PathBuf,
}

impl Tree {
    /// Root in the client's namespace, not necessarily a local host path.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Debug)]
struct Entry {
    path: PathBuf,
    data: Option<Vec<u8>>,
    declaration_index: usize,
}

/// Lazily create a fresh directory tree with vectorized directory and file I/O.
///
/// Relative entry paths must not contain `..`, a root/prefix, or NUL. Missing
/// parents are inferred; sibling directories are created together, level by
/// level. Files use vector OPEN (exclusive creation), WRITE-all, and CLOSE.
/// Defaults are 64 entries per batch, 10,000 planned entries including inferred
/// parents, and 16 MiB for contents plus planned path storage. Depth is capped
/// at 128 components. These budgets are not a process-wide peak memory cap.
///
/// Configure limits before adding entries. Builder errors are saved and
/// returned by `create` before any I/O. The root's parent must already exist;
/// the root itself must not exist. No overwrite, implicit deletion, durability
/// guarantee, or rollback is provided. On I/O failure a partial tree can remain
/// at the supplied root. Use trusted parents: concurrent namespace changes and
/// symlinks in the root's ancestors are not sandboxed by this helper.
///
/// ```no_run
/// use vnfs::{Fs, FsExt, helpers::TreeBuilder};
/// # fn example(client: &impl Fs) -> vnfs::Result<()> {
/// let tree = TreeBuilder::new()
///     .add_file("config/app.conf", "host = localhost")
///     .add_empty_file("logs/app.log")
///     .add_directory("data/raw")
///     .create(client, "/new-workspace")?;
/// // Explicit cleanup when appropriate:
/// client.remove_dir_all_one(tree.root())?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct TreeBuilder {
    entries: Vec<Entry>,
    directories: HashSet<PathBuf>,
    declarations: usize,
    error: Option<Error>,
    max_entries: usize,
    max_total_bytes: usize,
    batch_size: usize,
    stored_bytes: usize,
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            directories: HashSet::new(),
            declarations: 0,
            error: None,
            max_entries: 10_000,
            max_total_bytes: 16 * 1024 * 1024,
            batch_size: 64,
            stored_bytes: 0,
        }
    }
}

impl TreeBuilder {
    /// Start an empty plan; no filesystem I/O occurs until `create`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bound planned files and directories, including inferred parents.
    pub fn max_entries(mut self, limit: usize) -> Self {
        self.max_entries = limit;
        self
    }

    /// Bound copied contents and planned path bytes. Set before adding entries.
    pub fn max_total_bytes(mut self, limit: usize) -> Self {
        self.max_total_bytes = limit;
        self
    }

    /// Maximum files or sibling directories per vector call. Must be nonzero.
    pub fn batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Add a binary or text file. Contents are copied within the builder budget.
    pub fn add_file(self, path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> Self {
        self.add(path.as_ref(), Some(data.as_ref()))
    }

    /// Add an empty file; creation is still exclusive.
    pub fn add_empty_file(self, path: impl AsRef<Path>) -> Self {
        self.add_file(path, [])
    }

    /// Add a directory and infer its missing ancestors. Repeated directory
    /// declarations are harmless; duplicate files or file/directory conflicts
    /// are rejected before creating the root.
    pub fn add_directory(self, path: impl AsRef<Path>) -> Self {
        self.add(path.as_ref(), None)
    }

    fn add(mut self, path: &Path, data: Option<&[u8]>) -> Self {
        if self.error.is_some() {
            return self;
        }
        let index = self.declarations;
        let Some(next) = self.declarations.checked_add(1) else {
            self.error = Some(Error::client(index, libc::EOVERFLOW as u32));
            return self;
        };
        self.declarations = next;
        let result = (|| {
            // Do not copy an oversized caller-owned path into error context.
            if path.as_os_str().len() > self.max_total_bytes {
                return Err(Error::client(index, libc::EFBIG as u32));
            }
            let normalized = relative_path(path).map_err(|()| error(index, libc::EINVAL, path))?;
            // Repeated directory spellings are one planned object. Suppress
            // them before charging either entry slots or stored path bytes.
            if data.is_none() && self.directories.contains(&normalized) {
                return Ok(());
            }
            if self.entries.len() >= self.max_entries {
                return Err(error(index, libc::EFBIG, path));
            }
            let bytes = self
                .stored_bytes
                .checked_add(normalized.as_os_str().len())
                .and_then(|n| n.checked_add(data.map_or(0, <[u8]>::len)))
                .filter(|n| *n <= self.max_total_bytes)
                .ok_or_else(|| error(index, libc::EFBIG, path))?;
            if data.is_none() {
                self.directories.insert(normalized.clone());
            }
            self.entries.push(Entry {
                path: normalized,
                data: data.map(<[u8]>::to_vec),
                declaration_index: index,
            });
            self.stored_bytes = bytes;
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(error);
        }
        self
    }

    /// Materialize under a new root through a high-level vNFS client.
    /// All local validation precedes I/O. I/O errors preserve their status and
    /// path; entry indices refer to the original declarations (inferred parents
    /// use the first declaration requiring them), not to vector batch positions.
    /// Root-creation errors identify the root, not an entry declaration.
    pub fn create<C: Fs>(self, client: &C, root: impl AsRef<Path>) -> Result<Tree> {
        let root = root.as_ref();
        if let Some(error) = self.error {
            return Err(error);
        }
        if root.as_os_str().len() > self.max_total_bytes {
            return Err(Error::client(0, libc::EFBIG as u32));
        }
        if self.batch_size == 0
            || root.as_os_str().is_empty()
            || root.as_os_str().as_encoded_bytes().contains(&0)
            || root
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(error(0, libc::EINVAL, root));
        }
        // Map each planned path to its declaring entry. Infer every ancestor
        // once, checking collisions independently of declaration order.
        let mut planned: BTreeMap<PathBuf, (bool, usize)> = BTreeMap::new();
        let mut bytes = self.entries.iter().try_fold(0usize, |bytes, entry| {
            bytes
                .checked_add(entry.data.as_ref().map_or(0, Vec::len))
                .ok_or_else(|| error(0, libc::EFBIG, root))
        })?;
        for (position, entry) in self.entries.iter().enumerate() {
            let index = entry.declaration_index;
            let mut paths: Vec<_> = entry
                .path
                .ancestors()
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            paths.reverse();
            for path in paths {
                let file = path == entry.path && entry.data.is_some();
                if let Some(&(existing_file, _)) = planned.get(path) {
                    if existing_file || file {
                        return Err(error(index, libc::EINVAL, path));
                    }
                    continue;
                }
                if planned.len() >= self.max_entries {
                    return Err(error(index, libc::EFBIG, path));
                }
                bytes = bytes
                    .checked_add(root.join(path).as_os_str().len())
                    .filter(|n| *n <= self.max_total_bytes)
                    .ok_or_else(|| error(index, libc::EFBIG, path))?;
                planned.insert(path.to_path_buf(), (file, position));
            }
        }
        let mut directories = BTreeMap::<usize, Vec<(PathBuf, usize)>>::new();
        let mut files = Vec::new();
        for (path, (file, index)) in planned {
            if file {
                files.push((root.join(path), index));
            } else {
                directories
                    .entry(path.components().count())
                    .or_default()
                    .push((root.join(path), index));
            }
        }
        client
            .create_dir_one(root)
            .map_err(|e| e.with_context("create_tree", root))?;
        for level in directories.values() {
            for batch in level.chunks(self.batch_size) {
                let paths: Vec<_> = batch.iter().map(|(path, _)| path).collect();
                client
                    .mkdirv(&paths)
                    .map_err(|e| entry_error(e, batch, &self.entries))?;
            }
        }
        for batch in files.chunks(self.batch_size) {
            let requests: Vec<_> = batch
                .iter()
                .map(|(path, _)| OpenRequest::new(path, OpenFlags::WRITE | OpenFlags::CREATE_NEW))
                .collect();
            let handles = client
                .openv(&requests)
                .map_err(|e| entry_error(e, batch, &self.entries))?;
            if handles.len() != batch.len() {
                let _ = client.closev(handles);
                return Err(
                    Error::transport(None, "create_tree: invalid OPEN result count")
                        .with_context("create_tree", root),
                );
            }
            let writes: Vec<_> = handles
                .iter()
                .zip(batch)
                .map(|(file, (_, index))| {
                    WriteOp::at(file, 0, self.entries[*index].data.as_deref().unwrap())
                })
                .collect();
            let write = client
                .writev_with_options(&writes, crate::WriteOptions::new().write_all(true))
                .map(|_| ())
                .map_err(|e| entry_error(e, batch, &self.entries));
            drop(writes);
            let close = client
                .closev(handles)
                .map_err(|e| entry_error(e, batch, &self.entries));
            write?;
            close?;
        }
        Ok(Tree {
            root: root.to_path_buf(),
        })
    }
}

fn relative_path(path: &Path) -> std::result::Result<PathBuf, ()> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(());
    }
    let mut normalized = PathBuf::new();
    let mut depth = 0;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                depth += 1;
                if depth > 128 {
                    return Err(());
                }
                normalized.push(name);
            }
            Component::CurDir => {}
            _ => return Err(()),
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(());
    }
    Ok(normalized)
}

fn error(index: usize, errno: i32, path: &Path) -> Error {
    Error::client(index, errno as u32).with_context("create_tree", path)
}

fn entry_error(error: Error, batch: &[(PathBuf, usize)], entries: &[Entry]) -> Error {
    match error.index().and_then(|index| batch.get(index)) {
        Some((path, position)) => error
            .with_index(entries[*position].declaration_index)
            .with_context("create_tree", path),
        None => error,
    }
}

#[cfg(all(test, feature = "test-faults", feature = "auto", target_os = "linux"))]
mod tests {
    use super::*;
    use crate::backend::{DummyVecFs, FsClient};
    use std::sync::Arc;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};

    #[test]
    fn failed_open_preserves_original_index_and_partial_tree_without_replay_or_leaks() {
        let root = tempfile::tempdir().unwrap();
        let mut backend = DummyVecFs::try_new(root.path().to_path_buf()).unwrap();
        let script = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeRegister { index: 1 },
            Error::transport_with_kind(777, crate::TransportKind::Timeout, "injected OPEN failure"),
        ));
        backend.set_fault_injector(script.clone());
        let client = crate::Mounted {
            inner: FsClient::new(backend),
        };
        // Planning sorts names: a is first and b is second, but b was declared
        // at index two, including the suppressed duplicate directory.
        let error = TreeBuilder::new()
            .batch_size(2)
            .add_directory("unused")
            .add_directory("./unused")
            .add_file("b", "b")
            .add_file("c", "c")
            .add_file("a", "a")
            .create(&client, "/fixture")
            .unwrap_err();
        assert_eq!(error.index(), Some(2));
        assert_eq!(error.path(), Some(Path::new("/fixture/b")));
        assert_eq!(error.transport_kind(), Some(crate::TransportKind::Timeout));
        assert!(script.is_consumed());
        assert_eq!(
            script
                .visited()
                .iter()
                .filter(|point| matches!(point, OpenFaultPoint::BeforeDispatch { .. }))
                .count(),
            1
        );
        assert_eq!(std::fs::read(root.path().join("fixture/a")).unwrap(), b"");
        assert!(!root.path().join("fixture/b").exists());
        assert!(!root.path().join("fixture/c").exists());
        let backend = client.inner.into_inner().unwrap();
        assert_eq!(backend.test_open_handle_count(), 0);
    }

    #[test]
    fn failed_directory_batch_maps_inferred_parent_to_original_declaration() {
        let root = tempfile::tempdir().unwrap();
        let mut backend = DummyVecFs::try_new(root.path().to_path_buf()).unwrap();
        let script = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeSetPermissions { index: 1 },
            Error::client(1, libc::EACCES as u32),
        ));
        backend.set_fault_injector(script.clone());
        let client = crate::Mounted {
            inner: FsClient::new(backend),
        };
        let error = TreeBuilder::new()
            .add_directory("unused")
            .add_directory("./unused")
            .add_file("b/file", "b")
            .add_file("a/file", "a")
            .create(&client, "/fixture")
            .unwrap_err();
        assert_eq!(error.index(), Some(2));
        assert_eq!(error.path(), Some(Path::new("/fixture/b")));
        assert_eq!(error.kind(), crate::ErrorKind::PermissionDenied);
        assert!(script.is_consumed());
        assert!(root.path().join("fixture/a").is_dir());
        assert!(root.path().join("fixture/b").is_dir());
        assert!(!root.path().join("fixture/a/file").exists());
        assert!(!root.path().join("fixture/b/file").exists());
    }
}
use crate::{Fs, FsExt};
