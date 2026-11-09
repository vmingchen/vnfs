//! Metadata, filesystem statistics, and synchronization dispatch.

use super::*;

impl<F: HandleBackend> FsClient<F> {
    /// Synchronize valid independent handles in one backend batch.
    pub(crate) fn vfsync_impl(
        &self,
        files: &[&OpenFile<F>],
        mode: vfsi_core::api::SyncMode,
    ) -> vfsi_core::VecResult<()> {
        self.vfsync_projected_native(files, mode, |file| *file)
    }

    /// Project each input once before locking the backend. Invalid owners and
    /// closed handles retain their own NotAttempted slots.
    #[doc(hidden)]
    pub fn vfsync_projected_native<'a, P>(
        &self,
        files: &'a [P],
        mode: vfsi_core::api::SyncMode,
        project: impl Fn(&'a P) -> &'a OpenFile<F>,
    ) -> vfsi_core::VecResult<()>
    where
        F: 'a,
    {
        use vfsi_core::OpError;
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut raw = Vec::with_capacity(files.len());
        let mut projected = Vec::with_capacity(files.len());
        let mut indices = Vec::new();
        let mut rejected = Vec::new();
        for (index, input) in files.iter().enumerate() {
            let file = project(input);
            match self
                .validate_owner(file, index)
                .and_then(|()| file.raw().cloned())
            {
                Ok(file_raw) => {
                    if !rejected.is_empty() {
                        indices.push(index);
                    }
                    raw.push(file_raw);
                    projected.push(file);
                }
                Err(error) => {
                    if rejected.is_empty() {
                        indices.extend(0..index);
                    }
                    rejected.push((
                        index,
                        OpError::not_attempted(error).with_context("vfsync", &file.path),
                    ));
                }
            }
        }
        let outcomes = if raw.is_empty() {
            Ok(Vec::new())
        } else {
            self.lock()?.vfsync_impl(&raw, mode)
        };
        let mut outcomes = match outcomes {
            Ok(outcomes) if outcomes.len() <= raw.len() => outcomes,
            Ok(_) => raw
                .iter()
                .map(|_| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "too many synchronization outcomes"),
                        None,
                    ))
                })
                .collect(),
            Err(error) => {
                let error = if error.index().is_some_and(|index| index >= raw.len()) {
                    VfError::invalid_reply(None, "fsync backend returned an invalid error index")
                } else {
                    error
                };
                if rejected.is_empty() {
                    return Err(with_indexed_context(error, "vfsync", |index| {
                        projected.get(index).map(|file| file.path.as_path())
                    }));
                }
                raw.iter()
                    .map(|_| Err(OpError::not_attempted(error.clone())))
                    .collect()
            }
        };
        outcomes.resize_with(raw.len(), || {
            Err(OpError::unknown(
                VfError::invalid_reply(None, "missing synchronization outcome"),
                None,
            ))
        });
        let results = outcomes.into_iter().zip(projected).map(|(result, file)| {
            let result = match result {
                Err(error) if error.progress().is_some() => Err(OpError::unknown(
                    VfError::invalid_reply(None, "synchronization cannot report I/O byte progress"),
                    None,
                )),
                result => result,
            };
            result.map_err(|error| error.with_context("vfsync", &file.path))
        });
        if rejected.is_empty() {
            return Ok(results.collect());
        }
        let mut output: Vec<_> = (0..files.len()).map(|_| None).collect();
        for (index, result) in indices.into_iter().zip(results) {
            output[index] = Some(result);
        }
        for (index, error) in rejected {
            output[index] = Some(Err(error));
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every sync input assigned"))
            .collect())
    }

    /// Query filesystems using one native vector of paths and retained handles.
    pub(crate) fn vstatfs<P: vfsi_core::AsTarget<OpenFile<F>>>(
        &self,
        targets: &[P],
    ) -> vfsi_core::VecResult<crate::FilesystemStats> {
        self.vstatfs_projected_native(targets, |target| target.as_target())
    }

    /// Adapter projection fused with full-vector ownership preflight.
    #[doc(hidden)]
    pub fn vstatfs_projected_native<'a, P>(
        &self,
        targets: &'a [P],
        project: impl Fn(&'a P) -> vfsi_core::Target<'a, OpenFile<F>>,
    ) -> vfsi_core::VecResult<crate::FilesystemStats>
    where
        F: 'a,
    {
        use vfsi_core::{OpError, Target};
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut output: Vec<Option<vfsi_core::OpResult<crate::FilesystemStats>>> =
            (0..targets.len()).map(|_| None).collect();
        let mut files = Vec::with_capacity(targets.len());
        let mut paths = Vec::with_capacity(targets.len());
        let mut indices = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (raw, path) = match project(target) {
                Target::Path(path) => (Ok(FileOperand::from_os_path(path)), path),
                Target::File(file) => {
                    let raw = self
                        .validate_owner(file, index)
                        .and_then(|()| file.raw().cloned())
                        .map_err(|error| error.with_context("vstatfs", &file.path));
                    (raw, file.path.as_path())
                }
            };
            match raw {
                Ok(raw) => {
                    files.push(raw);
                    paths.push(path);
                    indices.push(index);
                }
                Err(error) => output[index] = Some(Err(OpError::not_attempted(error))),
            }
        }
        if !files.is_empty() {
            let results = self.lock()?.vstatfs_impl(&files);
            match results {
                Ok(results) if results.len() <= files.len() => {
                    let mut results = results.into_iter();
                    for (local, &index) in indices.iter().enumerate() {
                        output[index] = Some(
                            results
                                .next()
                                .unwrap_or_else(|| {
                                    Err(OpError::unknown(
                                        VfError::invalid_reply(None, "missing statfs outcome"),
                                        None,
                                    ))
                                })
                                .map_err(|error| error.with_context("vstatfs", paths[local])),
                        );
                    }
                }
                Ok(_) => {
                    for (local, &index) in indices.iter().enumerate() {
                        output[index] = Some(Err(OpError::unknown(
                            VfError::invalid_reply(
                                None,
                                "statfs backend returned too many outcomes",
                            ),
                            None,
                        )
                        .with_context("vstatfs", paths[local])));
                    }
                }
                Err(error) => {
                    for (local, &index) in indices.iter().enumerate() {
                        output[index] = Some(Err(OpError::not_attempted(error.clone())
                            .with_context("vstatfs", paths[local])));
                    }
                }
            }
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every input assigned a statfs outcome"))
            .collect())
    }

    /// Update paths and open objects with one native attribute vector.
    pub(crate) fn vsetattrs<P: vfsi_core::AsTarget<OpenFile<F>>>(
        &self,
        updates: &[vfsi_core::SetAttrsOp<P>],
    ) -> vfsi_core::VecResult<()> {
        self.vsetattrs_projected_native(updates, |target| target.as_target())
    }

    /// Project each target once and isolate validation failures before dispatch.
    #[doc(hidden)]
    pub fn vsetattrs_projected_native<'a, P>(
        &self,
        updates: &'a [vfsi_core::SetAttrsOp<P>],
        project: impl Fn(&'a P) -> vfsi_core::Target<'a, OpenFile<F>>,
    ) -> vfsi_core::VecResult<()>
    where
        F: 'a,
    {
        use vfsi_core::{OpError, Target};
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let mut slots = (0..updates.len()).map(|_| None).collect::<Vec<_>>();
        let mut paths = Vec::with_capacity(updates.len());
        let mut attrs = Vec::with_capacity(updates.len());
        let mut indices = Vec::with_capacity(updates.len());
        for (index, op) in updates.iter().enumerate() {
            let target = project(op.target());
            let path = match target {
                Target::Path(path) => path,
                Target::File(file) => file.path.as_path(),
            };
            let raw = (|| {
                vfsi_core::internal::validate_setattrs(op)?;
                Ok(match target {
                    Target::Path(path) => Target::Path(path),
                    Target::File(file) => {
                        self.validate_owner(file, index)?;
                        Target::File(file.raw()?)
                    }
                })
            })();
            match raw {
                Ok(raw) => {
                    indices.push(index);
                    paths.push(path);
                    attrs.push(op.with_target(raw));
                }
                Err(cause) => {
                    slots[index] = Some(Err(
                        OpError::not_attempted(cause).with_context("vsetattrs", path)
                    ))
                }
            }
        }
        if !attrs.is_empty() {
            let mut backend = self.lock()?;
            let mut start = 0;
            while start < attrs.len() {
                let follow = attrs[start].follow_final_symlink();
                let count = attrs[start..]
                    .iter()
                    .take_while(|op| op.follow_final_symlink() == follow)
                    .count();
                let result = super::namespace::namespace_results(
                    backend.vsetattrs_impl(&attrs[start..start + count]),
                    &paths[start..start + count],
                    "vsetattrs",
                );
                let results = match result {
                    Ok(results) => results,
                    Err(cause) => (0..count)
                        .map(|offset| {
                            Err(OpError::not_attempted(cause.clone())
                                .with_context("vsetattrs", paths[start + offset]))
                        })
                        .collect(),
                };
                for (&index, result) in indices[start..start + count].iter().zip(results) {
                    slots[index] = Some(result);
                }
                start += count;
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all attribute inputs assigned"))
            .collect())
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
    ) -> vfsi_core::VecResult<Attrs> {
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
    ) -> vfsi_core::VecResult<Attrs>
    where
        F: 'a,
    {
        use vfsi_core::{OpError, Target};
        let mut output: Vec<Option<vfsi_core::OpResult<Attrs>>> =
            (0..targets.len()).map(|_| None).collect();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut attrs = Vec::with_capacity(targets.len());
        let mut paths = Vec::with_capacity(targets.len());
        let mut policies = Vec::with_capacity(targets.len());
        let mut indices = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (raw, path, policy) = match project(target) {
                Target::Path(path) => (Ok(FileOperand::from_os_path(path)), path, follow),
                Target::File(file) => (
                    self.validate_owner(file, index)
                        .and_then(|()| file.raw().cloned())
                        .map_err(|error| error.with_context("vgetattrs", &file.path)),
                    file.path.as_path(),
                    true,
                ),
            };
            match raw {
                Ok(file) => {
                    indices.push(index);
                    paths.push(path);
                    policies.push(policy);
                    attrs.push(crate::VfAttrs {
                        file,
                        masks: fields | AttrMask::MODE,
                        ..Default::default()
                    });
                }
                Err(error) => output[index] = Some(Err(OpError::not_attempted(error))),
            }
        }
        let mut statuses = Vec::with_capacity(attrs.len());
        if !attrs.is_empty() {
            let mut backend = self.lock()?;
            let mut start = 0;
            while start < attrs.len() {
                let end = start
                    + policies[start..]
                        .iter()
                        .take_while(|policy| **policy == policies[start])
                        .count();
                let result = if policies[start] {
                    backend.vgetattrs_impl(&mut attrs[start..end])
                } else {
                    backend.vgetattrs_nofollow_impl(&mut attrs[start..end])
                };
                match result {
                    Ok(results) if results.len() <= end - start => {
                        let mut results = results.into_iter();
                        for path in &paths[start..end] {
                            statuses.push(
                                results
                                    .next()
                                    .unwrap_or_else(|| {
                                        Err(OpError::unknown(
                                            VfError::invalid_reply(
                                                None,
                                                "missing metadata outcome",
                                            ),
                                            None,
                                        ))
                                    })
                                    .map_err(|error| error.with_context("vgetattrs", *path)),
                            );
                        }
                    }
                    Ok(_) => {
                        for path in &paths[start..end] {
                            statuses.push(Err(OpError::unknown(
                                VfError::invalid_reply(None, "too many metadata outcomes"),
                                None,
                            )
                            .with_context("vgetattrs", *path)));
                        }
                    }
                    Err(error) => {
                        for path in &paths[start..end] {
                            statuses.push(Err(OpError::not_attempted(error.clone())
                                .with_context("vgetattrs", *path)));
                        }
                    }
                }
                start = end;
            }
        }
        for ((index, attr), status) in indices.into_iter().zip(attrs).zip(statuses) {
            output[index] = Some(status.map(|()| vfsi_core::metadata_from_attrs(attr)));
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every metadata input assigned"))
            .collect())
    }
}
