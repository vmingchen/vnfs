//! Mount eligibility, connection identities, and path routing.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MountSpec {
    pub(super) id: u64,
    pub(super) mount_point: PathBuf,
    pub(super) export: PathBuf,
    pub(super) server: String,
    pub(super) minor: u32,
}

#[derive(Default)]
pub(super) struct MountTable {
    pub(super) eligible: Vec<MountSpec>,
    pub(super) mount_points: HashSet<PathBuf>,
}

#[derive(Clone)]
pub(super) struct NfsConnection {
    pub(super) spec: MountSpec,
    pub(super) client: NfsClient,
    pub(super) identity: Arc<()>,
    pub(super) credentials: AuthSysIdentity,
}

use vfsi_nfs::mount::{AuthSysIdentity, decode_mount_field, path_mount_id};

#[derive(Clone)]
pub(super) enum Route {
    Mounted,
    Nfs(NfsConnection),
}

impl Route {
    pub(super) fn same_backend(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Mounted, Self::Mounted) => true,
            (Self::Nfs(a), Self::Nfs(b)) => Arc::ptr_eq(&a.identity, &b.identity),
            _ => false,
        }
    }

    pub(super) fn public(&self) -> AutoRoute {
        match self {
            Self::Mounted => AutoRoute::Mounted,
            Self::Nfs(connection) => AutoRoute::DirectNfs {
                mount_point: connection.spec.mount_point.clone(),
                server: connection.spec.server.clone(),
            },
        }
    }
}

impl Auto {
    pub(super) fn public_path(&self, connection: &NfsConnection, path: &Path) -> VfResult<PathBuf> {
        let suffix = path
            .strip_prefix("/")
            .map_err(|_| VfError::client(0, libc::EIO as u32))?;
        let host = connection.spec.mount_point.join(suffix);
        let relative = host
            .strip_prefix(&self.root)
            .map_err(|_| VfError::client(0, libc::EIO as u32))?;
        Ok(Path::new("/").join(relative))
    }

    /// Recursive operations retain kernel routing if another mount appears
    /// underneath their root, rather than bypassing that mounted subtree.
    pub(super) fn resolve_tree(&self, path: &Path) -> Resolved {
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

    /// Inspect the currently eligible path route. An individual operation
    /// can still choose Mounted (for example, when its target is a symlink).
    /// Inspect an opened [`AutoFile`] to see the route actually used.
    pub fn route_for(&self, path: impl AsRef<Path>) -> AutoRoute {
        let mounts = read_mounts(false);
        self.resolve(path.as_ref(), &mounts).route.public()
    }

    pub(super) fn host_path(&self, path: &Path) -> Option<PathBuf> {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return None;
        }
        let relative = path.strip_prefix("/").unwrap_or(path);
        Some(self.root.join(relative))
    }

    pub(super) fn resolve(&self, path: &Path, mounts: &MountTable) -> Resolved {
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

    pub(super) fn connection(&self, spec: &MountSpec) -> Option<NfsConnection> {
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
        if client.attrs("/").ok()?.file_id() != Some(kernel_id) {
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

    pub(super) fn resolve_open_batch(
        &self,
        requests: &[OpenOp],
        mounts: &MountTable,
    ) -> Vec<Resolved> {
        let mut parents: HashMap<PathBuf, Option<NfsConnection>> = HashMap::new();
        let mut resolved: Vec<_> = requests
            .iter()
            .map(|request| {
                let fallback = || Resolved {
                    route: Route::Mounted,
                    path: request.path().to_path_buf(),
                };
                let Some(host) = self.host_path(request.path()) else {
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
                    .filter(|index| !requests[*index].flags().contains(OpenFlags::CREATE_NEW))
                    .collect();
                if !checked.is_empty() {
                    let paths: Vec<_> = checked
                        .iter()
                        .map(|index| resolved[*index].path.as_path())
                        .collect();
                    match connection.client.vgetattrs_native(
                        &paths,
                        vfsi_core::AttrMask::MODE,
                        false,
                    ) {
                        Ok(attrs) if attrs.len() == checked.len() => {
                            for (index, item) in checked.into_iter().zip(attrs) {
                                if item.file_type() == crate::FileType::Symlink {
                                    resolved[index] = Resolved {
                                        route: Route::Mounted,
                                        path: requests[index].path().to_path_buf(),
                                    };
                                }
                            }
                        }
                        _ => {
                            for index in start..end {
                                resolved[index] = Resolved {
                                    route: Route::Mounted,
                                    path: requests[index].path().to_path_buf(),
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
}

pub(super) fn read_mounts(include_covering_mounts: bool) -> MountTable {
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

pub(super) fn mount_point(line: &[u8]) -> Option<PathBuf> {
    decode_mount_field(line.split(|byte| *byte == b' ').nth(4)?)
}

pub(super) fn parse_mount(line: &[u8]) -> Option<MountSpec> {
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
