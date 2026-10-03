//! Linux mount-aware client. Only unambiguous NFSv4 AUTH_SYS mounts are
//! promoted to a separate direct NFS connection; everything else stays on
//! the kernel-mounted path.

#[cfg(test)]
use crate::FsExt as _;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{
    DirEntry, Error as VfError, Mounted, Nfs, OpenFlags, OpenRequest,
    OwnedReadResult as ReadResult, ReadDirOptions, ReadIntoResult, ResourceLimits,
    Result as VfResult, WriteResult,
};
use vfsi_local::DummyVecFs;
use vfsi_sync::{FsClient, FsFile};
type NfsClient = FsClient<vfsi_nfs::NfsVecFs>;
type NfsFile = FsFile<vfsi_nfs::NfsVecFs>;
#[cfg(test)]
use vfsi_sync::DEFAULT_READ_ALLV_MAX_TOTAL_BYTES;

macro_rules! routed_path_method {
    ($name:ident, $result:ty) => {
        pub fn $name(&self, path: impl AsRef<Path>) -> VfResult<$result> {
            let route = self.resolve(path.as_ref(), &read_mounts(false));
            match route.route {
                Route::Mounted => self.mounted.$name(&route.path),
                Route::Nfs(connection) => connection.client.$name(&route.path),
            }
        }
    };
}

const READ_CHUNK: usize = 1024 * 1024;

/// Opt into mount-aware direct NFS acceleration. Unlike [`Mounted`], eligible
/// NFSv4 AUTH_SYS mounts use a separate client and vectorized COMPOUNDs.
///
/// # Consistency
/// Direct operations do not share or invalidate the kernel client's caches.
/// Mixing them with kernel I/O on the same objects (including path aliases)
/// can expose stale data or delayed writes. This is not a transparent caching
/// acceleration. Use `Mounted` when kernel coherency semantics are required.
#[derive(Debug)]
pub struct Auto(AutoClient);

impl Auto {
    /// Root all application paths at this existing host directory. For example,
    /// root `/work` plus application `/a` addresses host `/work/a`.
    /// The root is a namespace convention, not a security sandbox.
    pub fn new(root: impl AsRef<Path>) -> VfResult<Self> {
        let root = fs::canonicalize(root.as_ref())
            .map_err(|e| VfError::client(0, e.raw_os_error().unwrap_or(libc::EIO) as u32))?;
        if !root.is_dir() {
            return Err(VfError::client(0, libc::ENOTDIR as u32));
        }
        Ok(Self(AutoClient {
            mounted: Mounted::new(&root)?.inner,
            root,
            connections: Mutex::new(HashMap::new()),
            owner: Arc::new(()),
            limits: ResourceLimits::default(),
        }))
    }

    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.0.limits = limits;
        self.0.mounted = self.0.mounted.with_limits(limits);
        // Keep the connection/route identities: already-open handles still
        // belong to these clients. Only replace their per-clone policies.
        for connection in self
            .0
            .connections
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .values_mut()
        {
            connection.client = connection.client.clone().with_limits(limits);
        }
        self
    }
}

impl std::ops::Deref for Auto {
    type Target = AutoClient;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The backend actually selected for a path. A direct route means a fresh
/// connection was successfully established and its root identity verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoRoute {
    Mounted,
    DirectNfs {
        mount_point: PathBuf,
        server: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountSpec {
    id: u64,
    mount_point: PathBuf,
    export: PathBuf,
    server: String,
    minor: u32,
}

#[derive(Default)]
struct MountTable {
    eligible: Vec<MountSpec>,
    mount_points: HashSet<PathBuf>,
}

#[derive(Clone)]
struct NfsConnection {
    spec: MountSpec,
    client: NfsClient,
    identity: Arc<()>,
    credentials: AuthSysIdentity,
}

use vfsi_nfs::mount::{AuthSysIdentity, decode_mount_field, path_mount_id};

#[derive(Clone)]
enum Route {
    Mounted,
    Nfs(NfsConnection),
}

impl Route {
    fn same_backend(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Mounted, Self::Mounted) => true,
            (Self::Nfs(a), Self::Nfs(b)) => Arc::ptr_eq(&a.identity, &b.identity),
            _ => false,
        }
    }

    fn public(&self) -> AutoRoute {
        match self {
            Self::Mounted => AutoRoute::Mounted,
            Self::Nfs(connection) => AutoRoute::DirectNfs {
                mount_point: connection.spec.mount_point.clone(),
                server: connection.spec.server.clone(),
            },
        }
    }
}

struct Resolved {
    route: Route,
    path: PathBuf,
}

/// Account for paths in the public namespace before allocating/handing off
/// remapped entries. Backend-relative accounting alone omits mount prefixes.
struct PathByteBudget {
    used: usize,
    limit: usize,
}

impl PathByteBudget {
    fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }
    fn charge(&mut self, path: &Path, operation: &'static str) -> VfResult<()> {
        self.used = self
            .used
            .checked_add(path.as_os_str().len())
            .filter(|bytes| *bytes <= self.limit)
            .ok_or_else(|| VfError::client(0, libc::EFBIG as u32).with_context(operation, path))?;
        Ok(())
    }
}

/// Mount-aware client with lazily established per-mount NFS connections.
pub struct AutoClient {
    root: PathBuf,
    mounted: FsClient<DummyVecFs>,
    connections: Mutex<HashMap<u64, NfsConnection>>,
    owner: Arc<()>,
    limits: ResourceLimits,
}

impl std::fmt::Debug for AutoClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AutoClient")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl AutoClient {
    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }
    pub fn open_options(&self) -> AutoOpenOptions<'_> {
        AutoOpenOptions {
            client: self,
            flags: OpenFlags::empty(),
            mode: 0o666,
        }
    }
    pub fn set_metadata(&self, path: impl AsRef<Path>) -> AutoSetMetadata<'_> {
        AutoSetMetadata {
            client: self,
            path: path.as_ref().to_path_buf(),
            update: vfsi_core::MetadataUpdate::new(),
            follow: true,
        }
    }

    pub fn open_dir_handle(&self, path: impl AsRef<Path>) -> VfResult<AutoDir> {
        let route = self.resolve_tree(path.as_ref());
        let inner = match &route.route {
            Route::Mounted => AutoDirInner::Mounted(self.mounted.open_dir_handle(&route.path)?),
            Route::Nfs(connection) => {
                AutoDirInner::Nfs(connection.client.open_dir_handle(&route.path)?)
            }
        };
        Ok(AutoDir {
            path: path.as_ref().to_path_buf(),
            route: route.route,
            inner,
        })
    }
    routed_path_method!(create_dir_all, ());
    routed_path_method!(remove_dir, ());
    routed_path_method!(read_link, PathBuf);
    pub fn ensure_empty_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let route = self.resolve_tree(path.as_ref());
        match route.route {
            Route::Mounted => self.mounted.ensure_empty_dir(&route.path),
            Route::Nfs(connection) => connection.client.ensure_empty_dir(&route.path),
        }
    }

    pub fn create_dir_with_mode(&self, path: impl AsRef<Path>, mode: u32) -> VfResult<()> {
        let route = self.resolve(path.as_ref(), &read_mounts(false));
        match route.route {
            Route::Mounted => self.mounted.create_dir_with_mode(&route.path, mode),
            Route::Nfs(connection) => connection.client.create_dir_with_mode(&route.path, mode),
        }
    }

    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> VfResult<()> {
        // A symlink target is interpreted by subsequent kernel pathname
        // resolution; do not rewrite its text into an export-relative name.
        self.mounted.symlink(target, link)
    }

    pub fn hard_link(&self, source: impl AsRef<Path>, link: impl AsRef<Path>) -> VfResult<()> {
        let mounts = read_mounts(false);
        let source_route = self.resolve(source.as_ref(), &mounts);
        let link_route = self.resolve(link.as_ref(), &mounts);
        match &source_route.route {
            Route::Nfs(connection) if source_route.route.same_backend(&link_route.route) => {
                connection
                    .client
                    .hard_link(&source_route.path, &link_route.path)
            }
            _ => self.mounted.hard_link(source, link),
        }
    }

    pub fn metadatav_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::MetadataOptions,
    ) -> VfResult<Vec<crate::Metadata>> {
        let mounts = read_mounts(false);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| self.resolve(path.as_ref(), &mounts))
            .collect();
        let mut output = Vec::with_capacity(paths.len());
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(&resolved, start);
            let batch: Vec<_> = resolved[start..end]
                .iter()
                .map(|route| route.path.as_path())
                .collect();
            output.extend(
                match &resolved[start].route {
                    Route::Mounted => self.mounted.metadata_many(
                        &batch,
                        options.requested_fields(),
                        options.follows_symlinks(),
                    ),
                    Route::Nfs(connection) => connection.client.metadata_many(
                        &batch,
                        options.requested_fields(),
                        options.follows_symlinks(),
                    ),
                }
                .map_err(|error| indexed(error, start))?,
            );
            start = end;
        }
        Ok(output)
    }

    /// Create directories in bounded backend cohorts, retaining input order.
    /// Parents must exist; this does not promise transactional rollback.
    pub fn create_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut seen = std::collections::HashSet::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            if !seen.insert(path.as_ref()) {
                return Err(VfError::client(index, libc::EINVAL as u32)
                    .with_context("create_dirs", path.as_ref()));
            }
        }
        let mounts = read_mounts(true);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| self.resolve(path.as_ref(), &mounts))
            .collect();
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(&resolved, start);
            let batch: Vec<_> = resolved[start..end]
                .iter()
                .map(|route| route.path.as_path())
                .collect();
            let result = match &resolved[start].route {
                Route::Mounted => self.mounted.create_dirs(&batch),
                Route::Nfs(connection) => connection.client.create_dirs(&batch),
            };
            result.map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| paths.get(index)) {
                    Some(path) => error.with_context("create_dirs", path.as_ref()),
                    None => error,
                }
            })?;
            start = end;
        }
        Ok(())
    }

    pub fn remove_paths<P: AsRef<Path>>(&self, paths: &[P], recursive: bool) -> VfResult<()> {
        self.remove_paths_with_options(paths, recursive, crate::RemoveOptions::default())
    }

    pub fn remove_paths_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
        options: crate::RemoveOptions,
    ) -> VfResult<()> {
        let mounts = read_mounts(true);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| {
                let path = path.as_ref();
                if recursive
                    && self.host_path(path).is_some_and(|host| {
                        mounts
                            .mount_points
                            .iter()
                            .any(|point| point != &host && point.starts_with(&host))
                    })
                {
                    Resolved {
                        route: Route::Mounted,
                        path: path.to_path_buf(),
                    }
                } else {
                    self.resolve(path, &mounts)
                }
            })
            .collect();
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(&resolved, start);
            let batch: Vec<_> = resolved[start..end]
                .iter()
                .map(|route| route.path.as_path())
                .collect();
            match &resolved[start].route {
                Route::Mounted => self
                    .mounted
                    .remove_paths_with_options(&batch, recursive, options),
                Route::Nfs(connection) => connection
                    .client
                    .remove_paths_with_options(&batch, recursive, options),
            }
            .map_err(|error| indexed(error, start))?;
            start = end;
        }
        Ok(())
    }

    pub fn copy_files<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> VfResult<()> {
        let mounts = read_mounts(false);
        let pairs: Vec<_> = pairs
            .iter()
            .map(|(source, destination)| {
                let a = self.resolve(source.as_ref(), &mounts);
                let b = self.resolve(destination.as_ref(), &mounts);
                if a.route.same_backend(&b.route) {
                    (a, b.path)
                } else {
                    (
                        Resolved {
                            route: Route::Mounted,
                            path: source.as_ref().to_path_buf(),
                        },
                        destination.as_ref().to_path_buf(),
                    )
                }
            })
            .collect();
        let mut start = 0;
        while start < pairs.len() {
            let mut end = start + 1;
            while end < pairs.len() && pairs[start].0.route.same_backend(&pairs[end].0.route) {
                end += 1;
            }
            let batch: Vec<_> = pairs[start..end]
                .iter()
                .map(|(source, destination)| (source.path.as_path(), destination.as_path()))
                .collect();
            match &pairs[start].0.route {
                Route::Mounted => self.mounted.copy_files(&batch),
                Route::Nfs(connection) => connection.client.copy_files(&batch),
            }
            .map_err(|error| indexed(error, start))?;
            start = end;
        }
        Ok(())
    }

    pub fn read_dirs_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        fields: crate::MetadataFields,
        options: ReadDirOptions,
    ) -> VfResult<Vec<crate::DirectoryListing>> {
        let mounts = read_mounts(false);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| self.resolve(path.as_ref(), &mounts))
            .collect();
        let mut output = Vec::with_capacity(paths.len());
        let mut entries = 0usize;
        let mut bytes = 0usize;
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(&resolved, start);
            let batch: Vec<_> = resolved[start..end]
                .iter()
                .map(|route| route.path.as_path())
                .collect();
            let remaining = ReadDirOptions::new()
                .max_entries(options.entry_limit().saturating_sub(entries))
                .max_path_bytes(options.path_byte_limit().saturating_sub(bytes));
            let mut listings = match &resolved[start].route {
                Route::Mounted => self
                    .mounted
                    .read_dirs_with_options(&batch, fields, remaining),
                Route::Nfs(connection) => connection
                    .client
                    .read_dirs_with_options(&batch, fields, remaining),
            }
            .map_err(|error| indexed(error, start))?;
            for (relative_index, listing) in listings.iter_mut().enumerate() {
                listing.path = paths[start + relative_index].as_ref().to_path_buf();
                for entry in &mut listing.entries {
                    if let Route::Nfs(connection) = &resolved[start].route {
                        *entry = DirEntry::new(
                            self.public_path(connection, entry.path())?,
                            entry.metadata().clone(),
                        );
                    }
                    entries = entries.checked_add(1).ok_or_else(|| {
                        VfError::client(start + relative_index, libc::EFBIG as u32)
                    })?;
                    bytes = bytes
                        .checked_add(entry.path().as_os_str().len())
                        .ok_or_else(|| {
                            VfError::client(start + relative_index, libc::EFBIG as u32)
                        })?;
                    if entries > options.entry_limit() || bytes > options.path_byte_limit() {
                        return Err(VfError::client(start + relative_index, libc::EFBIG as u32));
                    }
                }
            }
            output.extend(listings);
            start = end;
        }
        Ok(output)
    }

    pub fn capabilities_for(&self, path: impl AsRef<Path>) -> VfResult<crate::Capabilities> {
        let route = self.resolve(path.as_ref(), &read_mounts(false));
        match route.route {
            Route::Mounted => self.mounted.capabilities(),
            Route::Nfs(connection) => connection.client.capabilities(),
        }
    }

    pub(crate) fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        let mounts = read_mounts(false);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| self.resolve(path.as_ref(), &mounts))
            .collect();
        let mut output = Vec::with_capacity(paths.len());
        let mut remaining = options.total_byte_limit();
        let mut start = 0;
        while start < paths.len() {
            let mut end = start + 1;
            while end < paths.len() && resolved[start].route.same_backend(&resolved[end].route) {
                end += 1;
            }
            let batch: Vec<_> = resolved[start..end]
                .iter()
                .map(|item| item.path.as_path())
                .collect();
            let options = crate::ReadAllOptions::new().max_total_bytes(remaining);
            let buffers = match &resolved[start].route {
                Route::Mounted => self.mounted.read_files_with_options(&batch, options),
                Route::Nfs(connection) => {
                    connection.client.read_files_with_options(&batch, options)
                }
            }
            .map_err(|error| indexed(error, start))?;
            for buffer in buffers {
                remaining = remaining
                    .checked_sub(buffer.len())
                    .ok_or_else(|| VfError::client(start, libc::EFBIG as u32))?;
                output.push(buffer);
            }
            start = end;
        }
        Ok(output)
    }

    pub fn read_stream_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::ReadStreamOptions,
        callback: impl FnMut(u64, &[u8]) -> VfResult<bool>,
    ) -> VfResult<crate::StreamCompletion> {
        let route = self.resolve(path.as_ref(), &read_mounts(false));
        match route.route {
            Route::Mounted => self
                .mounted
                .read_stream_with_options(&route.path, options, callback),
            Route::Nfs(connection) => {
                connection
                    .client
                    .read_stream_with_options(&route.path, options, callback)
            }
        }
    }

    fn public_path(&self, connection: &NfsConnection, path: &Path) -> VfResult<PathBuf> {
        let suffix = path
            .strip_prefix("/")
            .map_err(|_| VfError::client(0, libc::EIO as u32))?;
        let host = connection.spec.mount_point.join(suffix);
        let relative = host
            .strip_prefix(&self.root)
            .map_err(|_| VfError::client(0, libc::EIO as u32))?;
        Ok(Path::new("/").join(relative))
    }

    pub fn visit_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: ReadDirOptions,
        mut callback: impl FnMut(DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<crate::TraversalCompletion> {
        let route = self.resolve(path.as_ref(), &read_mounts(false));
        match route.route {
            Route::Mounted => self
                .mounted
                .visit_dir_with_options(&route.path, options, callback),
            Route::Nfs(connection) => {
                let mut budget = PathByteBudget::new(options.path_byte_limit());
                connection
                    .client
                    .visit_dir_with_options(&route.path, options, |entry| {
                        let public = self.public_path(&connection, entry.path())?;
                        budget.charge(&public, "visit_dir")?;
                        callback(DirEntry::new(public, entry.metadata().clone()))
                    })
            }
        }
    }

    pub fn visit_walk_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::WalkOptions,
        mut callback: impl FnMut(&DirEntry) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<crate::TraversalCompletion> {
        let route = self.resolve_tree(path.as_ref());
        match route.route {
            Route::Mounted => self
                .mounted
                .visit_walk_with_options(&route.path, options, callback),
            Route::Nfs(connection) => {
                let mut budget = PathByteBudget::new(options.path_byte_limit());
                budget.charge(&self.public_path(&connection, &route.path)?, "visit_walk")?;
                connection
                    .client
                    .visit_walk_with_options(&route.path, options, |entry| {
                        let public = self.public_path(&connection, entry.path())?;
                        budget.charge(&public, "visit_walk")?;
                        callback(&DirEntry::new(public, entry.metadata().clone()))
                    })
            }
        }
    }

    pub fn walk_with_options(
        &self,
        path: impl AsRef<Path>,
        fields: crate::MetadataFields,
        options: crate::WalkOptions,
    ) -> VfResult<Vec<crate::DirectoryListing>> {
        let route = self.resolve_tree(path.as_ref());
        let mut budget = PathByteBudget::new(options.path_byte_limit());
        match route.route {
            Route::Mounted => self.mounted.walk_with_options(&route.path, fields, options),
            Route::Nfs(connection) => connection
                .client
                .walk_with_options(&route.path, fields, options)?
                .into_iter()
                .map(|listing| {
                    let public = self.public_path(&connection, &listing.path)?;
                    budget.charge(&public, "walk")?;
                    Ok(crate::DirectoryListing {
                        path: public,
                        entries: listing
                            .entries
                            .into_iter()
                            .map(|entry| {
                                let public = self.public_path(&connection, entry.path())?;
                                budget.charge(&public, "walk")?;
                                Ok(DirEntry::new(public, entry.metadata().clone()))
                            })
                            .collect::<VfResult<_>>()?,
                    })
                })
                .collect(),
        }
    }

    /// Recursive operations retain kernel routing if another mount appears
    /// underneath their root, rather than bypassing that mounted subtree.
    fn resolve_tree(&self, path: &Path) -> Resolved {
        let mounts = read_mounts(true);
        if self.host_path(path).is_some_and(|host| {
            mounts
                .mount_points
                .iter()
                .any(|point| point != &host && point.starts_with(&host))
        }) {
            return Resolved {
                route: Route::Mounted,
                path: path.to_path_buf(),
            };
        }
        self.resolve(path, &mounts)
    }

    pub fn remove_dir_all(&self, path: impl AsRef<Path>) -> VfResult<()> {
        self.remove_dir_all_with_options(path, crate::RemoveOptions::default())
    }

    pub fn remove_dir_all_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::RemoveOptions,
    ) -> VfResult<()> {
        let route = self.resolve_tree(path.as_ref());
        match route.route {
            Route::Mounted => self
                .mounted
                .remove_dir_all_with_options(&route.path, options),
            Route::Nfs(connection) => connection
                .client
                .remove_dir_all_with_options(&route.path, options),
        }
    }

    pub fn remove_dir_contents(&self, path: impl AsRef<Path>) -> VfResult<()> {
        self.remove_dir_contents_with_options(path, crate::RemoveOptions::default())
    }

    pub fn remove_dir_contents_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::RemoveOptions,
    ) -> VfResult<()> {
        let route = self.resolve_tree(path.as_ref());
        match route.route {
            Route::Mounted => self
                .mounted
                .remove_dir_contents_with_options(&route.path, options),
            Route::Nfs(connection) => connection
                .client
                .remove_dir_contents_with_options(&route.path, options),
        }
    }
    /// Inspect the currently eligible path route. An individual operation
    /// can still choose Mounted (for example, when its target is a symlink).
    /// Inspect an opened [`AutoFile`] to see the route actually used.
    pub fn route_for(&self, path: impl AsRef<Path>) -> AutoRoute {
        let mounts = read_mounts(false);
        self.resolve(path.as_ref(), &mounts).route.public()
    }

    fn host_path(&self, path: &Path) -> Option<PathBuf> {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return None;
        }
        let relative = path.strip_prefix("/").unwrap_or(path);
        Some(self.root.join(relative))
    }

    fn resolve(&self, path: &Path, mounts: &MountTable) -> Resolved {
        let mounted = || Resolved {
            route: Route::Mounted,
            path: path.to_path_buf(),
        };
        let Some(host_path) = self.host_path(path) else {
            return mounted();
        };
        // Do not bypass kernel symlink resolution or map a path outside root.
        // For creation, inspect the existing parent instead of the missing file.
        let (canonical, inspect) = match fs::canonicalize(&host_path) {
            Ok(actual) => (actual.clone(), actual),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = host_path.parent() else {
                    return mounted();
                };
                let Ok(parent) = fs::canonicalize(parent) else {
                    return mounted();
                };
                let Some(name) = host_path.file_name() else {
                    return mounted();
                };
                (parent.join(name), parent)
            }
            Err(_) => return mounted(),
        };
        if canonical != host_path || !canonical.starts_with(&self.root) {
            return mounted();
        }
        let Some(mount_id) = path_mount_id(&inspect) else {
            return mounted();
        };
        let Some(spec) = mounts
            .eligible
            .iter()
            .find(|mount| mount.id == mount_id && canonical.starts_with(&mount.mount_point))
        else {
            return mounted();
        };
        let Some(connection) = self.connection(spec) else {
            return mounted();
        };
        let Ok(suffix) = canonical.strip_prefix(&spec.mount_point) else {
            return mounted();
        };
        Resolved {
            route: Route::Nfs(connection),
            path: Path::new("/").join(suffix),
        }
    }

    fn connection(&self, spec: &MountSpec) -> Option<NfsConnection> {
        let credentials = AuthSysIdentity::current()?;
        if let Ok(cache) = self.connections.lock()
            && let Some(existing) = cache.get(&spec.id)
            && existing.spec == *spec
            && existing.credentials == credentials
        {
            return Some(existing.clone());
        }
        // Connection failure is a pre-dispatch fallback. Never replay a
        // possibly completed mutation through the mounted backend.
        let client = Nfs::builder(&spec.server)
            .root(&spec.export)
            .version(crate::NfsVersion::try_from(Some(spec.minor)).ok()?)
            .connect()
            .ok()?
            .with_limits(self.limits)
            .inner;
        let kernel_id = fs::metadata(&spec.mount_point).ok()?.ino();
        if client.metadata("/").ok()?.file_id() != Some(kernel_id) {
            return None;
        }
        if AuthSysIdentity::current().as_ref() != Some(&credentials) {
            return None;
        }
        let connection = NfsConnection {
            spec: spec.clone(),
            client,
            identity: Arc::new(()),
            credentials,
        };
        let mut cache = self.connections.lock().ok()?;
        if let Some(existing) = cache.get(&spec.id)
            && existing.spec == *spec
            && existing.credentials == connection.credentials
        {
            return Some(existing.clone());
        }
        cache.insert(spec.id, connection.clone());
        Some(connection)
    }

    fn resolve_open_batch(&self, requests: &[OpenRequest], mounts: &MountTable) -> Vec<Resolved> {
        let mut parents: HashMap<PathBuf, Option<NfsConnection>> = HashMap::new();
        let mut resolved: Vec<_> = requests
            .iter()
            .map(|request| {
                let fallback = || Resolved {
                    route: Route::Mounted,
                    path: request.path.clone(),
                };
                let Some(host) = self.host_path(&request.path) else {
                    return fallback();
                };
                // A file or directory mounted over an NFS entry belongs to
                // the covering mount, not the NFS filesystem of its parent.
                // mountinfo is already read once for this call, so this costs
                // only a hash lookup per request rather than a statx syscall.
                if mounts.mount_points.contains(&host) {
                    return fallback();
                }
                let Some(parent) = host.parent() else {
                    return fallback();
                };
                let connection = parents
                    .entry(parent.to_path_buf())
                    .or_insert_with(|| {
                        let canonical = fs::canonicalize(parent).ok()?;
                        if canonical != parent || !canonical.starts_with(&self.root) {
                            return None;
                        }
                        let id = path_mount_id(&canonical)?;
                        let spec = mounts.eligible.iter().find(|mount| {
                            mount.id == id && canonical.starts_with(&mount.mount_point)
                        })?;
                        self.connection(spec)
                    })
                    .clone();
                let Some(connection) = connection else {
                    return fallback();
                };
                // CREATE_NEW cannot follow a pre-existing final symlink. Other
                // opens are checked in the vector no-follow preflight below.
                let Ok(suffix) = host.strip_prefix(&connection.spec.mount_point) else {
                    return fallback();
                };
                Resolved {
                    route: Route::Nfs(connection),
                    path: Path::new("/").join(suffix),
                }
            })
            .collect();

        // One no-follow NFS vector preflight identifies final symlinks without
        // paying a separate mounted-path lookup for every file. An ambiguous
        // preflight leaves that cohort on the kernel path before any mutation.
        let mut start = 0;
        while start < resolved.len() {
            let mut end = start + 1;
            while end < resolved.len() && resolved[start].route.same_backend(&resolved[end].route) {
                end += 1;
            }
            if let Route::Nfs(connection) = &resolved[start].route {
                let checked: Vec<_> = (start..end)
                    .filter(|index| !requests[*index].flags.contains(OpenFlags::CREATE_NEW))
                    .collect();
                if !checked.is_empty() {
                    let paths: Vec<_> = checked
                        .iter()
                        .map(|index| resolved[*index].path.as_path())
                        .collect();
                    match connection.client.symlink_metadatav(&paths) {
                        Ok(metadata) if metadata.len() == checked.len() => {
                            for (index, item) in checked.into_iter().zip(metadata) {
                                if item.file_type() == crate::FileType::Symlink {
                                    resolved[index] = Resolved {
                                        route: Route::Mounted,
                                        path: requests[index].path.clone(),
                                    };
                                }
                            }
                        }
                        _ => {
                            for index in start..end {
                                resolved[index] = Resolved {
                                    route: Route::Mounted,
                                    path: requests[index].path.clone(),
                                };
                            }
                        }
                    }
                }
            }
            start = end;
        }
        resolved
    }

    /// Preserve request order, including the completed-prefix semantics of
    /// strict vector operations. Consecutive requests to one mount batch.
    pub fn open_with(&self, request: OpenRequest) -> VfResult<AutoFile> {
        self.openv(&[request]).map(|mut files| files.remove(0))
    }

    pub fn openv(&self, requests: &[OpenRequest]) -> VfResult<Vec<AutoFile>> {
        let mounts = read_mounts(true);
        let resolved = self.resolve_open_batch(requests, &mounts);
        let mut output = Vec::with_capacity(requests.len());
        let mut start = 0;
        while start < requests.len() {
            let mut end = start + 1;
            while end < requests.len() && resolved[start].route.same_backend(&resolved[end].route) {
                end += 1;
            }
            let batch: Vec<_> = (start..end)
                .map(|index| {
                    let mut request = requests[index].clone();
                    request.path = resolved[index].path.clone();
                    request
                })
                .collect();
            let route = resolved[start].route.clone();
            match &route {
                Route::Mounted => {
                    let files = self.mounted.openv(&batch).map_err(|e| indexed(e, start))?;
                    for (index, file) in files.into_iter().enumerate() {
                        output.push(AutoFile::new(
                            requests[start + index].path.clone(),
                            route.clone(),
                            AutoFileInner::Mounted(file),
                            &self.owner,
                        ));
                    }
                }
                Route::Nfs(connection) => {
                    let files = connection
                        .client
                        .openv(&batch)
                        .map_err(|e| indexed(e, start))?;
                    for (index, file) in files.into_iter().enumerate() {
                        output.push(AutoFile::new(
                            requests[start + index].path.clone(),
                            route.clone(),
                            AutoFileInner::Nfs(file),
                            &self.owner,
                        ));
                    }
                }
            }
            start = end;
        }
        Ok(output)
    }

    fn check_owner(&self, file: &AutoFile, index: usize) -> VfResult<()> {
        if !Arc::ptr_eq(&self.owner, &file.owner) {
            return Err(VfError::client(index, libc::EINVAL as u32));
        }
        file.check_credentials()
            .map_err(|error| error.with_index(index))
    }

    pub fn readv<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, AutoFile>>,
    ) -> VfResult<Vec<crate::ReadResult>> {
        self.readv_with_options(ops, crate::ReadOptions::default())
    }
    /// Consume a batch with an explicit aggregate read budget.
    pub fn readv_with_options<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, AutoFile>>,
        options: crate::ReadOptions,
    ) -> VfResult<Vec<crate::ReadResult>> {
        crate::read::consume_ops(
            ops,
            options.limit_or(self.limits.max_read_bytes),
            |requests, options| self.readv_owned(requests, options),
            |requests, bytes| self.readv_into_with_limit(requests, bytes),
        )
    }
    pub(crate) fn readv_owned(
        &self,
        requests: &[crate::ReadRequest<'_, AutoRead<'_>>],
        options: crate::ReadOptions,
    ) -> VfResult<Vec<ReadResult>> {
        let budget = options.limit_or(self.limits.max_read_bytes);
        if requests.iter().all(|request| request.range_ref().is_some()) {
            return self.readv_with_limit_projected(requests, budget, |request| {
                request.range_ref().expect("checked range requests")
            });
        }
        crate::read::read_batch(
            requests,
            budget,
            |ranges, bytes| self.readv_with_limit_projected(ranges, bytes, |request| request),
            |paths, bytes| {
                self.read_files_with_options(
                    paths,
                    crate::ReadAllOptions::new().max_total_bytes(bytes),
                )
            },
        )
    }

    fn readv_with_limit_projected<'a, T>(
        &self,
        requests: &[T],
        max_bytes: usize,
        project: impl for<'r> Fn(&'r T) -> &'r AutoRead<'a>,
    ) -> VfResult<Vec<ReadResult>> {
        let mut requested = 0usize;
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.check_owner(request.file, index)?;
            requested = requested
                .checked_add(request.length)
                .ok_or_else(|| VfError::client(index, libc::EFBIG as u32))?;
            if requested > max_bytes {
                return Err(VfError::client(index, libc::EFBIG as u32));
            }
        }
        let mut output = Vec::with_capacity(requests.len());
        let mut start = 0;
        while start < requests.len() {
            let mut end = start + 1;
            while end < requests.len()
                && project(&requests[start])
                    .file
                    .route
                    .same_backend(&project(&requests[end]).file.route)
            {
                end += 1;
            }
            match &project(&requests[start]).file.route {
                Route::Mounted => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
                        .map(&project)
                        .map(|request| {
                            let AutoFileInner::Mounted(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at(request.offset, request.length)
                        })
                        .collect();
                    output.extend(
                        self.mounted
                            .readv_with_limit(&batch, max_bytes)
                            .map_err(|e| indexed(e, start))?,
                    );
                }
                Route::Nfs(connection) => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
                        .map(&project)
                        .map(|request| {
                            let AutoFileInner::Nfs(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at(request.offset, request.length)
                        })
                        .collect();
                    output.extend(
                        connection
                            .client
                            .readv_with_limit(&batch, max_bytes)
                            .map_err(|e| indexed(e, start))?,
                    );
                }
            }
            start = end;
        }
        Ok(output)
    }

    fn readv_into_with_limit(
        &self,
        requests: &mut [AutoReadInto<'_>],
        max_bytes: usize,
    ) -> VfResult<Vec<ReadIntoResult>> {
        let mut requested = 0usize;
        for (index, request) in requests.iter().enumerate() {
            self.check_owner(request.file, index)?;
            requested = requested
                .checked_add(request.buffer.len())
                .ok_or_else(|| VfError::client(index, libc::EFBIG as u32))?;
            if requested > max_bytes {
                return Err(VfError::client(index, libc::EFBIG as u32));
            }
        }
        let mut output = Vec::with_capacity(requests.len());
        let mut start = 0;
        while start < requests.len() {
            let route = requests[start].file.route.clone();
            let mut end = start + 1;
            while end < requests.len() && route.same_backend(&requests[end].file.route) {
                end += 1;
            }
            match &route {
                Route::Mounted => {
                    let mut batch: Vec<_> = requests[start..end]
                        .iter_mut()
                        .map(|request| {
                            let AutoFileInner::Mounted(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at_into(request.offset, &mut *request.buffer)
                        })
                        .collect();
                    output.extend(
                        self.mounted
                            .readv_into_with_limit(&mut batch, max_bytes)
                            .map_err(|error| indexed(error, start))?,
                    );
                }
                Route::Nfs(connection) => {
                    let mut batch: Vec<_> = requests[start..end]
                        .iter_mut()
                        .map(|request| {
                            let AutoFileInner::Nfs(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at_into(request.offset, &mut *request.buffer)
                        })
                        .collect();
                    output.extend(
                        connection
                            .client
                            .readv_into_with_limit(&mut batch, max_bytes)
                            .map_err(|error| indexed(error, start))?,
                    );
                }
            }
            start = end;
        }
        Ok(output)
    }

    pub fn writev(&self, requests: &[crate::WriteOp<'_, AutoFile>]) -> VfResult<Vec<WriteResult>> {
        self.write_vector(requests, false)
    }

    /// Complete short writes without replaying failed or ambiguous mutations.
    /// Validate ownership, live handles, and positional ranges across the
    /// entire batch before dispatching any backend cohort. Server-side errors
    /// may still follow completed writes; this is not an atomic operation.
    pub fn write_allv(
        &self,
        requests: &[crate::WriteOp<'_, AutoFile>],
    ) -> VfResult<Vec<WriteResult>> {
        self.write_vector(requests, true)
    }

    fn write_vector(
        &self,
        requests: &[crate::WriteOp<'_, AutoFile>],
        complete: bool,
    ) -> VfResult<Vec<WriteResult>> {
        for (index, request) in requests.iter().enumerate() {
            self.check_owner(request.file(), index)?;
            if complete {
                // This is preflight, not transactional rollback: reject all
                // locally detectable invalid requests before any cohort writes.
                // Validate empty requests too, matching FsClient::write_allv.
                if request.file().is_closed() {
                    return Err(VfError::client(index, libc::EBADF as u32)
                        .with_context("write_allv", request.file().path()));
                }
                request
                    .offset()
                    .checked_add(request.data().len() as u64)
                    .ok_or_else(|| {
                        VfError::client(index, libc::EOVERFLOW as u32)
                            .with_context("write_allv", request.file().path())
                    })?;
            }
        }
        let mut output = Vec::with_capacity(requests.len());
        let mut start = 0;
        while start < requests.len() {
            let mut end = start + 1;
            while end < requests.len()
                && requests[start]
                    .file()
                    .route
                    .same_backend(&requests[end].file().route)
            {
                end += 1;
            }
            match &requests[start].file().route {
                Route::Mounted => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
                        .map(|request| {
                            let AutoFileInner::Mounted(file) = &request.file().inner else {
                                unreachable!()
                            };
                            file.write_request_at(request.offset(), request.data())
                        })
                        .collect();
                    let result = if complete {
                        self.mounted.write_allv(&batch)
                    } else {
                        self.mounted.writev(&batch)
                    };
                    output.extend(result.map_err(|e| indexed(e, start))?);
                }
                Route::Nfs(connection) => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
                        .map(|request| {
                            let AutoFileInner::Nfs(file) = &request.file().inner else {
                                unreachable!()
                            };
                            file.write_request_at(request.offset(), request.data())
                        })
                        .collect();
                    let result = if complete {
                        connection.client.write_allv(&batch)
                    } else {
                        connection.client.writev(&batch)
                    };
                    output.extend(result.map_err(|e| indexed(e, start))?);
                }
            }
            start = end;
        }
        Ok(output)
    }

    /// Retain every handle on cohort failure. Already completed cohorts are
    /// closed; a failing cohort may have a server-side completed prefix.
    pub fn try_closev(&self, files: &mut [AutoFile]) -> VfResult<()> {
        for (index, file) in files.iter().enumerate() {
            self.check_owner(file, index)?;
        }
        let mut start = 0;
        while start < files.len() {
            if files[start].is_closed() {
                start += 1;
                continue;
            }
            let route = files[start].route.clone();
            let mut end = start + 1;
            while end < files.len()
                && !files[end].is_closed()
                && route.same_backend(&files[end].route)
            {
                end += 1;
            }
            match &route {
                Route::Mounted => {
                    let batch = files[start..end].iter_mut().map(|file| {
                        let AutoFileInner::Mounted(file) = &mut file.inner else {
                            unreachable!()
                        };
                        file
                    });
                    self.mounted
                        .try_closev(batch)
                        .map_err(|error| indexed(error, start))?;
                }
                Route::Nfs(connection) => {
                    let batch = files[start..end].iter_mut().map(|file| {
                        let AutoFileInner::Nfs(file) = &mut file.inner else {
                            unreachable!()
                        };
                        file
                    });
                    connection
                        .client
                        .try_closev(batch)
                        .map_err(|error| indexed(error, start))?;
                }
            }
            start = end;
        }
        Ok(())
    }

    /// Read a complete file with the same default allocation limit as
    /// `FsClient::read`; use `AutoFile` for streaming larger files.
    #[cfg(test)]
    pub(crate) fn read(&self, path: impl AsRef<Path>) -> VfResult<Vec<u8>> {
        self.read_with_limit(path, self.limits.max_read_bytes)
    }

    #[cfg(test)]
    pub(crate) fn read_with_limit(
        &self,
        path: impl AsRef<Path>,
        limit: usize,
    ) -> VfResult<Vec<u8>> {
        let path = path.as_ref();
        let route = self.resolve(path, &read_mounts(false));
        match route.route {
            Route::Mounted => self.mounted.read_with_limit(&route.path, limit),
            Route::Nfs(connection) => connection.client.read_with_limit(&route.path, limit),
        }
    }

    pub fn remove_file(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        let route = self.resolve(path, &read_mounts(false));
        match route.route {
            Route::Mounted => self.mounted.remove_file(&route.path),
            Route::Nfs(connection) => connection.client.remove_file(&route.path),
        }
    }

    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfResult<()> {
        let from = from.as_ref();
        let to = to.as_ref();
        let mounts = read_mounts(false);
        let source = self.resolve(from, &mounts);
        let destination = self.resolve(to, &mounts);
        match (&source.route, &destination.route) {
            (Route::Nfs(a), Route::Nfs(_)) if source.route.same_backend(&destination.route) => {
                a.client.rename(&source.path, &destination.path)
            }
            _ => self.mounted.rename(from, to),
        }
    }
}

fn indexed(error: VfError, start: usize) -> VfError {
    match error.index() {
        Some(index) => error.with_index(start + index),
        None => error,
    }
}

fn cohort_end(resolved: &[Resolved], start: usize) -> usize {
    let mut end = start + 1;
    while end < resolved.len() && resolved[start].route.same_backend(&resolved[end].route) {
        end += 1;
    }
    end
}

/// OpenOptions-style builder for a mount-aware client.
#[derive(Clone)]
pub struct AutoOpenOptions<'a> {
    client: &'a AutoClient,
    flags: OpenFlags,
    mode: u32,
}

/// Metadata builder with the same application semantics as `SetMetadata`.
#[derive(Clone)]
pub struct AutoSetMetadata<'a> {
    client: &'a AutoClient,
    path: PathBuf,
    update: vfsi_core::MetadataUpdate,
    follow: bool,
}

macro_rules! metadata_setter {
    ($name:ident, $type:ty) => {
        pub fn $name(&mut self, value: $type) -> &mut Self {
            self.update.$name = Some(value);
            self
        }
    };
}

impl AutoSetMetadata<'_> {
    metadata_setter!(permissions, crate::Permissions);
    metadata_setter!(len, u64);
    metadata_setter!(accessed, std::time::SystemTime);
    metadata_setter!(modified, std::time::SystemTime);
    pub fn follow_symlinks(&mut self, follow: bool) -> &mut Self {
        self.follow = follow;
        self
    }
    pub fn apply(&self) -> VfResult<()> {
        macro_rules! apply {
            ($client:expr, $path:expr) => {{
                let mut builder = $client.set_metadata($path);
                builder.follow_symlinks(self.follow);
                if let Some(value) = self.update.permissions {
                    builder.permissions(value);
                }
                if let Some(value) = self.update.len {
                    builder.len(value);
                }
                if let Some(value) = self.update.accessed {
                    builder.accessed(value);
                }
                if let Some(value) = self.update.modified {
                    builder.modified(value);
                }
                builder.apply()
            }};
        }
        let route = self.client.resolve(&self.path, &read_mounts(false));
        match route.route {
            Route::Mounted => apply!(self.client.mounted, &route.path),
            Route::Nfs(connection) => apply!(connection.client, &route.path),
        }
    }
}

enum AutoDirInner {
    Mounted(vfsi_sync::FsDir<DummyVecFs>),
    Nfs(vfsi_sync::FsDir<vfsi_nfs::NfsVecFs>),
}

/// Owned handle-rooted directory. Path-only backends fail at open rather than
/// weakening the handle-rooted safety contract.
pub struct AutoDir {
    path: PathBuf,
    route: Route,
    inner: AutoDirInner,
}

impl std::fmt::Debug for AutoDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoDir")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl AutoDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn is_closed(&self) -> bool {
        match &self.inner {
            AutoDirInner::Mounted(dir) => dir.is_closed(),
            AutoDirInner::Nfs(dir) => dir.is_closed(),
        }
    }
    pub fn remove_contents(&self) -> VfResult<()> {
        self.remove_contents_with_options(crate::RemoveOptions::default())
    }
    pub fn remove_contents_with_options(&self, options: crate::RemoveOptions) -> VfResult<()> {
        if let Route::Nfs(connection) = &self.route
            && AuthSysIdentity::current().as_ref() != Some(&connection.credentials)
        {
            return Err(
                VfError::client(0, libc::EACCES as u32).with_context("auto_auth", &self.path)
            );
        }
        match &self.inner {
            AutoDirInner::Mounted(dir) => dir.remove_contents_with_options(options),
            AutoDirInner::Nfs(dir) => dir.remove_contents_with_options(options),
        }
    }
    pub fn try_close(&mut self) -> VfResult<()> {
        match &mut self.inner {
            AutoDirInner::Mounted(dir) => dir.try_close(),
            AutoDirInner::Nfs(dir) => dir.try_close(),
        }
    }
    pub fn close(mut self) -> VfResult<()> {
        self.try_close()
    }
}

macro_rules! auto_open_flag {
    ($name:ident, $flag:ident) => {
        pub fn $name(&mut self, enabled: bool) -> &mut Self {
            self.flags.set(OpenFlags::$flag, enabled);
            self
        }
    };
}

impl AutoOpenOptions<'_> {
    auto_open_flag!(read, READ);
    auto_open_flag!(write, WRITE);
    auto_open_flag!(append, APPEND);
    auto_open_flag!(truncate, TRUNCATE);
    auto_open_flag!(create, CREATE);
    auto_open_flag!(create_new, CREATE_NEW);
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }
    pub fn open(&self, path: impl AsRef<Path>) -> VfResult<AutoFile> {
        self.client
            .open_with(OpenRequest::new(path.as_ref(), self.flags).mode(self.mode))
    }
    pub fn openv<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<AutoFile>> {
        self.client.openv(
            &paths
                .iter()
                .map(|path| OpenRequest::new(path.as_ref(), self.flags).mode(self.mode))
                .collect::<Vec<_>>(),
        )
    }
}

enum AutoFileInner {
    Mounted(FsFile<DummyVecFs>),
    Nfs(NfsFile),
}

/// Open file pinned to the backend chosen when it was opened.
pub struct AutoFile {
    path: PathBuf,
    route: Route,
    inner: AutoFileInner,
    owner: Arc<()>,
}

impl std::fmt::Debug for AutoFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AutoFile")
            .field("path", &self.path)
            .field("route", &self.route.public())
            .finish_non_exhaustive()
    }
}

impl AutoFile {
    pub fn is_closed(&self) -> bool {
        match &self.inner {
            AutoFileInner::Mounted(file) => file.is_closed(),
            AutoFileInner::Nfs(file) => file.is_closed(),
        }
    }
    fn new(path: PathBuf, route: Route, inner: AutoFileInner, owner: &Arc<()>) -> Self {
        Self {
            path,
            route,
            inner,
            owner: Arc::clone(owner),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn check_credentials(&self) -> VfResult<()> {
        if let Route::Nfs(connection) = &self.route
            && AuthSysIdentity::current().as_ref() != Some(&connection.credentials)
        {
            return Err(
                VfError::client(0, libc::EACCES as u32).with_context("auto_auth", &self.path)
            );
        }
        Ok(())
    }
    pub fn route(&self) -> AutoRoute {
        self.route.public()
    }

    pub(crate) fn read_request_at(&self, offset: u64, length: usize) -> AutoRead<'_> {
        AutoRead {
            file: self,
            offset,
            length,
        }
    }
    pub(crate) fn read_request_at_into<'a>(
        &'a self,
        offset: u64,
        buffer: &'a mut [u8],
    ) -> AutoReadInto<'a> {
        AutoReadInto {
            file: self,
            offset,
            buffer,
        }
    }
    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> VfResult<usize> {
        self.check_credentials()?;
        let count = buffer.len().min(READ_CHUNK);
        let buffer = &mut buffer[..count];
        match &self.inner {
            AutoFileInner::Mounted(file) => file.read_at(buffer, offset),
            AutoFileInner::Nfs(file) => file.read_at(buffer, offset),
        }
    }
    pub fn read_native(&mut self, buffer: &mut [u8]) -> VfResult<usize> {
        self.check_credentials()?;
        let count = buffer.len().min(READ_CHUNK);
        let buffer = &mut buffer[..count];
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.read_native(buffer),
            AutoFileInner::Nfs(file) => file.read_native(buffer),
        }
    }
    /// Bounded cursor-based collection from the already-opened object.
    /// On failure the cursor can advance, including a one-byte EOF probe.
    pub fn read_to_end_with_limit(&mut self, max_bytes: usize) -> VfResult<Vec<u8>> {
        self.check_credentials()?;
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.read_to_end_with_limit(max_bytes),
            AutoFileInner::Nfs(file) => file.read_to_end_with_limit(max_bytes),
        }
        .map_err(|error| error.with_context("read_to_end_with_limit", &self.path))
    }

    pub fn write_native(&mut self, buffer: &[u8]) -> VfResult<usize> {
        self.check_credentials()?;
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.write_native(buffer),
            AutoFileInner::Nfs(file) => file.write_native(buffer),
        }
    }
    pub fn write_at(&self, buffer: &[u8], offset: u64) -> VfResult<usize> {
        self.check_credentials()?;
        match &self.inner {
            AutoFileInner::Mounted(file) => file.write_at(buffer, offset),
            AutoFileInner::Nfs(file) => file.write_at(buffer, offset),
        }
    }
    pub fn metadata(&self) -> VfResult<crate::Metadata> {
        self.check_credentials()?;
        match &self.inner {
            AutoFileInner::Mounted(file) => file.metadata(),
            AutoFileInner::Nfs(file) => file.metadata(),
        }
    }
    pub fn sync_all(&self) -> VfResult<()> {
        self.check_credentials()?;
        match &self.inner {
            AutoFileInner::Mounted(file) => file.sync_all(),
            AutoFileInner::Nfs(file) => file.sync_all(),
        }
    }
    pub fn sync_data(&self) -> VfResult<()> {
        self.check_credentials()?;
        match &self.inner {
            AutoFileInner::Mounted(file) => file.sync_data(),
            AutoFileInner::Nfs(file) => file.sync_data(),
        }
    }
    pub fn set_len(&self, len: u64) -> VfResult<()> {
        self.check_credentials()?;
        match &self.inner {
            AutoFileInner::Mounted(file) => file.set_len(len),
            AutoFileInner::Nfs(file) => file.set_len(len),
        }
    }
    pub fn set_permissions(&self, permissions: crate::Permissions) -> VfResult<()> {
        self.check_credentials()?;
        match &self.inner {
            AutoFileInner::Mounted(file) => file.set_permissions(permissions),
            AutoFileInner::Nfs(file) => file.set_permissions(permissions),
        }
    }
    pub fn seek_native(&mut self, position: SeekFrom) -> VfResult<u64> {
        self.check_credentials()?;
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.seek_native(position),
            AutoFileInner::Nfs(file) => file.seek_native(position),
        }
    }
    pub fn try_close(&mut self) -> VfResult<()> {
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.try_close(),
            AutoFileInner::Nfs(file) => file.try_close(),
        }
    }
    pub fn close(self) -> VfResult<()> {
        match self.inner {
            AutoFileInner::Mounted(file) => file.close(),
            AutoFileInner::Nfs(file) => file.close(),
        }
    }
}

impl Read for AutoFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check_credentials().map_err(io::Error::from)?;
        let count = buffer.len().min(READ_CHUNK);
        let buffer = &mut buffer[..count];
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.read(buffer),
            AutoFileInner::Nfs(file) => file.read(buffer),
        }
    }
}
impl Write for AutoFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.check_credentials().map_err(io::Error::from)?;
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.write(buffer),
            AutoFileInner::Nfs(file) => file.write(buffer),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check_credentials().map_err(io::Error::from)?;
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.flush(),
            AutoFileInner::Nfs(file) => file.flush(),
        }
    }
}
impl Seek for AutoFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check_credentials().map_err(io::Error::from)?;
        match &mut self.inner {
            AutoFileInner::Mounted(file) => file.seek(position),
            AutoFileInner::Nfs(file) => file.seek(position),
        }
    }
}

/// Positional read request for [`AutoClient::readv`].
pub struct AutoRead<'a> {
    file: &'a AutoFile,
    offset: u64,
    length: usize,
}
/// Internal borrowed-buffer representation used by [`crate::ReadOp::into`].
pub struct AutoReadInto<'a> {
    file: &'a AutoFile,
    offset: u64,
    buffer: &'a mut [u8],
}

fn read_mounts(include_covering_mounts: bool) -> MountTable {
    fs::read("/proc/self/mountinfo")
        .ok()
        .map_or_else(MountTable::default, |contents| {
            let mut table = MountTable::default();
            for line in contents.split(|byte| *byte == b'\n') {
                if include_covering_mounts && let Some(point) = mount_point(line) {
                    table.mount_points.insert(point);
                }
                if let Some(spec) = parse_mount(line) {
                    table.eligible.push(spec);
                }
            }
            table
        })
}

fn mount_point(line: &[u8]) -> Option<PathBuf> {
    decode_mount_field(line.split(|byte| *byte == b' ').nth(4)?)
}

fn parse_mount(line: &[u8]) -> Option<MountSpec> {
    let info = vfsi_nfs::mount::parse_mount(line)?;
    if info.read_only {
        return None;
    }
    Some(MountSpec {
        id: info.id,
        mount_point: info.mount_point,
        export: info.export,
        server: info.server,
        minor: info.minor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn require_live_direct_route(client: &Auto, path: &Path) {
        // Ganesha/kernel mount setup can briefly return EREMOTEIO. Retry only
        // the read-only eligibility probe, never an application mutation.
        let mut route = client.route_for(path);
        for delay_ms in [5, 20, 100] {
            if matches!(route, AutoRoute::DirectNfs { .. }) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            route = client.route_for(path);
        }
        assert!(
            matches!(route, AutoRoute::DirectNfs { .. }),
            "expected direct NFS for {path:?}, got {route:?}"
        );
    }

    #[test]
    fn public_path_budget_checks_exact_bounds_and_overflow_before_delivery() {
        let mut budget = PathByteBudget::new(6);
        budget.charge(Path::new("/a"), "visit_dir").unwrap();
        budget.charge(Path::new("/bbb"), "visit_dir").unwrap();
        let error = budget.charge(Path::new("/c"), "visit_dir").unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::FileTooLarge);
        assert_eq!(error.operation(), Some("visit_dir"));
        assert_eq!(error.path(), Some(Path::new("/c")));
        let mut budget = PathByteBudget {
            used: usize::MAX,
            limit: usize::MAX,
        };
        assert!(budget.charge(Path::new("/a"), "walk").is_err());
    }

    #[test]
    fn parses_only_unambiguous_supported_mounts() {
        let line = b"46 49 0:45 / /mnt/nfs rw,relatime - nfs4 server:/export rw,vers=4.2,proto=tcp,sec=sys,addr=192.0.2.7";
        let spec = parse_mount(line).unwrap();
        assert_eq!(spec.mount_point, Path::new("/mnt/nfs"));
        assert_eq!(spec.server, "192.0.2.7:2049");
        assert_eq!(spec.export, Path::new("/export"));
        assert_eq!(spec.minor, 2);
        let line = std::str::from_utf8(line).unwrap();
        assert!(parse_mount(line.replace("sec=sys", "sec=krb5").as_bytes()).is_none());
        assert!(parse_mount(line.replace("sec=sys", "sec=sys:krb5").as_bytes()).is_none());
        assert!(parse_mount(line.replace("sec=sys", "sec=sys,sec=krb5").as_bytes()).is_none());
        assert!(parse_mount(line.replace("vers=4.2", "vers=3").as_bytes()).is_none());
        assert!(parse_mount(line.replace("vers=4.2", "vers=4.0").as_bytes()).is_none());
        assert!(parse_mount(line.replace(" rw,relatime", " ro,relatime").as_bytes()).is_none());
        assert!(parse_mount(line.replace(" / /mnt/nfs ", " /sub /mnt/nfs ").as_bytes()).is_none());
        assert!(parse_mount(line.replace("sec=sys", "sec=sys,port=bad").as_bytes()).is_none());
        assert!(parse_mount(line.replace("sec=sys", "sec=sys,port=0").as_bytes()).is_none());
        assert!(parse_mount(line.replace(",addr=192.0.2.7", "").as_bytes()).is_none());
        assert!(parse_mount(line.replace("addr=192.0.2.7", "addr=server").as_bytes()).is_none());
        assert!(
            parse_mount(
                line.replace("addr=192.0.2.7", "addr=192.0.2.7,addr=192.0.2.8")
                    .as_bytes()
            )
            .is_none()
        );
        assert_eq!(
            parse_mount(
                line.replace("addr=192.0.2.7", "addr=2001:db8::7")
                    .as_bytes()
            )
            .unwrap()
            .server,
            "[2001:db8::7]:2049"
        );
    }

    #[test]
    fn decodes_mountinfo_escapes() {
        assert_eq!(
            decode_mount_field(br"/mnt/with\040space").unwrap(),
            Path::new("/mnt/with space")
        );
        assert!(decode_mount_field(br"/mnt/bad\08x").is_none());
        assert!(decode_mount_field(br"/mnt/bad\777").is_none());
    }

    #[test]
    fn mount_id_prevents_prefix_based_misrouting() {
        let client = Auto::new("/").unwrap();
        let local = std::env::temp_dir();
        let fake = MountSpec {
            id: u64::MAX,
            mount_point: PathBuf::from("/"),
            export: PathBuf::from("/"),
            server: "127.0.0.1:2049".into(),
            minor: 2,
        };
        assert!(matches!(
            client
                .resolve(
                    &local,
                    &MountTable {
                        eligible: vec![fake],
                        mount_points: HashSet::new(),
                    }
                )
                .route,
            Route::Mounted
        ));
    }

    #[test]
    fn local_paths_stay_mounted_and_open_file_roundtrips() {
        let root = std::env::temp_dir().join(format!(
            "vnfs-auto-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&root).unwrap();
        let client = Auto::new(&root).unwrap();
        assert_eq!(client.route_for("/file"), AutoRoute::Mounted);
        let files = client
            .openv(&[
                OpenRequest::new(
                    "/one",
                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
                ),
                OpenRequest::new(
                    "/two",
                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
                ),
            ])
            .unwrap();
        client
            .writev(&[
                crate::WriteOp::at(&files[0], 0, b"one"),
                crate::WriteOp::at(&files[1], 0, b"two"),
            ])
            .unwrap();
        let reads = client
            .readv_with_options(
                [
                    crate::ReadOp::range(&files[0], 0, 3),
                    crate::ReadOp::range(&files[1], 0, 3),
                ],
                crate::ReadOptions::default(),
            )
            .unwrap();
        assert_eq!(reads[0].data.as_deref().unwrap(), b"one");
        assert_eq!(reads[1].data.as_deref().unwrap(), b"two");
        std::os::unix::fs::symlink("one", root.join("link")).unwrap();
        let metadata = client
            .mounted
            .symlink_metadatav(&[Path::new("/one"), Path::new("/link")])
            .unwrap();
        assert_eq!(metadata[0].file_type(), crate::FileType::Regular);
        assert_eq!(metadata[1].file_type(), crate::FileType::Symlink);
        let error = client
            .mounted
            .symlink_metadatav(&[Path::new("/one"), Path::new("/missing")])
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        let tiny = Auto::new(&root).unwrap().with_limits(ResourceLimits {
            max_read_bytes: 5,
            ..ResourceLimits::default()
        });
        let tiny_file = tiny.open("/one").unwrap();
        let error = tiny
            .readv_with_options(
                [crate::ReadOp::range(&tiny_file, 0, 6)],
                crate::ReadOptions::default(),
            )
            .unwrap_err();
        assert_eq!(error.err_no(), libc::EFBIG as u32);
        assert_eq!(
            tiny.readv_with_options(
                [crate::ReadOp::range(&tiny_file, 0, 3)],
                crate::ReadOptions::default()
            )
            .unwrap()[0]
                .data
                .as_deref()
                .unwrap(),
            b"one"
        );
        tiny_file.close().unwrap();
        let error = client
            .readv_with_options(
                [crate::ReadOp::range(
                    &files[0],
                    0,
                    DEFAULT_READ_ALLV_MAX_TOTAL_BYTES + 1,
                )],
                crate::ReadOptions::default(),
            )
            .unwrap_err();
        assert_eq!(error.err_no(), libc::EFBIG as u32);
        client.closev(files).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn write_allv_preflights_closed_empty_and_overflowing_requests_locally() {
        let root = std::env::temp_dir().join(format!(
            "auto-local-preflight-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&root).unwrap();
        let client = Auto::new(&root).unwrap();
        for closed in [false, true] {
            client.write("/first", b"original").unwrap();
            client.write("/second", b"original").unwrap();
            let mut files = client
                .openv(
                    &["/first", "/second"]
                        .map(|path| OpenRequest::new(path, OpenFlags::READ | OpenFlags::WRITE)),
                )
                .unwrap();
            if closed {
                files[1].try_close().unwrap();
            }
            let error = client
                .write_allv(&[
                    crate::WriteOp::at(&files[0], 0, b"changed"),
                    crate::WriteOp::at(
                        &files[1],
                        if closed { 0 } else { u64::MAX },
                        if closed { b"" } else { b"XX" },
                    ),
                ])
                .unwrap_err();
            let first = client.read("/first").unwrap();
            client.try_closev(&mut files).unwrap();
            assert_eq!(first, b"original");
            assert_eq!(error.index(), Some(1));
            assert_eq!(
                error.err_no(),
                if closed { libc::EBADF } else { libc::EOVERFLOW } as u32
            );
            assert_eq!(error.operation(), Some("write_allv"));
            assert_eq!(error.path(), Some(Path::new("/second")));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_nfs_mount_uses_direct_connection() {
        let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
            .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
        let client = Auto::new("/").unwrap();
        let mount = PathBuf::from(mount);
        let mut route = client.route_for(&mount);
        // A just-mounted kernel NFS client can briefly return EREMOTEIO for
        // the root stat. Auto safely falls back for that attempt; retry the
        // read-only route probe after the mount has settled.
        for delay_ms in [5, 20] {
            if matches!(&route, AutoRoute::DirectNfs { .. }) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            route = client.route_for(&mount);
        }
        assert!(
            matches!(&route, AutoRoute::DirectNfs { .. }),
            "Auto chose {route:?}"
        );
        assert!(client.metadata(&mount).unwrap().is_dir());
        let unique = format!(
            "vnfs-auto-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let first = mount.join(format!("{unique}-1"));
        let second = mount.join(format!("{unique}-2"));
        let local = std::env::temp_dir().join(format!("{unique}-local"));
        assert_eq!(client.route_for(&local), AutoRoute::Mounted);
        let requests = [first.as_path(), second.as_path(), local.as_path()].map(|path| {
            OpenRequest::new(
                path,
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE_NEW,
            )
        });
        let files = client.openv(&requests).unwrap();
        assert!(matches!(files[0].route(), AutoRoute::DirectNfs { .. }));
        assert!(matches!(files[1].route(), AutoRoute::DirectNfs { .. }));
        assert_eq!(files[2].route(), AutoRoute::Mounted);
        client
            .writev(&[
                crate::WriteOp::at(&files[0], 0, b"first"),
                crate::WriteOp::at(&files[1], 0, b"second"),
                crate::WriteOp::at(&files[2], 0, b"local"),
            ])
            .unwrap();
        let reads = client
            .readv_with_options(
                [
                    crate::ReadOp::range(&files[0], 0, 5),
                    crate::ReadOp::range(&files[1], 0, 6),
                    crate::ReadOp::range(&files[2], 0, 5),
                ],
                crate::ReadOptions::default(),
            )
            .unwrap();
        assert_eq!(reads[0].data.as_deref().unwrap(), b"first");
        assert_eq!(reads[1].data.as_deref().unwrap(), b"second");
        assert_eq!(reads[2].data.as_deref().unwrap(), b"local");
        let mut buffers = [[0_u8; 8]; 3];
        let [first_buffer, second_buffer, local_buffer] = &mut buffers;
        let reads = client
            .readv([
                crate::ReadOp::into(&files[0], 0, first_buffer),
                crate::ReadOp::into(&files[1], 0, second_buffer),
                crate::ReadOp::into(&files[2], 0, local_buffer),
            ])
            .unwrap();
        assert_eq!(
            reads.iter().map(|read| read.read).collect::<Vec<_>>(),
            [5, 6, 5]
        );
        assert!(reads.iter().all(|read| read.eof));
        assert_eq!(&buffers[0][..5], b"first");
        assert_eq!(&buffers[1][..6], b"second");
        assert_eq!(&buffers[2][..5], b"local");
        client
            .write_allv(&[
                crate::WriteOp::at(&files[0], 0, b"FIRST"),
                crate::WriteOp::at(&files[1], 0, b"SECOND"),
                crate::WriteOp::at(&files[2], 0, b"LOCAL"),
            ])
            .unwrap();
        let mut files = files;
        client.try_closev(&mut files).unwrap();
        client.try_closev(&mut files).unwrap();
        assert!(files.iter().all(AutoFile::is_closed));
        let paths = [first.as_path(), second.as_path(), local.as_path()];
        assert_eq!(
            client.read_files(&paths).unwrap(),
            [b"FIRST".to_vec(), b"SECOND".to_vec(), b"LOCAL".to_vec()]
        );
        assert!(
            client
                .read_files_with_options(&paths, crate::ReadOptions::new().max_total_bytes(15))
                .is_err()
        );
        let existing = client
            .openv(&[
                OpenRequest::new(
                    &first,
                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
                ),
                OpenRequest::new(
                    &second,
                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
                ),
            ])
            .unwrap();
        assert!(
            existing
                .iter()
                .all(|file| matches!(file.route(), AutoRoute::DirectNfs { .. }))
        );
        client.closev(existing).unwrap();
        let missing = std::env::temp_dir().join(format!("{unique}-missing"));
        let error = client
            .openv(&[
                OpenRequest::new(&first, OpenFlags::READ),
                OpenRequest::new(&second, OpenFlags::READ),
                OpenRequest::new(&missing, OpenFlags::READ),
            ])
            .unwrap_err();
        assert_eq!(error.index(), Some(2));
        let link = mount.join(format!("{unique}-symlink"));
        std::os::unix::fs::symlink(&local, &link).unwrap();
        let linked = client.open(&link).unwrap();
        assert_eq!(linked.route(), AutoRoute::Mounted);
        linked.close().unwrap();
        let entries = client.read_dir(&mount).unwrap();
        assert!(entries.iter().any(|entry| entry.path() == first));
        client.remove_file(&link).unwrap();
        client.remove_file(&first).unwrap();
        client.remove_file(&second).unwrap();
        client.remove_file(&local).unwrap();
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_changed_auto_limits_apply_to_cached_connections_and_open_handles() {
        let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
            .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
        let path = Path::new(&mount).join(format!(
            "auto-limits-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::write(&path, b"abcdef").unwrap();
        let mut outcomes = Vec::new();
        for reopen in [false, true] {
            let client = Auto::new("/").unwrap().with_limits(ResourceLimits {
                max_read_bytes: 4,
                ..Default::default()
            });
            let file = client.open(&path).unwrap();
            assert!(matches!(file.route(), AutoRoute::DirectNfs { .. }));
            let client = client.with_limits(ResourceLimits {
                max_read_bytes: 8,
                ..Default::default()
            });
            let file = if reopen {
                file.close().unwrap();
                client.open(&path).unwrap()
            } else {
                file
            };
            let mut buffer = [0; 6];
            outcomes.push(
                client
                    .readv([crate::ReadOp::into(&file, 0, &mut buffer)])
                    .map(|results| {
                        assert_eq!(results[0].read, 6);
                        assert_eq!(&buffer, b"abcdef");
                    }),
            );
            let client = client.with_limits(ResourceLimits {
                max_read_bytes: 3,
                ..Default::default()
            });
            buffer.fill(0xff);
            assert_eq!(
                client
                    .readv([crate::ReadOp::into(&file, 0, &mut buffer)])
                    .unwrap_err()
                    .kind(),
                crate::ErrorKind::FileTooLarge
            );
            assert_eq!(buffer, [0xff; 6]);
            file.close().unwrap();
        }
        fs::remove_file(path).unwrap();
        assert!(
            outcomes.iter().all(Result::is_ok),
            "old-handle and reopened-handle results: {outcomes:?}"
        );
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_write_allv_preflights_invalid_later_requests_across_routes() {
        let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
            .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
        let unique = format!(
            "auto-write-preflight-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let client = Auto::new("/").unwrap();
        let mut observations = Vec::new();
        for remote_first in [false, true] {
            // Cover overflow, a closed nonempty request, and a closed empty
            // request. An empty payload must not bypass handle validation.
            for invalid in 0..3 {
                // Do not reuse a pathname removed through the kernel while a
                // direct NFS client still caches its former filehandle.
                let case = format!("{unique}-{remote_first}-{invalid}");
                let remote = Path::new(&mount).join(&case);
                let local = std::env::temp_dir().join(&case);
                fs::write(&remote, b"original").unwrap();
                fs::write(&local, b"original").unwrap();
                require_live_direct_route(&client, &remote);
                let paths = if remote_first {
                    [&remote, &local]
                } else {
                    [&local, &remote]
                };
                let mut files = client
                    .openv(
                        &paths
                            .map(|path| OpenRequest::new(path, OpenFlags::READ | OpenFlags::WRITE)),
                    )
                    .unwrap();
                assert_eq!(
                    matches!(files[0].route(), AutoRoute::DirectNfs { .. }),
                    remote_first
                );
                assert_eq!(
                    matches!(files[1].route(), AutoRoute::DirectNfs { .. }),
                    !remote_first
                );
                if invalid != 0 {
                    files[1].try_close().unwrap();
                }
                let payload: &[u8] = if invalid == 2 { b"" } else { b"XX" };
                let error = client
                    .write_allv(&[
                        crate::WriteOp::at(&files[0], 0, b"changed"),
                        crate::WriteOp::at(
                            &files[1],
                            if invalid == 0 { u64::MAX } else { 0 },
                            payload,
                        ),
                    ])
                    .unwrap_err();
                let mut first = [0; 8];
                assert_eq!(files[0].read_at(&mut first, 0).unwrap(), first.len());
                let second = client.read(paths[1]).unwrap();
                observations.push((
                    remote_first,
                    invalid,
                    error,
                    first,
                    second,
                    paths[1].to_path_buf(),
                ));
                client.try_closev(&mut files).unwrap();
                fs::remove_file(&remote).unwrap();
                fs::remove_file(&local).unwrap();
            }
        }
        for (remote_first, invalid, error, first, second, failed_path) in observations {
            assert_eq!(
                &first, b"original",
                "earlier cohort changed: remote_first={remote_first}, invalid={invalid}"
            );
            assert_eq!(second, b"original");
            assert_eq!(error.index(), Some(1));
            assert_eq!(
                error.err_no(),
                if invalid == 0 {
                    libc::EOVERFLOW
                } else {
                    libc::EBADF
                } as u32
            );
            assert_eq!(error.operation(), Some("write_allv"));
            assert_eq!(error.path(), Some(failed_path.as_path()));
        }
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_auto_traversal_charges_public_path_bytes() {
        let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
            .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
        let root = Path::new(&mount).join(format!(
            "auto-quota-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"a").unwrap();
        fs::write(root.join("b"), b"b").unwrap();
        let client = Auto::new("/").unwrap();
        require_live_direct_route(&client, &root);
        let route = client.resolve(&root, &read_mounts(false));
        let Route::Nfs(connection) = &route.route else {
            panic!("expected direct NFS");
        };
        let listings = connection
            .client
            .walk_with_options(
                &route.path,
                crate::MetadataFields::stat(),
                crate::WalkOptions::unlimited(),
            )
            .unwrap();
        let backend_entries: usize = listings
            .iter()
            .flat_map(|listing| &listing.entries)
            .map(|entry| entry.path().as_os_str().len())
            .sum();
        let backend_total = backend_entries
            + listings
                .iter()
                .map(|listing| listing.path.as_os_str().len())
                .sum::<usize>();
        let public_entries: usize = listings
            .iter()
            .flat_map(|listing| &listing.entries)
            .map(|entry| {
                client
                    .public_path(connection, entry.path())
                    .unwrap()
                    .as_os_str()
                    .len()
            })
            .sum();
        let public_total = public_entries + root.as_os_str().len();
        assert!(public_entries > backend_entries && public_total > backend_total);
        let walk = client.walk_with_options(
            &root,
            crate::MetadataFields::stat(),
            crate::WalkOptions::new().max_path_bytes(backend_total),
        );
        let mut dir_bytes = 0;
        let dir = client.visit_dir_with_options(
            &root,
            ReadDirOptions::new().max_path_bytes(backend_entries),
            |entry| {
                dir_bytes += entry.path().as_os_str().len();
                Ok(std::ops::ControlFlow::Continue(()))
            },
        );
        let mut tree_bytes = root.as_os_str().len();
        let tree = client.visit_walk_with_options(
            &root,
            crate::WalkOptions::new().max_path_bytes(backend_total),
            |entry| {
                tree_bytes += entry.path().as_os_str().len();
                Ok(std::ops::ControlFlow::Continue(()))
            },
        );
        // Exact public budgets remain usable, including both callbacks.
        assert!(
            client
                .walk_with_options(
                    &root,
                    crate::MetadataFields::stat(),
                    crate::WalkOptions::new().max_path_bytes(public_total)
                )
                .is_ok()
        );
        assert!(
            client
                .visit_dir_with_options(
                    &root,
                    ReadDirOptions::new().max_path_bytes(public_entries),
                    |_| Ok(std::ops::ControlFlow::Continue(()))
                )
                .is_ok()
        );
        assert_eq!(
            client
                .visit_walk_with_options(
                    &root,
                    crate::WalkOptions::new().max_path_bytes(public_total),
                    |_| Ok(std::ops::ControlFlow::Continue(()))
                )
                .unwrap(),
            crate::TraversalCompletion::Complete
        );
        // Clean up through the client that traversed this directory instead
        // of mixing its direct NFS view with kernel directory caches.
        client.remove_dir_all(&root).unwrap();
        assert!(
            walk.is_err() && dir.is_err() && tree.is_err(),
            "walk={walk:?}; dir={dir:?}; tree={tree:?}"
        );
        assert!(dir_bytes <= backend_entries && tree_bytes <= backend_total);
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_ensure_empty_dir_respects_nested_mounts() {
        // The fixture is an NFS directory with a separately mounted child.
        // Its remote child contains hidden data that kernel routing cannot see.
        let root = std::env::var("VFSI_AUTO_TEST_NESTED_ROOT")
            .expect("VFSI_AUTO_TEST_NESTED_ROOT is required for this ignored integration test");
        let client = Auto::new("/").unwrap();
        require_live_direct_route(&client, Path::new(&root));
        let route = client.resolve(Path::new(&root), &read_mounts(false));
        let Route::Nfs(connection) = route.route else {
            panic!("expected parent direct route");
        };
        let hidden = route.path.join("child/hidden");
        assert_eq!(connection.client.read(&hidden).unwrap(), b"preserve\n");
        let result = client.ensure_empty_dir(&root);
        let preserved = connection.client.read(&hidden);
        assert!(
            result.is_err(),
            "kernel removal of a mounted child must fail"
        );
        assert_eq!(preserved.unwrap(), b"preserve\n");
    }

    #[test]
    #[ignore = "requires configured NFS mount fixtures"]
    fn live_file_bind_mount_uses_covering_mount() {
        let path = std::env::var("VFSI_AUTO_TEST_BIND")
            .expect("VFSI_AUTO_TEST_BIND is required for this ignored integration test");
        let client = Auto::new("/").unwrap();
        let mut file = client.open(&path).unwrap();
        assert_eq!(file.route(), AutoRoute::Mounted);
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "bind source\n");
        file.close().unwrap();
    }
}
