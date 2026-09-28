//! Linux mount-aware client. Only unambiguous NFSv4 AUTH_SYS mounts are
//! promoted to a separate direct NFS connection; everything else stays on
//! the kernel-mounted path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{
    DirEntry, FsClient, FsFile, Nfs, NfsClient, NfsFile, OpenFlags, OpenRequest, ReadDirOptions,
    ReadResult, VfError, VfResult, WriteResult,
};
use vfsi_local::DummyVecFs;
use vfsi_sync::DEFAULT_READ_ALLV_MAX_TOTAL_BYTES;

const READ_CHUNK: usize = 1024 * 1024;

/// Explicitly use Linux's mounted filesystem tree. This includes local,
/// NFS, SMB/CIFS, and other mounted filesystems without a second connection.
#[derive(Debug, Clone)]
pub struct Mounted(FsClient<DummyVecFs>);

impl Mounted {
    pub fn new(root: impl AsRef<Path>) -> VfResult<Self> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(VfError::client(0, libc::ENOTDIR as u32).with_context("mounted", root));
        }
        DummyVecFs::try_new(root.to_path_buf())
            .map(FsClient::new)
            .map(Self)
    }
}

impl std::ops::Deref for Mounted {
    type Target = FsClient<DummyVecFs>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Construct a mount-aware client. Unlike [`Mounted`], eligible NFSv4
/// AUTH_SYS mounts are accessed via direct, vectorized NFS COMPOUNDs.
#[derive(Debug)]
pub struct Auto(AutoClient);

impl Auto {
    pub fn new(root: impl AsRef<Path>) -> VfResult<Self> {
        let root = fs::canonicalize(root.as_ref())
            .map_err(|e| VfError::client(0, e.raw_os_error().unwrap_or(libc::EIO) as u32))?;
        if !root.is_dir() {
            return Err(VfError::client(0, libc::ENOTDIR as u32));
        }
        Ok(Self(AutoClient {
            mounted: Mounted::new(&root)?.0,
            root,
            connections: Mutex::new(HashMap::new()),
            owner: Arc::new(()),
            max_readv_bytes: DEFAULT_READ_ALLV_MAX_TOTAL_BYTES,
        }))
    }

    pub fn with_readv_limit(mut self, bytes: usize) -> Self {
        self.0.max_readv_bytes = bytes;
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

/// AUTH_SYS credentials are captured when the RPC connection is created.
/// Do not reuse them for a thread whose filesystem credentials differ.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthSysIdentity {
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
}

impl AuthSysIdentity {
    fn current() -> Option<Self> {
        // Linux rejects (uid_t)-1, making setfsuid/setfsgid a query without
        // changing the task's filesystem identity.
        let fsuid = unsafe { libc::setfsuid(!0) };
        let fsgid = unsafe { libc::setfsgid(!0) };
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        if fsuid as libc::uid_t != uid || fsgid as libc::gid_t != gid {
            return None;
        }
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        // AUTH_SYS carries at most 16 supplementary groups. Let the kernel
        // handle identities that would be truncated by the direct client.
        if !(0..=16).contains(&count) {
            return None;
        }
        let mut groups = vec![0; count as usize];
        if unsafe { libc::getgroups(count, groups.as_mut_ptr()) } != count {
            return None;
        }
        Some(Self { uid, gid, groups })
    }
}

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

/// Mount-aware client with lazily established per-mount NFS connections.
pub struct AutoClient {
    root: PathBuf,
    mounted: FsClient<DummyVecFs>,
    connections: Mutex<HashMap<u64, NfsConnection>>,
    owner: Arc<()>,
    max_readv_bytes: usize,
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
            .minor_version(Some(spec.minor))
            .connect()
            .ok()?;
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
                                if item.file_type() == crate::VfType::Symlink {
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

    pub fn open(&self, path: impl AsRef<Path>) -> VfResult<AutoFile> {
        self.open_with(OpenRequest::new(path.as_ref(), OpenFlags::READ))
    }

    pub fn create(&self, path: impl AsRef<Path>) -> VfResult<AutoFile> {
        self.open_with(OpenRequest::new(
            path.as_ref(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        ))
    }

    pub fn open_with(&self, request: OpenRequest) -> VfResult<AutoFile> {
        self.openv(&[request]).map(|mut files| files.remove(0))
    }

    /// Preserve request order, including the completed-prefix semantics of
    /// strict vector operations. Consecutive requests to one mount batch.
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

    pub fn readv(&self, requests: &[AutoRead<'_>]) -> VfResult<Vec<ReadResult>> {
        let mut requested = 0usize;
        for (index, request) in requests.iter().enumerate() {
            self.check_owner(request.file, index)?;
            requested = requested
                .checked_add(request.length)
                .ok_or_else(|| VfError::client(index, libc::EFBIG as u32))?;
            if requested > self.max_readv_bytes {
                return Err(VfError::client(index, libc::EFBIG as u32));
            }
        }
        let mut output = Vec::with_capacity(requests.len());
        let mut start = 0;
        while start < requests.len() {
            let mut end = start + 1;
            while end < requests.len()
                && requests[start]
                    .file
                    .route
                    .same_backend(&requests[end].file.route)
            {
                end += 1;
            }
            match &requests[start].file.route {
                Route::Mounted => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
                        .map(|request| {
                            let AutoFileInner::Mounted(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at(request.offset, request.length)
                        })
                        .collect();
                    output.extend(self.mounted.readv(&batch).map_err(|e| indexed(e, start))?);
                }
                Route::Nfs(connection) => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
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
                            .readv(&batch)
                            .map_err(|e| indexed(e, start))?,
                    );
                }
            }
            start = end;
        }
        Ok(output)
    }

    pub fn writev(&self, requests: &[AutoWrite<'_>]) -> VfResult<Vec<WriteResult>> {
        for (index, request) in requests.iter().enumerate() {
            self.check_owner(request.file, index)?;
        }
        let mut output = Vec::with_capacity(requests.len());
        let mut start = 0;
        while start < requests.len() {
            let mut end = start + 1;
            while end < requests.len()
                && requests[start]
                    .file
                    .route
                    .same_backend(&requests[end].file.route)
            {
                end += 1;
            }
            match &requests[start].file.route {
                Route::Mounted => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
                        .map(|request| {
                            let AutoFileInner::Mounted(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.write_request_at(request.offset, request.data)
                        })
                        .collect();
                    output.extend(self.mounted.writev(&batch).map_err(|e| indexed(e, start))?);
                }
                Route::Nfs(connection) => {
                    let batch: Vec<_> = requests[start..end]
                        .iter()
                        .map(|request| {
                            let AutoFileInner::Nfs(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.write_request_at(request.offset, request.data)
                        })
                        .collect();
                    output.extend(
                        connection
                            .client
                            .writev(&batch)
                            .map_err(|e| indexed(e, start))?,
                    );
                }
            }
            start = end;
        }
        Ok(output)
    }

    /// Close all handles, using a vector CLOSE for each contiguous backend
    /// cohort. Remaining handles are still dropped if one cohort fails.
    pub fn closev(&self, files: Vec<AutoFile>) -> VfResult<()> {
        for (index, file) in files.iter().enumerate() {
            self.check_owner(file, index)?;
        }
        let mut files = files.into_iter().peekable();
        let mut start = 0;
        while let Some(first) = files.next() {
            let route = first.route.clone();
            match first.inner {
                AutoFileInner::Mounted(file) => {
                    let mut group = vec![file];
                    while files
                        .peek()
                        .is_some_and(|next| route.same_backend(&next.route))
                    {
                        let next = files.next().expect("peeked");
                        let AutoFileInner::Mounted(file) = next.inner else {
                            unreachable!()
                        };
                        group.push(file);
                    }
                    let count = group.len();
                    self.mounted.closev(group).map_err(|e| indexed(e, start))?;
                    start += count;
                }
                AutoFileInner::Nfs(file) => {
                    let mut group = vec![file];
                    while files
                        .peek()
                        .is_some_and(|next| route.same_backend(&next.route))
                    {
                        let next = files.next().expect("peeked");
                        let AutoFileInner::Nfs(file) = next.inner else {
                            unreachable!()
                        };
                        group.push(file);
                    }
                    let count = group.len();
                    let Route::Nfs(connection) = route else {
                        unreachable!()
                    };
                    connection
                        .client
                        .closev(group)
                        .map_err(|e| indexed(e, start))?;
                    start += count;
                }
            }
        }
        Ok(())
    }

    pub fn metadata(&self, path: impl AsRef<Path>) -> VfResult<crate::Metadata> {
        let mounts = read_mounts(false);
        let route = self.resolve(path.as_ref(), &mounts);
        match route.route {
            Route::Mounted => self.mounted.metadata(&route.path),
            Route::Nfs(connection) => connection.client.metadata(&route.path),
        }
    }

    /// Read a complete file with the same default allocation limit as
    /// `FsClient::read`; use `AutoFile` for streaming larger files.
    pub fn read(&self, path: impl AsRef<Path>) -> VfResult<Vec<u8>> {
        self.read_with_limit(path, self.max_readv_bytes)
    }

    pub fn read_with_limit(&self, path: impl AsRef<Path>, limit: usize) -> VfResult<Vec<u8>> {
        let path = path.as_ref();
        let mut file = self.open(path)?;
        let mut output = Vec::new();
        let mut buffer = vec![0; READ_CHUNK.min(limit.saturating_add(1))];
        loop {
            let wanted = READ_CHUNK.min(limit.saturating_sub(output.len()).saturating_add(1));
            let count = file.read_native(&mut buffer[..wanted])?;
            if count == 0 {
                break;
            }
            if count > limit - output.len() {
                return Err(VfError::client(0, libc::EFBIG as u32).with_context("read", path));
            }
            output.extend_from_slice(&buffer[..count]);
        }
        file.close()?;
        Ok(output)
    }

    pub fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> VfResult<()> {
        let path = path.as_ref();
        let mut file = self.create(path)?;
        let mut written = 0;
        while written < data.len() {
            let count = file.write_native(&data[written..])?;
            if count == 0 {
                return Err(VfError::client(0, libc::EIO as u32));
            }
            written += count;
        }
        file.close()
    }

    pub fn create_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        let route = self.resolve(path, &read_mounts(false));
        match route.route {
            Route::Mounted => self.mounted.create_dir(&route.path),
            Route::Nfs(connection) => connection.client.create_dir(&route.path),
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

    pub fn copy(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfResult<()> {
        let from = from.as_ref();
        let to = to.as_ref();
        let mounts = read_mounts(false);
        let source = self.resolve(from, &mounts);
        let destination = self.resolve(to, &mounts);
        match (&source.route, &destination.route) {
            (Route::Nfs(a), Route::Nfs(_)) if source.route.same_backend(&destination.route) => {
                a.client.copy(&source.path, &destination.path)
            }
            _ => self.mounted.copy(from, to),
        }
    }

    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfResult<Vec<DirEntry>> {
        self.read_dir_with_options(path, ReadDirOptions::default())
    }

    pub fn read_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: ReadDirOptions,
    ) -> VfResult<Vec<DirEntry>> {
        let route = self.resolve(path.as_ref(), &read_mounts(false));
        match route.route {
            Route::Mounted => self.mounted.read_dir_with_options(&route.path, options),
            Route::Nfs(connection) => {
                let entries = connection
                    .client
                    .read_dir_with_options(&route.path, options)?;
                entries
                    .into_iter()
                    .map(|entry| {
                        let suffix = entry
                            .path()
                            .strip_prefix("/")
                            .map_err(|_| VfError::client(0, libc::EIO as u32))?;
                        let host = connection.spec.mount_point.join(suffix);
                        let relative = host
                            .strip_prefix(&self.root)
                            .map_err(|_| VfError::client(0, libc::EIO as u32))?;
                        Ok(DirEntry::new(
                            Path::new("/").join(relative),
                            entry.metadata().clone(),
                        ))
                    })
                    .collect()
            }
        }
    }
}

fn path_mount_id(path: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            0,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    };
    if result != 0 || stat.stx_mask & libc::STATX_MNT_ID == 0 {
        return None;
    }
    Some(stat.stx_mnt_id)
}

fn indexed(error: VfError, start: usize) -> VfError {
    match error.index_opt() {
        Some(index) => error.with_index(start + index),
        None => error,
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

    pub fn read_request_at(&self, offset: u64, length: usize) -> AutoRead<'_> {
        AutoRead {
            file: self,
            offset,
            length,
        }
    }
    pub fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> AutoWrite<'a> {
        AutoWrite {
            file: self,
            offset,
            data,
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
/// Positional write request for [`AutoClient::writev`].
pub struct AutoWrite<'a> {
    file: &'a AutoFile,
    offset: u64,
    data: &'a [u8],
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

fn decode_mount_field(field: &[u8]) -> Option<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\' {
            if index + 3 >= field.len() {
                return None;
            }
            let digits = &field[index + 1..index + 4];
            if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                return None;
            }
            let value = u16::from(digits[0] - b'0') * 64
                + u16::from(digits[1] - b'0') * 8
                + u16::from(digits[2] - b'0');
            decoded.push(u8::try_from(value).ok()?);
            index += 4;
        } else {
            decoded.push(field[index]);
            index += 1;
        }
    }
    Some(std::ffi::OsString::from_vec(decoded).into())
}

fn parse_mount(line: &[u8]) -> Option<MountSpec> {
    let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
    let separator = fields.iter().position(|field| *field == b"-")?;
    if separator < 6 || fields.len() < separator + 4 {
        return None;
    }
    if fields[separator + 1] != b"nfs" && fields[separator + 1] != b"nfs4" {
        return None;
    }
    if decode_mount_field(fields[3])? != Path::new("/") {
        return None;
    }
    if !fields[5]
        .split(|byte| *byte == b',')
        .any(|option| option == b"rw")
    {
        return None;
    }
    let options = fields[separator + 3];
    let options: Vec<_> = options.split(|byte| *byte == b',').collect();
    let security: Vec<_> = options
        .iter()
        .filter(|option| option.starts_with(b"sec="))
        .collect();
    let protocols: Vec<_> = options
        .iter()
        .filter(|option| option.starts_with(b"proto="))
        .collect();
    let versions: Vec<_> = options
        .iter()
        .filter(|option| option.starts_with(b"vers="))
        .collect();
    if security.len() != 1
        || *security[0] != b"sec=sys"
        || protocols.len() != 1
        || *protocols[0] != b"proto=tcp"
        || versions.len() != 1
    {
        return None;
    }
    let minor = if options.contains(&b"vers=4.1".as_slice()) {
        1
    } else if options.contains(&b"vers=4.2".as_slice()) {
        2
    } else {
        return None;
    };
    let source = std::str::from_utf8(fields[separator + 2]).ok()?;
    let (host, export) = source.rsplit_once(':')?;
    if host.is_empty() || !export.starts_with('/') {
        return None;
    }
    // The source hostname is not necessarily the endpoint held by the
    // kernel: DNS can change or return another server. Use the pinned addr
    // reported for this mount, and decline direct routing if it is ambiguous.
    let addresses: Vec<_> = options
        .iter()
        .filter_map(|option| option.strip_prefix(b"addr="))
        .collect();
    let [address] = addresses.as_slice() else {
        return None;
    };
    let address = std::str::from_utf8(address).ok()?;
    let address: IpAddr = address.trim_matches(['[', ']']).parse().ok()?;
    let port = match options
        .iter()
        .find_map(|option| option.strip_prefix(b"port="))
    {
        Some(value) => std::str::from_utf8(value).ok()?.parse::<u16>().ok()?,
        None => 2049,
    };
    if port == 0 {
        return None;
    }
    Some(MountSpec {
        id: std::str::from_utf8(fields[0]).ok()?.parse().ok()?,
        mount_point: decode_mount_field(fields[4])?,
        export: decode_mount_field(export.as_bytes())?,
        server: SocketAddr::new(address, port).to_string(),
        minor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
                files[0].write_request_at(0, b"one"),
                files[1].write_request_at(0, b"two"),
            ])
            .unwrap();
        let reads = client
            .readv(&[
                files[0].read_request_at(0, 3),
                files[1].read_request_at(0, 3),
            ])
            .unwrap();
        assert_eq!(reads[0].data, b"one");
        assert_eq!(reads[1].data, b"two");
        std::os::unix::fs::symlink("one", root.join("link")).unwrap();
        let metadata = client
            .mounted
            .symlink_metadatav(&[Path::new("/one"), Path::new("/link")])
            .unwrap();
        assert_eq!(metadata[0].file_type(), crate::VfType::Regular);
        assert_eq!(metadata[1].file_type(), crate::VfType::Symlink);
        let error = client
            .mounted
            .symlink_metadatav(&[Path::new("/one"), Path::new("/missing")])
            .unwrap_err();
        assert_eq!(error.index_opt(), Some(1));
        let tiny = Auto::new(&root).unwrap().with_readv_limit(5);
        let tiny_file = tiny.open("/one").unwrap();
        let error = tiny.readv(&[tiny_file.read_request_at(0, 6)]).unwrap_err();
        assert_eq!(error.err_no(), libc::EFBIG as u32);
        assert_eq!(
            tiny.readv(&[tiny_file.read_request_at(0, 3)]).unwrap()[0].data,
            b"one"
        );
        tiny_file.close().unwrap();
        let error = client
            .readv(&[files[0].read_request_at(0, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES + 1)])
            .unwrap_err();
        assert_eq!(error.err_no(), libc::EFBIG as u32);
        client.closev(files).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_nfs_mount_uses_direct_connection() {
        let Ok(mount) = std::env::var("VFSI_AUTO_TEST_MOUNT") else {
            return;
        };
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
                files[0].write_request_at(0, b"first"),
                files[1].write_request_at(0, b"second"),
                files[2].write_request_at(0, b"local"),
            ])
            .unwrap();
        let reads = client
            .readv(&[
                files[0].read_request_at(0, 5),
                files[1].read_request_at(0, 6),
                files[2].read_request_at(0, 5),
            ])
            .unwrap();
        assert_eq!(reads[0].data, b"first");
        assert_eq!(reads[1].data, b"second");
        assert_eq!(reads[2].data, b"local");
        client.closev(files).unwrap();
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
        assert_eq!(error.index_opt(), Some(2));
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
    fn live_file_bind_mount_uses_covering_mount() {
        let Ok(path) = std::env::var("VFSI_AUTO_TEST_BIND") else {
            return;
        };
        let client = Auto::new("/").unwrap();
        let mut file = client.open(&path).unwrap();
        assert_eq!(file.route(), AutoRoute::Mounted);
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "bind source\n");
        file.close().unwrap();
    }
}
