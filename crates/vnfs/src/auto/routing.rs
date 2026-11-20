//! Mount eligibility, connection identities, and path routing.

use super::*;
use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use vfsi_core::FsObjId;
use vfsi_core::internal::{ObservedObject, errors};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MountSpec {
    pub(super) id: u64,
    pub(super) mount_point: PathBuf,
    pub(super) export: PathBuf,
    pub(super) server: Box<str>,
    pub(super) minor: u32,
}

#[derive(Default)]
pub(super) struct MountTable {
    pub(super) eligible: Vec<MountSpec>,
    pub(super) mount_points: HashSet<PathBuf>,
    pub(super) nfs_mount_ids: HashSet<u64>,
}

pub(super) type NfsConnection = Arc<NfsConnectionState>;

#[derive(Clone)]
pub(super) struct NfsConnectionState {
    pub(super) spec: MountSpec,
    pub(super) client: NfsClient,
    pub(super) identity: Arc<()>,
    pub(super) credentials: AuthSysIdentity,
}

use vfsi_nfs::mount::{AuthSysIdentity, decode_mount_field, path_mount_id};

#[derive(Clone)]
pub(super) enum Route {
    Posix,
    Nfs(NfsConnection),
}

impl Route {
    pub(super) fn same_backend(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Posix, Self::Posix) => true,
            (Self::Nfs(a), Self::Nfs(b)) => Arc::ptr_eq(&a.identity, &b.identity),
            _ => false,
        }
    }

    fn shares_nfs_identity(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Nfs(a), Self::Nfs(b))
                if a.spec.server == b.spec.server && a.credentials == b.credentials
        )
    }

    pub(super) fn public(&self) -> AutoRoute {
        match self {
            Self::Posix => AutoRoute::Posix,
            Self::Nfs(connection) => AutoRoute::DirectNfs {
                mount_point: connection.spec.mount_point.clone(),
                server: connection.spec.server.to_string(),
            },
        }
    }
}

impl Resolved {
    pub(super) fn posix(path: &Path) -> Self {
        Self {
            route: Route::Posix,
            path: path.to_path_buf(),
        }
    }
}

impl Auto {
    pub(crate) fn same_object_namespace_impl(&self, left: &Path, right: &Path) -> bool {
        let mounts = read_mounts(false);
        match (
            self.resolve(left, &mounts).route,
            self.resolve(right, &mounts).route,
        ) {
            (Route::Posix, Route::Posix) => true,
            (a, b) => a.shares_nfs_identity(&b),
        }
    }

    pub(crate) fn compare_routed_objects_impl(
        &self,
        pairs: &[(ObservedObject<'_>, ObservedObject<'_>)],
    ) -> Vec<VfResult<Option<bool>>> {
        if pairs.is_empty() {
            return Vec::new();
        }
        let mounts = match read_mounts_for_identity() {
            Ok(mounts) => mounts,
            Err(error) => {
                return vfsi_core::internal::errors::fanout(error, pairs.len())
                    .map(Err)
                    .collect();
            }
        };
        let mut routes = HashMap::new();
        for &(left, right) in pairs {
            for path in [left.0, right.0] {
                routes
                    .entry(path)
                    .or_insert_with(|| self.resolve(path, &mounts));
            }
        }
        let direct = Self::direct_identities(pairs, &routes);
        let mut mounted = HashMap::new();
        pairs
            .iter()
            .map(|&(left, right)| {
                if routes[left.0]
                    .route
                    .shares_nfs_identity(&routes[right.0].route)
                {
                    let id =
                        |path: &Path| direct[path].as_ref().copied().map_err(errors::from_shared);
                    Ok(Some(id(left.0)? == id(right.0)?))
                } else {
                    self.compare_objects_in_mounts(left, right, &routes, &mounts, &mut mounted)
                        .map(Some)
                }
            })
            .collect()
    }

    fn direct_identities<'a>(
        pairs: &[(ObservedObject<'a>, ObservedObject<'a>)],
        routes: &HashMap<&Path, Resolved>,
    ) -> HashMap<&'a Path, Result<FsObjId, Arc<io::Error>>> {
        // Observe fresh IDs in known views. Earlier metadata could have
        // used kernel fallback. Keep one vector query per direct connection.
        let mut groups = HashMap::<_, Vec<&Path>>::new();
        let mut seen = HashSet::new();
        for &(left, right) in pairs {
            if !routes[left.0]
                .route
                .shares_nfs_identity(&routes[right.0].route)
            {
                continue;
            }
            for path in [left.0, right.0] {
                if seen.insert(path) {
                    let Route::Nfs(connection) = &routes[path].route else {
                        unreachable!()
                    };
                    groups
                        .entry(Arc::as_ptr(&connection.identity))
                        .or_default()
                        .push(path);
                }
            }
        }
        let mut observed = HashMap::with_capacity(seen.len());
        for paths in groups.into_values() {
            let Route::Nfs(connection) = &routes[paths[0]].route else {
                unreachable!()
            };
            let native: Vec<_> = paths
                .iter()
                .map(|path| routes[path].path.as_path())
                .collect();
            let options = crate::AttrsOptions::new()
                .with_fields(crate::Attributes::identity())
                .with_follow_final_symlink(false);
            let error = match connection.client.vgetattrs(&native, options) {
                Ok(results) if results.len() == paths.len() => {
                    observed.extend(paths.into_iter().zip(results.into_iter().map(|result| {
                        result
                            .map_err(vfsi_core::OpError::into_cause)
                            .and_then(|attrs| object_identity(&attrs))
                            .map_err(errors::shared_source)
                    })));
                    continue;
                }
                Ok(_) => errors::invalid_reply("invalid routed identity outcome count"),
                Err(error) => error,
            };
            let shared = errors::shared_source(error);
            observed.extend(paths.into_iter().map(|path| (path, Err(shared.clone()))));
        }
        observed
    }

    fn compare_objects_in_mounts<'a>(
        &self,
        (left, left_attrs): ObservedObject<'a>,
        (right, right_attrs): ObservedObject<'a>,
        routes: &HashMap<&Path, Resolved>,
        mounts: &MountTable,
        identities: &mut HashMap<&'a Path, FsObjId>,
    ) -> VfResult<bool> {
        let left_route = &routes[left];
        let right_route = &routes[right];
        let mut observe = |path: &'a Path, attrs: &crate::Attrs| {
            if let Some(id) = identities.get(path) {
                return Ok(*id);
            }
            let current = if attrs.is_symlink() {
                self.mounted.symlink_attrs(path)?
            } else {
                self.mounted.attrs(path)?
            };
            let id = object_identity(&current)?;
            identities.insert(path, id);
            Ok::<_, io::Error>(id)
        };
        // A direct NFS object and an entry on a non-NFS mount are distinct.
        // Prove the mounted side's type before asking the kernel about a
        // direct-created name that its cache may still report as absent.
        let mounted = match (&left_route.route, &right_route.route) {
            (Route::Posix, Route::Nfs(_)) => Some(left),
            (Route::Nfs(_), Route::Posix) => Some(right),
            _ => None,
        };
        if let Some(path) = mounted
            && let Some(id) = self.rooted_copy_mount_id(path, true)
            && !mounts.nfs_mount_ids.contains(&id)
        {
            return Ok(false);
        }
        let left_id = observe(left, left_attrs)?;
        let right_id = observe(right, right_attrs)?;
        if left_id.fs_id() == right_id.fs_id() {
            return Ok(left_id == right_id);
        }
        if left_attrs.is_symlink() || right_attrs.is_symlink() {
            return Ok(false);
        }
        if mounts.nfs_mount_ids.len() < 2 {
            return Ok(false);
        }
        use vfsi_local::open_rooted_path;
        let root = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(&self.root)?;
        let entries = [left, right].map(|path| {
            let file = open_rooted_path(&root, path, false)?;
            let fd = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            let id =
                path_mount_id(&fd).ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
            Ok::<_, io::Error>((id, fs::read_link(fd)?))
        });
        let [left_entry, right_entry] = entries;
        let (left_mount_id, left_path) = left_entry?;
        let (right_mount_id, right_path) = right_entry?;
        if !mounts.nfs_mount_ids.contains(&left_mount_id)
            || !mounts.nfs_mount_ids.contains(&right_mount_id)
        {
            return Ok(false);
        }
        let compatible = mounts
            .eligible
            .iter()
            .find(|mount| mount.id == left_mount_id)
            .zip(
                mounts
                    .eligible
                    .iter()
                    .find(|mount| mount.id == right_mount_id),
            )
            .filter(|(left, right)| left.server == right.server);
        let Some((left_mount, right_mount)) = compatible else {
            // Kernel inode numbers agree, but these mounted views cannot
            // share a native identity comparison. Do not remove either name.
            if left_id.file_id() == right_id.file_id() {
                return Err(io::ErrorKind::Unsupported.into());
            }
            return Ok(false);
        };
        let native_id = |mount: &MountSpec, path: &Path| -> VfResult<_> {
            let path = path
                .strip_prefix(&mount.mount_point)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let connection = self
                .connection(mount)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
            object_identity(&connection.client.symlink_attrs(Path::new("/").join(path))?)
        };
        Ok(native_id(left_mount, &left_path)? == native_id(right_mount, &right_path)?)
    }

    pub(crate) fn supports_regular_path_copies_impl(&self, pairs: &[(&Path, &Path)]) -> Vec<bool> {
        if pairs.is_empty() {
            return Vec::new();
        }
        let mounts = read_mounts(false);
        pairs
            .iter()
            .map(|&(from, to)| {
                let source = self.resolve(from, &mounts);
                let destination = self.resolve(to, &mounts);
                source.route.same_backend(&destination.route)
                    || self.mounted_regular_copy_pair(
                        from,
                        to,
                        &mounts,
                        &source.route,
                        &destination.route,
                    )
            })
            .collect()
    }

    pub(super) fn mounted_regular_copy_pair(
        &self,
        from: &Path,
        to: &Path,
        mounts: &MountTable,
        source: &Route,
        destination: &Route,
    ) -> bool {
        match (source, destination) {
            (other @ Route::Nfs(_), Route::Posix) => {
                self.mounted_copy_path_on(to, true, mounts, other)
            }
            (Route::Posix, other @ Route::Nfs(_)) => {
                self.mounted_copy_path_on(from, true, mounts, other)
            }
            _ => false,
        }
    }

    /// Recursive operations retain kernel routing if another mount appears
    /// underneath their root, rather than bypassing that mounted subtree.
    pub(super) fn resolve_tree(&self, path: &Path, mounts: &MountTable) -> Resolved {
        if mounts.eligible.is_empty()
            || self.host_path(path).is_some_and(|host| {
                mounts
                    .mount_points
                    .iter()
                    .any(|point| point != &host && point.starts_with(&host))
            })
        {
            return Resolved::posix(path);
        }
        self.resolve(path, mounts)
    }

    /// Inspect the currently eligible path route. An individual operation
    /// can still choose Posix (for example, when its target is a symlink).
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
        let mounted = || Resolved::posix(path);
        if mounts.eligible.is_empty() {
            return mounted();
        }
        let Some(host_path) = self.host_path(path) else {
            return mounted();
        };
        // Do not bypass kernel symlink resolution or map a path outside root.
        // For creation, inspect the existing parent instead of the missing file.
        let (canonical, parent) = match fs::canonicalize(&host_path) {
            Ok(actual) => (actual, None),
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
                // Canonicalization and lstat on a spelling ending in a
                // separator can both follow a dangling link. Inspect the
                // final entry directly before treating it as missing.
                let candidate = parent.join(name);
                if !fs::symlink_metadata(&candidate)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                {
                    return mounted();
                }
                (candidate, Some(parent))
            }
            Err(_) => return mounted(),
        };
        if canonical != host_path || !canonical.starts_with(&self.root) {
            return mounted();
        }
        let Some(mount_id) = path_mount_id(parent.as_deref().unwrap_or(&canonical)) else {
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

    /// Link and rename address the final directory entry, not its symlink
    /// target. Keep ordinary direct routes fast and preserve the kernel policy
    /// for symlink ancestors, mountpoints, and special terminal operands.
    pub(super) fn resolve_entry(&self, path: &Path, mounts: &MountTable) -> Resolved {
        let resolved = self.resolve(path, mounts);
        if mounts.eligible.is_empty() || matches!(&resolved.route, Route::Nfs(_)) {
            return resolved;
        }
        let Some(host) = self.host_path(path) else {
            return resolved;
        };
        if !mounts
            .eligible
            .iter()
            .any(|mount| host.starts_with(&mount.mount_point))
            || !fs::symlink_metadata(&host).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return resolved;
        }
        let Some(name) = path.file_name() else {
            return resolved;
        };
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("/"));
        let mut parent = self.resolve(parent, mounts);
        if matches!(&parent.route, Route::Nfs(_)) {
            parent.path.push(name);
            parent
        } else {
            resolved
        }
    }

    pub(super) fn mounted_copy_path_on(
        &self,
        path: &Path,
        follow_leaf: bool,
        mounts: &MountTable,
        other: &Route,
    ) -> bool {
        self.rooted_copy_mount_id(path, follow_leaf)
            .and_then(|id| mounts.eligible.iter().find(|mount| mount.id == id))
            .and_then(|mount| self.connection(mount))
            .is_some_and(|connection| Route::Nfs(connection).same_backend(other))
    }

    fn rooted_copy_mount_id(&self, path: &Path, follow_leaf: bool) -> Option<u64> {
        use vfsi_core::path::{normalize_bytes, path_bytes, path_from_bytes};
        use vfsi_local::open_rooted_path;

        let rooted = |path: &Path| path_from_bytes(&normalize_bytes(path_bytes(path)));
        let root = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(&self.root)
            .ok()?;
        let mut path = rooted(path);
        for _ in 0..=40 {
            // Resolve ancestors with the same rooted kernel lookup as Local.
            // For a final link, mirror Local's absolute and relative rebasing
            // so a dangling destination can still use its real parent mount.
            let parent = path.parent().unwrap_or(Path::new(""));
            let directory = open_rooted_path(&root, parent, true).ok()?;
            let anchor = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
            let entry = anchor.join(path.file_name().unwrap_or(OsStr::new(".")));
            if fs::symlink_metadata(&entry).is_ok_and(|attrs| attrs.file_type().is_symlink()) {
                if !follow_leaf {
                    return path_mount_id(&anchor);
                }
                let target = fs::read_link(entry).ok()?;
                path = if target.is_absolute() {
                    rooted(&target)
                } else {
                    parent.join(target)
                };
                continue;
            }
            return match open_rooted_path(&root, &path, false) {
                Ok(actual) => {
                    path_mount_id(Path::new(&format!("/proc/self/fd/{}", actual.as_raw_fd())))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => path_mount_id(&anchor),
                Err(_) => None,
            };
        }
        None
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
        // Mount aliases to the same endpoint/root share one filesystem client.
        // Route metadata retains each mount point solely for path translation.
        let alias = self
            .connections
            .lock()
            .ok()?
            .values()
            .find(|existing| {
                existing.spec.server == spec.server
                    && existing.spec.export == spec.export
                    && existing.spec.minor == spec.minor
                    && existing.credentials == credentials
            })
            .cloned();
        // Connection failure is a pre-dispatch fallback. Never replay a
        // possibly completed mutation through the mounted backend.
        let client = if let Some(existing) = &alias {
            existing.client.clone()
        } else {
            Nfs::builder(spec.server.as_ref())
                .root(&spec.export)
                .version(crate::NfsVersion::try_from(Some(spec.minor)).ok()?)
                .connect()
                .ok()?
                .with_limits(self.limits)
                .inner
        };
        let kernel_id = fs::metadata(&spec.mount_point).ok()?.ino();
        if client.attrs("/").ok()?.file_id() != Some(kernel_id) {
            return None;
        }
        if AuthSysIdentity::current().as_ref() != Some(&credentials) {
            return None;
        }
        let connection = Arc::new(NfsConnectionState {
            spec: spec.clone(),
            client,
            identity: alias
                .as_ref()
                .map_or_else(|| Arc::new(()), |existing| existing.identity.clone()),
            credentials,
        });
        let mut cache = self.connections.lock().ok()?;
        if let Some(existing) = cache.get(&spec.id)
            && existing.spec == *spec
            && existing.credentials == connection.credentials
        {
            return Some(existing.clone());
        }
        // Resolve simultaneous discoveries of different aliases to one client
        // before publishing either route to callers.
        let connection = if let Some(existing) = cache.values().find(|existing| {
            existing.spec.server == spec.server
                && existing.spec.export == spec.export
                && existing.spec.minor == spec.minor
                && existing.credentials == connection.credentials
        }) {
            Arc::new(NfsConnectionState {
                spec: spec.clone(),
                client: existing.client.clone(),
                identity: existing.identity.clone(),
                credentials: connection.credentials.clone(),
            })
        } else {
            connection
        };
        let retired = cache.insert(spec.id, connection.clone());
        drop(cache);
        // Session teardown can perform RPCs. Release the route cache first.
        drop(retired);
        Some(connection)
    }

    pub(super) fn resolve_open_batch(
        &self,
        requests: &[OpenOp],
        mounts: &MountTable,
    ) -> Vec<Resolved> {
        if mounts.eligible.is_empty() {
            return requests
                .iter()
                .map(|request| Resolved::posix(request.path()))
                .collect();
        }
        let mut parents: HashMap<PathBuf, Option<NfsConnection>> = HashMap::new();
        let mut resolved: Vec<_> = requests
            .iter()
            .map(|request| {
                let fallback = || Resolved::posix(request.path());
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
                                if match item {
                                    Ok(item) => item.file_type() == crate::FileType::Symlink,
                                    Err(_) => true,
                                } {
                                    resolved[index] = Resolved {
                                        route: Route::Posix,
                                        path: requests[index].path().to_path_buf(),
                                    };
                                }
                            }
                        }
                        _ => {
                            for index in start..end {
                                resolved[index] = Resolved {
                                    route: Route::Posix,
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

fn object_identity(attrs: &crate::Attrs) -> VfResult<FsObjId> {
    attrs
        .fs_obj_id()
        .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))
}

pub(super) fn read_mounts(include_covering_mounts: bool) -> MountTable {
    read_mount_table(include_covering_mounts, false).unwrap_or_default()
}

fn read_mounts_for_identity() -> VfResult<MountTable> {
    read_mount_table(false, true)
}

fn read_mount_table(covering_mounts: bool, nfs_mounts: bool) -> VfResult<MountTable> {
    let contents = fs::read("/proc/self/mountinfo")?;
    let mut table = MountTable::default();
    for line in contents.split(|byte| *byte == b'\n') {
        if covering_mounts && let Some(point) = mount_point(line) {
            table.mount_points.insert(point);
        }
        if nfs_mounts && let Some(id) = nfs_mount_id(line) {
            table.nfs_mount_ids.insert(id);
        }
        if let Some(spec) = parse_mount(line) {
            table.eligible.push(spec);
        }
    }
    Ok(table)
}

fn nfs_mount_id(line: &[u8]) -> Option<u64> {
    let mut fields = line.split(|byte| *byte == b' ');
    let id = fields.next()?;
    let filesystem = fields.skip(5).skip_while(|field| *field != b"-").nth(1)?;
    if matches!(filesystem, b"nfs" | b"nfs4") {
        return std::str::from_utf8(id).ok()?.parse().ok();
    }
    None
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
        server: info.server.into_boxed_str(),
        minor: info.minor,
    })
}

#[cfg(test)]
#[test]
fn rooted_copy_probe_works_from_a_search_only_root() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("real")).unwrap();
    symlink("real", root.path().join("alias")).unwrap();
    let client = Auto::new(root.path()).unwrap();
    let mount = path_mount_id(root.path()).unwrap();
    let permissions = fs::metadata(root.path()).unwrap().permissions();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o111)).unwrap();
    let observed = client.rooted_copy_mount_id(Path::new("alias/missing"), true);
    fs::set_permissions(root.path(), permissions).unwrap();
    assert_eq!(observed, Some(mount));
}
