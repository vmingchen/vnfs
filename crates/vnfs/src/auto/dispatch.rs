//! Vector dispatch across ordered route cohorts.

use super::*;

impl Auto {
    pub(crate) fn limits_impl(&self) -> ResourceLimits {
        self.limits
    }
    /// Drain Drop cleanup on mounted and already-connected NFS backends.
    /// No new connections are created; failed targets retain cleanup ownership.
    pub fn drain_cleanup(&self) -> VfResult<()> {
        let clients: Vec<_> = self
            .connections
            .lock()
            .map_err(|_| VfError::client(0, libc::EIO as u32))?
            .values()
            .map(|connection| connection.client.clone())
            .collect();
        let mut result = self.mounted.drain_cleanup();
        for client in clients {
            if let Err(error) = client.drain_cleanup()
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }

    pub(crate) fn vopen_dirs_impl<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<AutoDir>> {
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| self.resolve_tree(path.as_ref()))
            .collect();
        let mut output = Vec::with_capacity(paths.len());
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(&resolved, start);
            let batch: Vec<_> = resolved[start..end].iter().map(|r| &r.path).collect();
            let dirs: Vec<_> = match &resolved[start].route {
                Route::Mounted => self
                    .mounted
                    .vopen_dirs(&batch)
                    .map(|dirs| dirs.into_iter().map(AutoDirInner::Mounted).collect()),
                Route::Nfs(connection) => connection
                    .client
                    .vopen_dirs(&batch)
                    .map(|dirs| dirs.into_iter().map(AutoDirInner::Nfs).collect()),
            }
            .map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| paths.get(index)) {
                    Some(path) => error.with_context("vopen_dirs", path.as_ref()),
                    None => error,
                }
            })?;
            for (index, inner) in (start..end).zip(dirs) {
                output.push(AutoDir {
                    path: paths[index].as_ref().to_path_buf(),
                    route: resolved[index].route.clone(),
                    inner,
                    owner: Arc::clone(&self.owner),
                });
            }
            start = end;
        }
        Ok(output)
    }

    pub(crate) fn vremove_dir_contents_impl(
        &self,
        dirs: &[&AutoDir],
        options: crate::RemoveOptions,
    ) -> VfResult<()> {
        let mut resolved = Vec::with_capacity(dirs.len());
        for (index, dir) in dirs.iter().enumerate() {
            if !Arc::ptr_eq(&self.owner, &dir.owner) || dir.is_closed() {
                return Err(VfError::client(index, libc::EBADF as u32)
                    .with_context("vremove_dir_contents", &dir.path));
            }
            if let Route::Nfs(connection) = &dir.route
                && AuthSysIdentity::current().as_ref() != Some(&connection.credentials)
            {
                return Err(VfError::client(index, libc::EACCES as u32)
                    .with_context("auto_auth", &dir.path));
            }
            resolved.push(Resolved {
                route: dir.route.clone(),
                path: dir.path.clone(),
            });
        }
        let mut first_error = None;
        let mut start = 0;
        while start < dirs.len() {
            let end = cohort_end(&resolved, start);
            macro_rules! batch {
                ($variant:ident) => {
                    dirs[start..end]
                        .iter()
                        .map(|dir| match &dir.inner {
                            AutoDirInner::$variant(inner) => inner,
                            _ => unreachable!("validated route"),
                        })
                        .collect::<Vec<_>>()
                };
            }
            let result = match &resolved[start].route {
                Route::Mounted => self.mounted.vremove_dir_contents(&batch!(Mounted), options),
                Route::Nfs(connection) => connection
                    .client
                    .vremove_dir_contents(&batch!(Nfs), options),
            }
            .map_err(|e| {
                let error = indexed(e, start);
                match error.index().and_then(|i| dirs.get(i)) {
                    Some(dir) => error.with_context("vremove_dir_contents", &dir.path),
                    None => error,
                }
            });
            if let Err(error) = result {
                if error.is_transport() || !options.continues_on_error() {
                    return Err(error);
                }
                first_error.get_or_insert(error);
            }
            start = end;
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Common routed capabilities. Server-copy acceleration is route-specific
    /// and is not advertised as a guarantee for the whole namespace.
    pub(crate) fn capabilities_impl(&self) -> VfResult<crate::Capabilities> {
        self.mounted.capabilities()
    }

    /// Keep target text unchanged and let the kernel interpret it in the
    /// mounted namespace, matching the scalar symlink operation.
    pub(crate) fn vsymlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> VfResult<()> {
        self.mounted.vsymlink(pairs)
    }

    /// Read link text through the mounted namespace without following links.
    pub(crate) fn vreadlink_impl<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<PathBuf>> {
        self.mounted.vreadlink(paths)
    }

    /// Route same-backend hard-link pairs together. Cross-route pairs use the
    /// kernel namespace, which reports cross-filesystem errors when appropriate.
    pub(crate) fn vhardlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mounts = read_mounts(false);
        let mut sources: Vec<_> = pairs
            .iter()
            .map(|(source, _)| self.resolve(source.as_ref(), &mounts))
            .collect();
        let mut links: Vec<_> = pairs
            .iter()
            .map(|(_, link)| self.resolve(link.as_ref(), &mounts))
            .collect();
        for (index, (source, link)) in sources.iter_mut().zip(&mut links).enumerate() {
            if !source.route.same_backend(&link.route) {
                *source = Resolved {
                    route: Route::Mounted,
                    path: pairs[index].0.as_ref().to_path_buf(),
                };
                *link = Resolved {
                    route: Route::Mounted,
                    path: pairs[index].1.as_ref().to_path_buf(),
                };
            }
        }
        let mut start = 0;
        while start < pairs.len() {
            let end = cohort_end(&sources, start);
            let batch: Vec<_> = (start..end)
                .map(|index| (sources[index].path.as_path(), links[index].path.as_path()))
                .collect();
            match &sources[start].route {
                Route::Mounted => self.mounted.vhardlink(&batch),
                Route::Nfs(connection) => connection.client.vhardlink(&batch),
            }
            .map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| pairs.get(index)) {
                    Some((_, link)) => error.with_context("vhardlink", link.as_ref()),
                    None => error,
                }
            })?;
            start = end;
        }
        Ok(())
    }

    /// Query route-coherent batches, preserving each open handle's retained route.
    pub(crate) fn vstatfs_impl<P: vfsi_core::AsTarget<AutoFile>>(
        &self,
        targets: &[P],
    ) -> VfResult<Vec<crate::FilesystemStats>> {
        use vfsi_core::Target;
        let targets: Vec<_> = targets.iter().map(vfsi_core::AsTarget::as_target).collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mounts = read_mounts(false);
        let mut resolved = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let route = match *target {
                Target::Path(path) => self.resolve(path, &mounts),
                Target::File(file) => {
                    self.check_owner(file, index)?;
                    if file.is_closed() {
                        return Err(VfError::client(index, libc::EBADF as u32)
                            .with_context("vstatfs", &file.path));
                    }
                    Resolved {
                        route: file.route.clone(),
                        path: file.path.clone(),
                    }
                }
            };
            resolved.push(route);
        }
        let mut output = Vec::with_capacity(targets.len());
        let mut start = 0;
        while start < targets.len() {
            let end = cohort_end(&resolved, start);
            macro_rules! batch {
                ($variant:ident) => {{
                    (start..end)
                        .map(|index| match targets[index] {
                            Target::Path(_) => Target::Path(resolved[index].path.as_path()),
                            Target::File(file) => match &file.inner {
                                AutoFileInner::$variant(inner) => Target::File(inner),
                                _ => unreachable!("validated route"),
                            },
                        })
                        .collect::<Vec<_>>()
                }};
            }
            let result = match &resolved[start].route {
                Route::Mounted => self.mounted.vstatfs(&batch!(Mounted)),
                Route::Nfs(connection) => connection.client.vstatfs(&batch!(Nfs)),
            }
            .map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| targets.get(index)) {
                    Some(target) => {
                        let path = match *target {
                            Target::Path(path) => path,
                            Target::File(file) => file.path.as_path(),
                        };
                        error.with_context("vstatfs", path)
                    }
                    None => error,
                }
            })?;
            output.extend(result);
            start = end;
        }
        Ok(output)
    }
    /// Update route-coherent batches of paths and open objects.
    pub(crate) fn vsetattrs_impl<P: vfsi_core::AsTarget<AutoFile>>(
        &self,
        updates: &[crate::SetAttrsOp<P>],
    ) -> VfResult<()> {
        use vfsi_core::Target;
        let updates: Vec<_> = updates
            .iter()
            .map(|op| op.with_target(op.target().as_target()))
            .collect();
        if updates.is_empty() {
            return Ok(());
        }
        let mounts = read_mounts(false);
        let mut resolved = Vec::with_capacity(updates.len());
        for (index, op) in updates.iter().enumerate() {
            let target = op.target();
            // Preflight all inputs before any route is allowed to mutate.
            let route = match *target {
                Target::Path(path) => self.resolve(path, &mounts),
                Target::File(file) => {
                    self.check_owner(file, index)?;
                    if file.is_closed() {
                        return Err(VfError::client(index, libc::EBADF as u32)
                            .with_context("vsetattrs", &file.path));
                    }
                    Resolved {
                        route: file.route.clone(),
                        path: file.path.clone(),
                    }
                }
            };
            if op.requested_uid() == Some(u32::MAX) || op.requested_gid() == Some(u32::MAX) {
                return Err(VfError::client(index, libc::EINVAL as u32)
                    .with_context("vsetattrs", &route.path));
            }
            for time in [op.requested_accessed(), op.requested_modified()]
                .into_iter()
                .flatten()
            {
                // NFS timestamps are signed seconds, matching the core engine.
                let duration = time
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_else(|e| e.duration());
                let seconds = duration.as_secs();
                if seconds > i64::MAX as u64
                    || (time < std::time::UNIX_EPOCH
                        && seconds == i64::MAX as u64
                        && duration.subsec_nanos() != 0)
                {
                    return Err(VfError::client(index, libc::EOVERFLOW as u32)
                        .with_context("vsetattrs", &route.path));
                }
            }
            resolved.push(route);
        }
        let mut start = 0;
        while start < updates.len() {
            let end = cohort_end(&resolved, start);
            macro_rules! batch {
                ($variant:ident) => {{
                    (start..end)
                        .map(|index| {
                            let target = match *updates[index].target() {
                                Target::Path(_) => Target::Path(resolved[index].path.as_path()),
                                Target::File(file) => match &file.inner {
                                    AutoFileInner::$variant(inner) => Target::File(inner),
                                    _ => unreachable!("validated route"),
                                },
                            };
                            updates[index].with_target(target)
                        })
                        .collect::<Vec<_>>()
                }};
            }
            match &resolved[start].route {
                Route::Mounted => self.mounted.vsetattrs(&batch!(Mounted)),
                Route::Nfs(connection) => connection.client.vsetattrs(&batch!(Nfs)),
            }
            .map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| updates.get(index)) {
                    Some(op) => {
                        let path = match *op.target() {
                            Target::Path(path) => path,
                            Target::File(file) => file.path.as_path(),
                        };
                        error.with_context("vsetattrs", path)
                    }
                    None => error,
                }
            })?;
            start = end;
        }
        Ok(())
    }

    pub(crate) fn vgetattrs_impl<P: vfsi_core::AsTarget<AutoFile>>(
        &self,
        targets: &[P],
        options: crate::AttrsOptions,
    ) -> VfResult<Vec<crate::Attrs>> {
        use vfsi_core::Target;
        let targets: Vec<_> = targets.iter().map(vfsi_core::AsTarget::as_target).collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mounts = read_mounts(false);
        let mut resolved = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let route = match *target {
                Target::Path(path) => self.resolve(path, &mounts),
                Target::File(file) => {
                    self.check_owner(file, index)?;
                    if file.is_closed() {
                        return Err(VfError::client(index, libc::EBADF as u32)
                            .with_context("vgetattrs", &file.path));
                    }
                    Resolved {
                        route: file.route.clone(),
                        path: file.path.clone(),
                    }
                }
            };
            resolved.push(route);
        }
        let mut output = Vec::with_capacity(targets.len());
        let mut start = 0;
        while start < targets.len() {
            let end = cohort_end(&resolved, start);
            macro_rules! batch {
                ($variant:ident) => {{
                    (start..end)
                        .map(|index| match targets[index] {
                            Target::Path(_) => Target::Path(resolved[index].path.as_path()),
                            Target::File(file) => match &file.inner {
                                AutoFileInner::$variant(inner) => Target::File(inner),
                                _ => unreachable!("validated route"),
                            },
                        })
                        .collect::<Vec<_>>()
                }};
            }
            let result = match &resolved[start].route {
                Route::Mounted => self.mounted.vgetattrs(&batch!(Mounted), options),
                Route::Nfs(connection) => connection.client.vgetattrs(&batch!(Nfs), options),
            }
            .map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| targets.get(index)) {
                    Some(target) => {
                        let path = match *target {
                            Target::Path(path) => path,
                            Target::File(file) => file.path.as_path(),
                        };
                        error.with_context("vgetattrs", path)
                    }
                    None => error,
                }
            })?;
            output.extend(result);
            start = end;
        }
        Ok(output)
    }
    pub(crate) fn vfsync_impl(&self, files: &[&AutoFile], mode: crate::SyncMode) -> VfResult<()> {
        let mut resolved = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            self.check_owner(file, index)?;
            if file.is_closed() {
                return Err(
                    VfError::client(index, libc::EBADF as u32).with_context("vfsync", &file.path)
                );
            }
            resolved.push(Resolved {
                route: file.route.clone(),
                path: file.path.clone(),
            });
        }
        let mut start = 0;
        while start < files.len() {
            let end = cohort_end(&resolved, start);
            macro_rules! batch {
                ($variant:ident) => {{
                    files[start..end]
                        .iter()
                        .map(|file| match &file.inner {
                            AutoFileInner::$variant(inner) => inner,
                            _ => unreachable!("validated route"),
                        })
                        .collect::<Vec<_>>()
                }};
            }
            match &resolved[start].route {
                Route::Mounted => self.mounted.vfsync(&batch!(Mounted), mode),
                Route::Nfs(connection) => connection.client.vfsync(&batch!(Nfs), mode),
            }
            .map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| files.get(index)) {
                    Some(file) => error.with_context("vfsync", &file.path),
                    None => error,
                }
            })?;
            start = end;
        }
        Ok(())
    }

    /// Create directories in bounded backend cohorts, retaining input order.
    /// Parents must exist; this does not promise transactional rollback.
    pub(crate) fn vmkdir_impl<P: AsRef<Path>>(&self, paths: &[crate::MkDirOp<P>]) -> VfResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut seen = std::collections::HashSet::with_capacity(paths.len());
        for (index, op) in paths.iter().enumerate() {
            let path = op.path();
            if !seen.insert(path) {
                return Err(
                    VfError::client(index, libc::EINVAL as u32).with_context("vmkdir", path)
                );
            }
        }
        let mounts = read_mounts(true);
        let resolved: Vec<_> = paths
            .iter()
            .map(|op| self.resolve(op.path(), &mounts))
            .collect();
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(&resolved, start);
            let batch: Vec<_> = resolved[start..end]
                .iter()
                .zip(&paths[start..end])
                .map(|(route, op)| crate::MkDirOp::new(route.path.as_path(), op.mode()))
                .collect();
            let result = match &resolved[start].route {
                Route::Mounted => self.mounted.vmkdir(&batch),
                Route::Nfs(connection) => connection.client.vmkdir(&batch),
            };
            result.map_err(|error| {
                let error = indexed(error, start);
                match error.index().and_then(|index| paths.get(index)) {
                    Some(op) => error.with_context("vmkdir", op.path()),
                    None => error,
                }
            })?;
            start = end;
        }
        Ok(())
    }

    pub(crate) fn vremove_impl<P: AsRef<Path>>(
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
                Route::Mounted => self.mounted.vremove_impl(&batch, recursive, options),
                Route::Nfs(connection) => {
                    connection.client.vremove_impl(&batch, recursive, options)
                }
            }
            .map_err(|error| indexed(error, start))?;
            start = end;
        }
        Ok(())
    }

    pub(crate) fn vcopy_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::CopyOption,
    ) -> VfResult<()> {
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
                Route::Mounted => self.mounted.vcopy(&batch, options),
                Route::Nfs(connection) => connection.client.vcopy(&batch, options),
            }
            .map_err(|error| indexed(error, start))?;
            start = end;
        }
        Ok(())
    }

    pub(crate) fn directory_page_batch_size(&self, paths: &[&Path]) -> VfResult<usize> {
        if paths.is_empty() {
            return Ok(1);
        }
        let mounts = read_mounts(false);
        let routes: Vec<_> = paths
            .iter()
            .map(|path| self.resolve(path, &mounts))
            .collect();
        match &routes[0].route {
            Route::Mounted => Ok(1),
            Route::Nfs(connection) => {
                Ok(cohort_end(&routes, 0).min(connection.client.directory_page_batch_size()?))
            }
        }
    }
    pub(crate) fn read_dir_pages_with_fields(
        &self,
        paths: &[&Path],
        fields: crate::Attributes,
        cursors: Vec<Option<vfsi_sync::DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
        follow_symlinks: bool,
    ) -> VfResult<Vec<vfsi_sync::DirectoryPage>> {
        if paths.len() != cursors.len() {
            return Err(VfError::client(0, libc::EINVAL as u32));
        }
        let mounts = if cursors.iter().any(|cursor| {
            cursor
                .as_ref()
                .is_none_or(|cursor| cursor.is::<RoutedChildDirectoryCursor>())
        }) {
            read_mounts(false)
        } else {
            MountTable::default()
        };
        let mut states = Vec::new();
        for (index, (path, cursor)) in paths.iter().zip(cursors).enumerate() {
            let state = match cursor {
                Some(cursor) => {
                    let child = cursor.is::<RoutedChildDirectoryCursor>();
                    let saved = if child {
                        cursor
                            .into_state::<RoutedChildDirectoryCursor>()
                            .map_err(|error| indexed(error, index))?
                            .0
                    } else {
                        cursor
                            .into_state::<RoutedDirectoryCursor>()
                            .map_err(|error| indexed(error, index))?
                    };
                    if !Arc::ptr_eq(&saved.owner, &self.owner) || saved.public_path != *path {
                        return Err(VfError::client(index, libc::EINVAL as u32));
                    }
                    if child {
                        // Recheck the mount/identity before descent, but retain
                        // anchored lookup when the child still uses this backend.
                        let resolved = self.resolve(path, &mounts);
                        if saved.route.same_backend(&resolved.route)
                            && saved.backend_path == resolved.path
                        {
                            (saved.route, saved.backend_path, Some(saved.cursor))
                        } else {
                            (resolved.route, resolved.path, None)
                        }
                    } else {
                        (saved.route, saved.backend_path, Some(saved.cursor))
                    }
                }
                None => {
                    let resolved = self.resolve(path, &mounts);
                    (resolved.route, resolved.path, None)
                }
            };
            states.push(state);
        }
        let mut output = Vec::new();
        let mut start = 0;
        while start < states.len() {
            let mut end = start + 1;
            if matches!(states[start].0, Route::Nfs(_)) {
                while end < states.len() && states[start].0.same_backend(&states[end].0) {
                    end += 1;
                }
            }
            let cursors = states[start..end]
                .iter_mut()
                .map(|state| state.2.take())
                .collect();
            let batch: Vec<_> = states[start..end]
                .iter()
                .map(|state| state.1.as_path())
                .collect();
            let pages = match &states[start].0 {
                Route::Mounted => self.mounted.read_dir_pages_with_fields(
                    &batch,
                    fields,
                    cursors,
                    page_size,
                    max_entries,
                    follow_symlinks,
                ),
                Route::Nfs(connection) => connection.client.read_dir_pages_with_fields(
                    &batch,
                    fields,
                    cursors,
                    page_size,
                    max_entries,
                    follow_symlinks,
                ),
            }
            .map_err(|error| indexed(error, start))?;
            if pages.len() != end - start {
                return Err(VfError::transport(
                    Some(start),
                    "invalid routed directory page count",
                ));
            }
            for (relative, (mut listing, next, children)) in pages.into_iter().enumerate() {
                let index = start + relative;
                listing.path = paths[index].to_path_buf();
                if let Route::Nfs(connection) = &states[index].0 {
                    for entry in &mut listing.entries {
                        *entry = DirEntry::new(
                            self.public_path(connection, entry.path())?,
                            entry.attrs().clone(),
                        );
                    }
                }
                let next = next.map(|cursor| {
                    vfsi_sync::DirPageCursor::new(RoutedDirectoryCursor {
                        route: states[index].0.clone(),
                        backend_path: states[index].1.clone(),
                        public_path: paths[index].to_path_buf(),
                        owner: self.owner.clone(),
                        cursor,
                    })
                });
                let children = children
                    .into_iter()
                    .map(|(backend_path, cursor)| {
                        let public_path = match &states[index].0 {
                            Route::Mounted => backend_path.clone(),
                            Route::Nfs(connection) => {
                                self.public_path(connection, &backend_path)?
                            }
                        };
                        let seed = vfsi_sync::DirPageCursor::new(RoutedChildDirectoryCursor(
                            RoutedDirectoryCursor {
                                route: states[index].0.clone(),
                                backend_path,
                                public_path: public_path.clone(),
                                owner: self.owner.clone(),
                                cursor,
                            },
                        ));
                        Ok((public_path, seed))
                    })
                    .collect::<VfResult<Vec<_>>>()?;
                output.push((listing, next, children));
            }
            start = end;
        }
        Ok(output)
    }

    pub(crate) fn read_files_native<P: AsRef<Path>>(
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
                Route::Mounted => self.mounted.read_files_native(&batch, options),
                Route::Nfs(connection) => connection.client.read_files_native(&batch, options),
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

    pub(crate) fn stream_native(
        &self,
        path: impl AsRef<Path>,
        options: crate::StreamOptions,
        callback: impl FnMut(u64, &[u8]) -> VfResult<std::ops::ControlFlow<()>>,
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

    pub(crate) fn remove_dir_contents_impl(
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

    /// Preserve request order, including the completed-prefix semantics of
    /// strict vector operations. Consecutive requests to one mount batch.
    pub(crate) fn open_native(&self, request: OpenOp) -> VfResult<AutoFile> {
        self.vopen_impl(&[request]).map(|mut files| files.remove(0))
    }

    pub(crate) fn vopen_impl(&self, requests: &[OpenOp]) -> VfResult<Vec<AutoFile>> {
        vfsi_core::internal::validate_open_requests(requests)?;
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
                    OpenOp::new(&resolved[index].path, requests[index].flags())
                        .mode(requests[index].creation_mode())
                })
                .collect();
            let route = resolved[start].route.clone();
            match &route {
                Route::Mounted => {
                    let files = self.mounted.vopen(&batch).map_err(|e| indexed(e, start))?;
                    for (index, file) in files.into_iter().enumerate() {
                        output.push(AutoFile::new(
                            requests[start + index].path().to_path_buf(),
                            route.clone(),
                            AutoFileInner::Mounted(file),
                            &self.owner,
                        ));
                    }
                }
                Route::Nfs(connection) => {
                    let files = connection
                        .client
                        .vopen(&batch)
                        .map_err(|e| indexed(e, start))?;
                    for (index, file) in files.into_iter().enumerate() {
                        output.push(AutoFile::new(
                            requests[start + index].path().to_path_buf(),
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

    /// Consume a batch with an explicit aggregate read budget.
    pub(crate) fn vread_impl<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, AutoFile>>,
        options: crate::ReadOptions,
    ) -> VfResult<Vec<crate::ReadResult>> {
        crate::read::consume_ops(
            ops,
            options.limit_or(self.limits.read_byte_limit()),
            |file, offset, length| AutoRead {
                file,
                offset,
                length,
            },
            |file, offset, buffer| AutoReadInto {
                file,
                offset,
                buffer,
            },
            |requests, options| self.readv_owned(requests, options),
            |requests, bytes| self.vread_into_with_limit_native(requests, bytes),
        )
    }
    fn readv_owned(
        &self,
        requests: &[crate::ReadRequest<'_, AutoRead<'_>>],
        budget: usize,
    ) -> VfResult<Vec<ReadResult>> {
        if requests.iter().all(|request| request.range_ref().is_some()) {
            return self.vread_with_limit_projected_native(requests, budget, |request| {
                request.range_ref().expect("checked range requests")
            });
        }
        crate::read::read_batch(
            requests,
            budget,
            |ranges, bytes| {
                self.vread_with_limit_projected_native(ranges, bytes, |request| request)
            },
            |paths, bytes| {
                self.read_files_native(paths, crate::ReadAllOptions::new().max_total_bytes(bytes))
            },
        )
    }

    fn vread_with_limit_projected_native<'a, T>(
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
                            .vread_with_limit_native(&batch, max_bytes)
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
                            .vread_with_limit_native(&batch, max_bytes)
                            .map_err(|e| indexed(e, start))?,
                    );
                }
            }
            start = end;
        }
        Ok(output)
    }

    fn vread_into_with_limit_native(
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
                            .vread_into_with_limit_native(&mut batch, max_bytes)
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
                            .vread_into_with_limit_native(&mut batch, max_bytes)
                            .map_err(|error| indexed(error, start))?,
                    );
                }
            }
            start = end;
        }
        Ok(output)
    }

    pub(crate) fn write_partial_native(
        &self,
        requests: &[crate::WriteOp<'_, AutoFile>],
    ) -> VfResult<Vec<WriteResult>> {
        self.write_vector(requests, false)
    }

    pub(crate) fn write_complete(
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
                // Validate empty requests too, matching FsClient::vwrite_all_native.
                if request.file().is_closed() {
                    return Err(VfError::client(index, libc::EBADF as u32)
                        .with_context("vwrite_native", request.file().path()));
                }
                request
                    .offset()
                    .checked_add(request.data().len() as u64)
                    .ok_or_else(|| {
                        VfError::client(index, libc::EOVERFLOW as u32)
                            .with_context("vwrite_native", request.file().path())
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
                        self.mounted.vwrite_all_native(&batch)
                    } else {
                        self.mounted.vwrite_native(&batch)
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
                        connection.client.vwrite_all_native(&batch)
                    } else {
                        connection.client.vwrite_native(&batch)
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
    pub(crate) fn vclose_impl(&self, files: &mut [AutoFile]) -> VfResult<()> {
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
                        .vclose(batch)
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
                        .vclose(batch)
                        .map_err(|error| indexed(error, start))?;
                }
            }
            start = end;
        }
        Ok(())
    }

    /// Rename adjacent pairs on the same backend as a vector, preserving order.
    /// Options are atomic per pair; unsupported semantics are never emulated.
    pub(crate) fn vrename_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: crate::RenameOptions,
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        if options != crate::RenameOptions::Replace {
            let mounts = read_mounts(false);
            for (index, (source, destination)) in pairs.iter().enumerate() {
                let a = self.resolve(source.as_ref(), &mounts);
                let b = self.resolve(destination.as_ref(), &mounts);
                if !matches!((&a.route, &b.route), (Route::Mounted, Route::Mounted)) {
                    return Err(
                        vfsi_sync::VfError::client(index, vfsi_core::VF_ERR_UNSUPPORTED)
                            .with_context("vrename", source.as_ref()),
                    );
                }
            }
            return self.mounted.vrename(pairs, options);
        }
        let mounts = read_mounts(false);
        let sources: Vec<_> = pairs
            .iter()
            .map(|(from, _)| self.resolve(from.as_ref(), &mounts))
            .collect();
        let targets: Vec<_> = pairs
            .iter()
            .map(|(_, to)| self.resolve(to.as_ref(), &mounts))
            .collect();
        let mut start = 0;
        while start < pairs.len() {
            if !sources[start].route.same_backend(&targets[start].route) {
                self.mounted
                    .rename(pairs[start].0.as_ref(), pairs[start].1.as_ref())
                    .map_err(|error| indexed(error, start))?;
                start += 1;
                continue;
            }
            let mut end = start + 1;
            while end < pairs.len()
                && sources[start].route.same_backend(&sources[end].route)
                && sources[start].route.same_backend(&targets[end].route)
            {
                end += 1;
            }
            let batch: Vec<_> = (start..end)
                .map(|i| (sources[i].path.as_path(), targets[i].path.as_path()))
                .collect();
            match &sources[start].route {
                Route::Mounted => self.mounted.vrename(&batch, options),
                Route::Nfs(connection) => connection.client.vrename(&batch, options),
            }
            .map_err(|error| indexed(error, start))?;
            start = end;
        }
        Ok(())
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
