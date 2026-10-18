//! Metadata, filesystem statistics, and synchronization dispatch.

use super::*;

impl<F: HandleBackend> FsClient<F> {
    /// Preflight every handle, then submit one synchronization vector.
    pub(crate) fn vfsync_impl(
        &self,
        files: &[&OpenFile<F>],
        mode: vfsi_core::api::SyncMode,
    ) -> VfResult<()> {
        self.vfsync_projected_native(files, mode, |file| *file)
    }

    /// Adapter projection fused with full-vector ownership preflight.
    /// Each input is projected once before acquiring the backend lock.
    #[doc(hidden)]
    pub fn vfsync_projected_native<'a, P>(
        &self,
        files: &'a [P],
        mode: vfsi_core::api::SyncMode,
        project: impl Fn(&'a P) -> &'a OpenFile<F>,
    ) -> VfResult<()>
    where
        F: 'a,
    {
        if files.is_empty() {
            return Ok(());
        }
        let mut raw = Vec::with_capacity(files.len());
        let mut projected = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let file = project(file);
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
            // Retain only a borrowed handle for error context, not a cloned
            // path or another call to potentially reentrant projection code.
            projected.push(file);
        }
        let result = self.lock()?.vfsync_impl(&raw, mode);
        result.map_err(|error| match error.index() {
            Some(index) if index < projected.len() => {
                error.with_context("vfsync", &projected[index].path)
            }
            Some(_) => VfError::transport(None, "fsync backend returned an invalid error index"),
            None => error,
        })
    }

    /// Query filesystems using one native vector of paths and retained handles.
    pub(crate) fn vstatfs<P: vfsi_core::AsTarget<OpenFile<F>>>(
        &self,
        targets: &[P],
    ) -> VfResult<Vec<crate::FilesystemStats>> {
        self.vstatfs_projected_native(targets, |target| target.as_target())
    }

    /// Adapter projection fused with full-vector ownership preflight.
    #[doc(hidden)]
    pub fn vstatfs_projected_native<'a, P>(
        &self,
        targets: &'a [P],
        project: impl Fn(&'a P) -> vfsi_core::Target<'a, OpenFile<F>>,
    ) -> VfResult<Vec<crate::FilesystemStats>>
    where
        F: 'a,
    {
        use vfsi_core::Target;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut files = Vec::with_capacity(targets.len());
        let mut paths = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (raw, path) = match project(target) {
                Target::Path(path) => (FileOperand::from_os_path(path), path),
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
    pub(crate) fn vsetattrs<P: vfsi_core::AsTarget<OpenFile<F>>>(
        &self,
        updates: &[vfsi_core::SetAttrsOp<P>],
    ) -> VfResult<()> {
        self.vsetattrs_projected_native(updates, |target| target.as_target())
    }

    /// Adapter projection fused with full-vector validation before mutation.
    #[doc(hidden)]
    pub fn vsetattrs_projected_native<'a, P>(
        &self,
        updates: &'a [vfsi_core::SetAttrsOp<P>],
        project: impl Fn(&'a P) -> vfsi_core::Target<'a, OpenFile<F>>,
    ) -> VfResult<()>
    where
        F: 'a,
    {
        use vfsi_core::Target;
        if updates.is_empty() {
            return Ok(());
        }
        let mut paths = Vec::with_capacity(updates.len());
        let mut attrs = Vec::with_capacity(updates.len());
        for (index, op) in updates.iter().enumerate() {
            let target = op.target();
            let (raw, path) = match project(target) {
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
            vfsi_core::internal::validate_setattrs(op)
                .map_err(|error| error.with_index(index).with_context("vsetattrs", path))?;
            paths.push(path);
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
        let follow = updates[0].follow_final_symlink();
        if updates.iter().all(|op| op.follow_final_symlink() == follow) {
            return backend
                .vsetattrs_impl(&attrs)
                .map_err(|error| map_error(error, 0, updates.len()));
        }
        let mut start = 0;
        while start < updates.len() {
            let follow = updates[start].follow_final_symlink();
            let count = updates[start..]
                .iter()
                .take_while(|op| op.follow_final_symlink() == follow)
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
    pub fn vgetattrs_native<P: vfsi_core::AsTarget<OpenFile<F>>>(
        &self,
        targets: &[P],
        fields: AttrMask,
        follow: bool,
    ) -> VfResult<Vec<Attrs>> {
        self.vgetattrs_projected_native(targets, fields, follow, |target| target.as_target())
    }

    /// Adapter projection fused with ownership preflight and metadata dispatch.
    #[doc(hidden)]
    pub fn vgetattrs_projected_native<'a, P>(
        &self,
        targets: &'a [P],
        fields: AttrMask,
        follow: bool,
        project: impl Fn(&'a P) -> vfsi_core::Target<'a, OpenFile<F>>,
    ) -> VfResult<Vec<Attrs>>
    where
        F: 'a,
    {
        use vfsi_core::Target;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::with_capacity(targets.len());
        let mut attrs = Vec::with_capacity(targets.len());
        let mut policies = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (raw, path, follows) = match project(target) {
                Target::Path(path) => (FileOperand::from_os_path(path), path, follow),
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
