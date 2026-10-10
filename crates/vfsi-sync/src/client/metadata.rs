//! Metadata, filesystem statistics, and synchronization dispatch.

use super::*;

impl<F: HandleBackend> FsClient<F> {
    /// Preflight every handle, then submit one synchronization vector.
    pub(crate) fn vfsync_impl(
        &self,
        files: &[&FsFile<F>],
        mode: vfsi_core::api::SyncMode,
    ) -> VfResult<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut raw = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            if !Arc::ptr_eq(&self.inner, &file.inner) {
                return Err(
                    VfError::client(index, crate::ERR_INVAL).with_context("vfsync", &file.path)
                );
            }
            raw.push(
                file.raw()
                    .map_err(|e| e.with_index(index).with_context("vfsync", &file.path))?
                    .clone(),
            );
        }
        self.lock()?
            .vfsync_impl(&raw, mode)
            .map_err(|error| match error.index() {
                Some(index) if index < files.len() => {
                    error.with_context("vfsync", &files[index].path)
                }
                Some(_) => {
                    VfError::transport(None, "fsync backend returned an invalid error index")
                }
                None => error,
            })
    }

    /// Query filesystems using one native vector of paths and retained handles.
    pub(crate) fn vstatfs<P: vfsi_core::AsTarget<FsFile<F>>>(
        &self,
        targets: &[P],
    ) -> VfResult<Vec<crate::FilesystemStats>> {
        use vfsi_core::Target;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut files = Vec::with_capacity(targets.len());
        let mut paths = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (raw, path) = match target.as_target() {
                Target::Path(path) => (VfFile::from_os_path(path), path),
                Target::File(file) => {
                    if !Arc::ptr_eq(&self.inner, &file.inner) {
                        return Err(VfError::client(index, crate::ERR_INVAL)
                            .with_context("vstatfs", &file.path));
                    }
                    (
                        file.raw()
                            .map_err(|e| e.with_index(index).with_context("vstatfs", &file.path))?
                            .clone(),
                        file.path.as_path(),
                    )
                }
            };
            files.push(raw);
            paths.push(path);
        }
        let results = self
            .lock()?
            .vstatfs_impl(&files)
            .map_err(|error| match error.index() {
                Some(index) if index < paths.len() => error.with_context("vstatfs", paths[index]),
                Some(_) => {
                    VfError::transport(None, "statfs backend returned an invalid error index")
                }
                None => error,
            })?;
        if results.len() != targets.len() {
            return Err(VfError::transport(
                None,
                "statfs backend returned an invalid result count",
            ));
        }
        Ok(results)
    }

    /// Update paths and open objects with one native attribute vector.
    pub(crate) fn vsetattrs<P: vfsi_core::AsTarget<FsFile<F>>>(
        &self,
        updates: &[vfsi_core::SetAttrsOp<P>],
    ) -> VfResult<()> {
        use vfsi_core::Target;
        if updates.is_empty() {
            return Ok(());
        }
        let mut paths = Vec::with_capacity(updates.len());
        let mut attrs = Vec::with_capacity(updates.len());
        for (index, op) in updates.iter().enumerate() {
            let target = op.target();
            let (raw, path) = match target.as_target() {
                Target::Path(path) => (Target::Path(path), path),
                Target::File(file) => {
                    if !Arc::ptr_eq(&self.inner, &file.inner) {
                        return Err(VfError::client(index, crate::ERR_INVAL)
                            .with_context("vsetattrs", &file.path));
                    }
                    (
                        Target::File(file.raw().map_err(|e| {
                            e.with_index(index).with_context("vsetattrs", &file.path)
                        })?),
                        file.path.as_path(),
                    )
                }
            };
            if op.requested_uid() == Some(u32::MAX) || op.requested_gid() == Some(u32::MAX) {
                return Err(
                    VfError::client(index, crate::ERR_INVAL).with_context("vsetattrs", path)
                );
            }
            paths.push(path);
            for time in [op.requested_accessed(), op.requested_modified()]
                .into_iter()
                .flatten()
            {
                crate::backend::system_time_parts(time)
                    .map_err(|e| e.with_index(index).with_context("vsetattrs", path))?;
            }
            attrs.push(op.with_target(raw));
        }
        let map_error = |error: VfError, start: usize, count: usize| match error.index() {
            Some(index) if index < count => error
                .with_index(start + index)
                .with_context("vsetattrs", paths[start + index]),
            Some(_) => VfError::transport(None, "setattrs backend returned an invalid error index"),
            None => error,
        };
        let mut backend = self.lock()?;
        let follow = updates[0].follows_symlinks();
        if updates.iter().all(|op| op.follows_symlinks() == follow) {
            return backend
                .vsetattrs_impl(&attrs)
                .map_err(|error| map_error(error, 0, updates.len()));
        }
        let mut start = 0;
        while start < updates.len() {
            let follow = updates[start].follows_symlinks();
            let count = updates[start..]
                .iter()
                .take_while(|op| op.follows_symlinks() == follow)
                .count();
            backend
                .vsetattrs_impl(&attrs[start..start + count])
                .map_err(|error| map_error(error, start, count))?;
            start += count;
        }
        Ok(())
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Vector metadata query with explicit fields and final-symlink handling.
    /// Ancestor symlinks follow the backend's normal namespace semantics.
    #[doc(hidden)]
    pub fn vgetattrs_native<P: vfsi_core::AsTarget<FsFile<F>>>(
        &self,
        targets: &[P],
        fields: AttrMask,
        follow: bool,
    ) -> VfResult<Vec<Attrs>> {
        use vfsi_core::Target;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::with_capacity(targets.len());
        let mut attrs = Vec::with_capacity(targets.len());
        let mut policies = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (raw, path, follows) = match target.as_target() {
                Target::Path(path) => (VfFile::from_os_path(path), path, follow),
                Target::File(file) => {
                    self.validate_owner(file, index)
                        .map_err(|e| e.with_context("vgetattrs", &file.path))?;
                    (
                        file.raw()
                            .map_err(|e| e.with_index(index).with_context("vgetattrs", &file.path))?
                            .clone(),
                        file.path.as_path(),
                        true,
                    )
                }
            };
            paths.push(path);
            policies.push(follows);
            attrs.push(crate::VfAttrs {
                file: raw,
                masks: fields | AttrMask::MODE,
                ..crate::VfAttrs::default()
            });
        }
        let mut backend = self.lock()?;
        let mut start = 0;
        while start < attrs.len() {
            let end = start
                + policies[start..]
                    .iter()
                    .take_while(|p| **p == policies[start])
                    .count();
            let result = if policies[start] {
                backend.vgetattrs_impl(&mut attrs[start..end])
            } else {
                backend.vgetattrs_nofollow_impl(&mut attrs[start..end])
            };
            result.map_err(|error| match error.index() {
                Some(index) if index < end - start => error
                    .with_index(start + index)
                    .with_context("vgetattrs", paths[start + index]),
                Some(_) => {
                    VfError::transport(None, "metadata backend returned an invalid error index")
                }
                None => error,
            })?;
            start = end;
        }
        Ok(attrs
            .into_iter()
            .map(vfsi_core::metadata_from_attrs)
            .collect())
    }
}
