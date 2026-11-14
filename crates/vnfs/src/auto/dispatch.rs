//! Vector dispatch and per-input mapping across backend route cohorts.

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
            .map_err(|_| VfError::errno(0, libc::EIO as u32))?
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

    pub(crate) fn vopen_dirs_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
    ) -> vfsi_core::VecResult<AutoDir> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        let mounts = read_mounts(true);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| self.resolve_tree(path, &mounts))
            .collect();
        self.vopen_dirs_resolved(&paths, &resolved)
    }

    pub(super) fn vopen_dirs_resolved(
        &self,
        paths: &[&Path],
        resolved: &[Resolved],
    ) -> vfsi_core::VecResult<AutoDir> {
        let mut output = Vec::with_capacity(paths.len());
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(resolved, start);
            let batch: Vec<_> = resolved[start..end].iter().map(|r| &r.path).collect();
            let result = match &resolved[start].route {
                Route::Posix => self.mounted.vopen_dirs(&batch).map(|dirs| {
                    dirs.into_iter()
                        .map(|result| result.map(AutoDirInner::Posix))
                        .collect()
                }),
                Route::Nfs(connection) => connection.client.vopen_dirs(&batch).map(|dirs| {
                    dirs.into_iter()
                        .map(|result| result.map(AutoDirInner::Nfs))
                        .collect()
                }),
            };
            for (index, result) in (start..end).zip(route_results(result, end - start)) {
                output.push(
                    result
                        .map(|inner| AutoDir {
                            path: paths[index].to_owned(),
                            route: resolved[index].route.clone(),
                            inner,
                            owner: Arc::clone(&self.owner),
                        })
                        .map_err(|error| error.with_context("vopen_dirs", paths[index])),
                );
            }
            start = end;
        }
        Ok(output)
    }

    pub(crate) fn vremove_dir_contents_impl(
        &self,
        dirs: &[&AutoDir],
        options: crate::RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if dirs.is_empty() {
            return Ok(Vec::new());
        }
        let mut slots: Vec<_> = (0..dirs.len()).map(|_| None).collect();
        let mut resolved = Vec::with_capacity(dirs.len());
        for (index, dir) in dirs.iter().enumerate() {
            let cause = if !Arc::ptr_eq(&self.owner, &dir.owner) || dir.is_closed() {
                Some(
                    VfError::errno(None, libc::EBADF as u32)
                        .with_context("vremove_dir_contents", &dir.path),
                )
            } else if matches!(&dir.route,Route::Nfs(connection) if AuthSysIdentity::current().as_ref()!=Some(&connection.credentials))
            {
                Some(VfError::errno(None, libc::EACCES as u32).with_context("auto_auth", &dir.path))
            } else {
                None
            };
            if let Some(cause) = cause {
                slots[index] = Some(Err(OpError::not_attempted(cause)));
            }
            resolved.push(Resolved {
                route: dir.route.clone(),
                path: dir.path.clone(),
            });
        }
        let mut start = 0;
        while start < dirs.len() {
            let end = cohort_end(&resolved, start);
            let indices: Vec<_> = (start..end)
                .filter(|&index| slots[index].is_none())
                .collect();
            macro_rules! batch {
                ($variant:ident) => {
                    indices
                        .iter()
                        .map(|&index| match &dirs[index].inner {
                            AutoDirInner::$variant(inner) => inner,
                            _ => unreachable!("retained directory route"),
                        })
                        .collect::<Vec<_>>()
                };
            }
            let result = if indices.is_empty() {
                Ok(Vec::new())
            } else {
                match &resolved[start].route {
                    Route::Posix => self.mounted.vremove_dir_contents(&batch!(Posix), options),
                    Route::Nfs(connection) => connection
                        .client
                        .vremove_dir_contents(&batch!(Nfs), options),
                }
            };
            for (index, result) in indices
                .iter()
                .copied()
                .zip(route_results(result, indices.len()))
            {
                slots[index] = Some(result.map_err(|error| {
                    error.with_context("vremove_dir_contents", &dirs[index].path)
                }));
            }
            start = end;
        }
        Ok(slots.into_iter().map(Option::unwrap).collect())
    }

    /// Common routed capabilities. Server-copy acceleration is route-specific
    /// and is not advertised as a guarantee for the whole namespace.
    pub(crate) fn capabilities_impl(&self) -> VfResult<crate::Capabilities> {
        self.mounted.capabilities().map(|caps| {
            caps & !(crate::Capabilities::RENAME_NOREPLACE | crate::Capabilities::RENAME_EXCHANGE)
        })
    }

    /// Keep target text unchanged and let the kernel interpret it in the
    /// mounted namespace, matching the scalar symlink operation.
    pub(crate) fn vsymlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> vfsi_core::VecResult<()> {
        self.mounted.vsymlink(pairs)
    }

    /// Read link text through the mounted namespace without following links.
    pub(crate) fn vreadlink_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
    ) -> vfsi_core::VecResult<std::path::PathBuf> {
        self.mounted.vreadlink(paths)
    }

    /// Batch same-backend hard links. Cross-route requests retain their own
    /// EXDEV rejection while other route cohorts still execute.
    pub(crate) fn vhardlink_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mounts = read_mounts(false);
        let sources: Vec<_> = pairs
            .iter()
            .map(|(source, _)| self.resolve_entry(source.as_ref(), &mounts))
            .collect();
        let links: Vec<_> = pairs
            .iter()
            .map(|(_, link)| self.resolve_entry(link.as_ref(), &mounts))
            .collect();
        let mut output = Vec::with_capacity(pairs.len());
        let mut start = 0;
        while start < pairs.len() {
            let end = cohort_end(&sources, start);
            let same_route = |index: usize| sources[index].route.same_backend(&links[index].route);
            let batch: Vec<_> = (start..end)
                .filter(|&index| same_route(index))
                .map(|index| (sources[index].path.as_path(), links[index].path.as_path()))
                .collect();
            let result = if batch.is_empty() {
                Ok(Vec::new())
            } else {
                match &sources[start].route {
                    Route::Posix => self.mounted.vhardlink(&batch),
                    Route::Nfs(connection) => connection.client.vhardlink(&batch),
                }
            };
            let mut outcomes = route_results(result, batch.len()).into_iter();
            for (index, (_, link)) in pairs.iter().enumerate().take(end).skip(start) {
                let result = if same_route(index) {
                    outcomes.next().expect("validated hard-link route count")
                } else {
                    Err(OpError::not_attempted(VfError::errno(
                        None,
                        libc::EXDEV as u32,
                    )))
                };
                output.push(result.map_err(|error| error.with_context("vhardlink", link.as_ref())));
            }
            start = end;
        }
        Ok(output)
    }

    pub(crate) fn vstatfs_impl<P: vfsi_core::AsTarget<AutoFile>>(
        &self,
        inputs: &[P],
    ) -> vfsi_core::VecResult<crate::FilesystemStats> {
        use vfsi_core::{OpError, Target};
        let mut output: Vec<Option<vfsi_core::OpResult<crate::FilesystemStats>>> =
            (0..inputs.len()).map(|_| None).collect();
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mounts = read_mounts(false);
        let mut targets = Vec::with_capacity(inputs.len());
        let mut resolved = Vec::with_capacity(inputs.len());
        let mut indices = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.iter().enumerate() {
            let target = input.as_target();
            let route = match target {
                Target::Path(path) => self.resolve(path, &mounts),
                Target::File(file) => {
                    let valid = self.check_owner(file, index).and_then(|()| {
                        if file.is_closed() {
                            Err(VfError::errno(index, libc::EBADF as u32))
                        } else {
                            Ok(())
                        }
                    });
                    if let Err(error) = valid {
                        output[index] = Some(Err(
                            OpError::not_attempted(error).with_context("vstatfs", &file.path)
                        ));
                        continue;
                    }
                    Resolved {
                        route: file.route.clone(),
                        path: file.path.clone(),
                    }
                }
            };
            indices.push(index);
            targets.push(target);
            resolved.push(route);
        }
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
                Route::Posix => self.mounted.vstatfs(&batch!(Posix)),
                Route::Nfs(connection) => connection.client.vstatfs(&batch!(Nfs)),
            };
            let results = match result {
                Ok(results) if results.len() == end - start => results,
                Ok(_) => (start..end)
                    .map(|_| {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "invalid routed statfs outcome count"),
                            None,
                        ))
                    })
                    .collect(),
                Err(error) => (start..end)
                    .map(|_| Err(OpError::not_attempted(error.clone())))
                    .collect(),
            };
            for (local, result) in results.into_iter().enumerate() {
                let index = start + local;
                output[indices[index]] = Some(result.map_err(|error| {
                    let path = match targets[index] {
                        Target::Path(path) => path,
                        Target::File(file) => file.path.as_path(),
                    };
                    error.with_context("vstatfs", path)
                }));
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every routed statfs input assigned"))
            .collect())
    }
    /// Update route-coherent batches of paths and open objects.
    pub(crate) fn vsetattrs_impl<P: vfsi_core::AsTarget<AutoFile>>(
        &self,
        updates: &[crate::SetAttrsOp<P>],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::{OpError, Target};
        let updates: Vec<_> = updates
            .iter()
            .map(|op| op.with_target(op.target().as_target()))
            .collect();
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let mounts = read_mounts(false);
        let mut slots = (0..updates.len()).map(|_| None).collect::<Vec<_>>();
        let mut resolved = Vec::with_capacity(updates.len());
        for (index, op) in updates.iter().enumerate() {
            let route = match *op.target() {
                Target::Path(path) => self.resolve(path, &mounts),
                Target::File(file) => Resolved {
                    route: file.route.clone(),
                    path: file.path.clone(),
                },
            };
            let valid = (|| {
                if let Target::File(file) = *op.target() {
                    self.check_owner(file, index)?;
                    if file.is_closed() {
                        return Err(VfError::errno(None, libc::EBADF as u32));
                    }
                }
                vfsi_core::internal::validate_setattrs(op).map(|_| ())
            })();
            if let Err(cause) = valid {
                slots[index] = Some(Err(
                    OpError::not_attempted(cause).with_context("vsetattrs", &route.path)
                ));
            }
            resolved.push(route);
        }
        let mut start = 0;
        while start < updates.len() {
            let end = cohort_end(&resolved, start);
            let indices: Vec<_> = (start..end)
                .filter(|&index| slots[index].is_none())
                .collect();
            macro_rules! batch {
                ($variant:ident) => {{
                    indices
                        .iter()
                        .map(|&index| {
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
            let result = if indices.is_empty() {
                Ok(Vec::new())
            } else {
                match &resolved[start].route {
                    Route::Posix => self.mounted.vsetattrs(&batch!(Posix)),
                    Route::Nfs(connection) => connection.client.vsetattrs(&batch!(Nfs)),
                }
            };
            for (index, result) in indices
                .iter()
                .copied()
                .zip(route_results(result, indices.len()))
            {
                let path = match *updates[index].target() {
                    Target::Path(path) => path,
                    Target::File(file) => file.path.as_path(),
                };
                slots[index] = Some(result.map_err(|error| error.with_context("vsetattrs", path)));
            }
            start = end;
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all routed attribute inputs assigned"))
            .collect())
    }

    pub(crate) fn vgetattrs_impl<P: vfsi_core::AsTarget<AutoFile>>(
        &self,
        inputs: &[P],
        options: crate::AttrsOptions,
    ) -> vfsi_core::VecResult<crate::Attrs> {
        use vfsi_core::{OpError, Target};
        let mut output: Vec<Option<vfsi_core::OpResult<crate::Attrs>>> =
            (0..inputs.len()).map(|_| None).collect();
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mounts = read_mounts(false);
        let mut targets = Vec::with_capacity(inputs.len());
        let mut resolved = Vec::with_capacity(inputs.len());
        let mut indices = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.iter().enumerate() {
            let target = input.as_target();
            let route = match target {
                Target::Path(path) => self.resolve(path, &mounts),
                Target::File(file) => {
                    let valid = self.check_owner(file, index).and_then(|()| {
                        if file.is_closed() {
                            Err(VfError::errno(index, libc::EBADF as u32))
                        } else {
                            Ok(())
                        }
                    });
                    if let Err(error) = valid {
                        output[index] = Some(Err(
                            OpError::not_attempted(error).with_context("vgetattrs", &file.path)
                        ));
                        continue;
                    }
                    Resolved {
                        route: file.route.clone(),
                        path: file.path.clone(),
                    }
                }
            };
            indices.push(index);
            targets.push(target);
            resolved.push(route);
        }
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
                Route::Posix => self.mounted.vgetattrs(&batch!(Posix), options),
                Route::Nfs(connection) => connection.client.vgetattrs(&batch!(Nfs), options),
            };
            let results = match result {
                Ok(results) if results.len() == end - start => results,
                Ok(_) => (start..end)
                    .map(|_| {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "invalid routed metadata outcome count"),
                            None,
                        ))
                    })
                    .collect(),
                Err(error) => (start..end)
                    .map(|_| Err(OpError::not_attempted(error.clone())))
                    .collect(),
            };
            for (local, result) in results.into_iter().enumerate() {
                let index = start + local;
                output[indices[index]] = Some(result.map_err(|error| {
                    let path = match targets[index] {
                        Target::Path(path) => path,
                        Target::File(file) => file.path.as_path(),
                    };
                    error.with_context("vgetattrs", path)
                }));
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every routed metadata input assigned"))
            .collect())
    }
    pub(crate) fn vfsync_impl(
        &self,
        files: &[&AutoFile],
        mode: crate::SyncMode,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let mut output: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                self.check_owner(file, index)
                    .and_then(|()| {
                        if file.is_closed() {
                            Err(VfError::errno(None, libc::EBADF as u32))
                        } else {
                            Ok(())
                        }
                    })
                    .err()
                    .map(|error| {
                        Err(OpError::not_attempted(error).with_context("vfsync", &file.path))
                    })
            })
            .collect();
        let mut start = 0;
        while start < files.len() {
            let route = &files[start].route;
            let mut end = start + 1;
            while end < files.len() && route.same_backend(&files[end].route) {
                end += 1;
            }
            macro_rules! batch {
                ($variant:ident) => {{
                    files[start..end]
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| output[start + index].is_none())
                        .map(|(_, file)| match &file.inner {
                            AutoFileInner::$variant(inner) => inner,
                            _ => unreachable!("validated route"),
                        })
                        .collect::<Vec<_>>()
                }};
            }
            let result = match route {
                Route::Posix => {
                    let batch = batch!(Posix);
                    self.mounted.vfsync(&batch, mode)
                }
                Route::Nfs(connection) => {
                    let batch = batch!(Nfs);
                    connection.client.vfsync(&batch, mode)
                }
            };
            let count = output[start..end]
                .iter()
                .filter(|slot| slot.is_none())
                .count();
            let mut result = route_results(result, count).into_iter();
            for (index, slot) in output[start..end].iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(
                        result
                            .next()
                            .expect("validated synchronization count")
                            .map_err(|error| {
                                error.with_context("vfsync", &files[start + index].path)
                            }),
                    );
                }
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every routed sync input assigned"))
            .collect())
    }

    pub(crate) fn vmkdir_impl<P: AsRef<Path>>(
        &self,
        paths: &[crate::MkDirOp<P>],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen = std::collections::HashSet::with_capacity(paths.len());
        let mut slots = (0..paths.len()).map(|_| None).collect::<Vec<_>>();
        for (index, op) in paths.iter().enumerate() {
            if !seen.insert(op.path()) {
                slots[index] = Some(Err(OpError::not_attempted(VfError::from_kind(
                    None,
                    std::io::ErrorKind::InvalidInput,
                ))
                .with_context("vmkdir", op.path())));
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
            let batch: Vec<_> = (start..end)
                .filter(|&index| slots[index].is_none())
                .map(|index| {
                    crate::MkDirOp::new(resolved[index].path.as_path(), paths[index].mode())
                })
                .collect();
            let result = if batch.is_empty() {
                Ok(Vec::new())
            } else {
                match &resolved[start].route {
                    Route::Posix => self.mounted.vmkdir(&batch),
                    Route::Nfs(connection) => connection.client.vmkdir(&batch),
                }
            };
            let mut outcomes = route_results(result, batch.len()).into_iter();
            for (offset, slot) in slots[start..end].iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(
                        outcomes
                            .next()
                            .expect("validated directory route count")
                            .map_err(|error| {
                                error.with_context("vmkdir", paths[start + offset].path())
                            }),
                    );
                }
            }
            start = end;
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all mounted directory inputs assigned"))
            .collect())
    }

    pub(crate) fn vremove_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
        mode: vfsi_core::api::RemoveMode,
        options: crate::RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        let mounts = read_mounts(true);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| {
                if mode == vfsi_core::api::RemoveMode::Entry {
                    self.resolve(path, &mounts)
                } else {
                    self.resolve_tree(path, &mounts)
                }
            })
            .collect();
        let mut output = Vec::with_capacity(paths.len());
        let mut start = 0;
        while start < paths.len() {
            let end = cohort_end(&resolved, start);
            let batch: Vec<_> = resolved[start..end]
                .iter()
                .map(|route| route.path.as_path())
                .collect();
            let result = match &resolved[start].route {
                Route::Posix => self.mounted.vremove_impl(&batch, mode, options),
                Route::Nfs(connection) => connection.client.vremove_impl(&batch, mode, options),
            };
            output.extend(
                route_results(result, batch.len())
                    .into_iter()
                    .zip(&paths[start..end])
                    .map(|(result, path)| {
                        result.map_err(|error| error.with_context("vremove", path))
                    }),
            );
            start = end;
        }
        Ok(output)
    }

    pub(crate) fn vcopy_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::CopyOptions,
    ) -> vfsi_core::VecResult<()> {
        let mounts = read_mounts(false);
        let sources: Vec<_> = pairs
            .iter()
            .map(|(from, _)| self.resolve(from.as_ref(), &mounts))
            .collect();
        let targets: Vec<_> = pairs
            .iter()
            .map(|(_, to)| self.resolve(to.as_ref(), &mounts))
            .collect();
        let mut slots = (0..pairs.len()).map(|_| None).collect::<Vec<_>>();
        for (index, (source, target)) in sources.iter().zip(&targets).enumerate() {
            if !source.route.same_backend(&target.route) {
                slots[index] = Some(Err(vfsi_core::OpError::not_attempted(
                    VfError::unsupported(None),
                )
                .with_context("vcopy", pairs[index].1.as_ref())));
            }
        }
        let mut start = 0;
        while start < pairs.len() {
            let end = cohort_end(&sources, start);
            let indices: Vec<_> = (start..end)
                .filter(|&index| slots[index].is_none())
                .collect();
            let batch: Vec<_> = indices
                .iter()
                .map(|&index| (sources[index].path.as_path(), targets[index].path.as_path()))
                .collect();
            let result = if batch.is_empty() {
                Ok(Vec::new())
            } else {
                match &sources[start].route {
                    Route::Posix => self.mounted.vcopy(&batch, options),
                    Route::Nfs(connection) => connection.client.vcopy(&batch, options),
                }
            };
            for (index, result) in indices
                .iter()
                .copied()
                .zip(route_results(result, indices.len()))
            {
                slots[index] = Some(
                    result.map_err(|error| error.with_context("vcopy", pairs[index].1.as_ref())),
                );
            }
            start = end;
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all copy routes assigned"))
            .collect())
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
            Route::Posix => Ok(1),
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
    ) -> vfsi_core::VecResult<vfsi_sync::DirectoryPage> {
        use vfsi_core::OpError;
        if paths.len() != cursors.len() || page_size == 0 {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        if paths.is_empty() {
            return Ok(Vec::new());
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
        let mut output: Vec<_> = (0..paths.len()).map(|_| None).collect();
        let mut states = Vec::new();
        for (index, (path, cursor)) in paths.iter().zip(cursors).enumerate() {
            let state = (|| {
                Ok(match cursor {
                    Some(cursor) => {
                        let child = cursor.is::<RoutedChildDirectoryCursor>();
                        let saved = if child {
                            cursor.into_state::<RoutedChildDirectoryCursor>()?.0
                        } else {
                            cursor.into_state::<RoutedDirectoryCursor>()?
                        };
                        if !Arc::ptr_eq(&saved.owner, &self.owner) || saved.public_path != *path {
                            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
                        }
                        if let Route::Nfs(connection) = &saved.route
                            && AuthSysIdentity::current().as_ref() != Some(&connection.credentials)
                        {
                            return Err(VfError::errno(None, libc::EACCES as u32)
                                .with_context("auto_auth", path));
                        }
                        if child {
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
                })
            })();
            match state {
                Ok((route, path, cursor)) => states.push((index, route, path, cursor)),
                Err(cause) => {
                    output[index] = Some(Err(
                        OpError::not_attempted(cause).with_context("vlistdir_pages", path)
                    ))
                }
            }
        }
        let mut start = 0;
        while start < states.len() {
            let mut end = start + 1;
            if matches!(states[start].1, Route::Nfs(_)) {
                while end < states.len() && states[start].1.same_backend(&states[end].1) {
                    end += 1;
                }
            }
            let cursors = states[start..end]
                .iter_mut()
                .map(|state| state.3.take())
                .collect();
            let batch: Vec<_> = states[start..end]
                .iter()
                .map(|state| state.2.as_path())
                .collect();
            let pages = match &states[start].1 {
                Route::Posix => self.mounted.read_dir_pages_with_fields(
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
            };
            for (relative, result) in route_results(pages, end - start).into_iter().enumerate() {
                let state = &states[start + relative];
                let index = state.0;
                output[index] = Some(
                    result
                        .map_err(|error| {
                            if error.progress().is_some() {
                                OpError::unknown(
                                    VfError::invalid_reply(
                                        None,
                                        "directory page cannot report byte progress",
                                    ),
                                    None,
                                )
                            } else {
                                error
                            }
                        })
                        .and_then(|(mut listing, next, children)| {
                            let convert = |cause| OpError::unknown(cause, None);
                            listing.path = paths[index].to_path_buf();
                            if let Route::Nfs(connection) = &state.1 {
                                for entry in &mut listing.entries {
                                    *entry = DirEntry::new(
                                        self.public_path(connection, entry.path())
                                            .map_err(convert)?,
                                        entry.attrs().clone(),
                                    );
                                }
                            }
                            let next = next.map(|cursor| {
                                vfsi_sync::DirPageCursor::new(RoutedDirectoryCursor {
                                    route: state.1.clone(),
                                    backend_path: state.2.clone(),
                                    public_path: paths[index].to_path_buf(),
                                    owner: self.owner.clone(),
                                    cursor,
                                })
                            });
                            let children = children
                                .into_iter()
                                .map(|(backend_path, cursor)| {
                                    let public_path = match &state.1 {
                                        Route::Posix => backend_path.clone(),
                                        Route::Nfs(connection) => self
                                            .public_path(connection, &backend_path)
                                            .map_err(convert)?,
                                    };
                                    let seed = vfsi_sync::DirPageCursor::new(
                                        RoutedChildDirectoryCursor(RoutedDirectoryCursor {
                                            route: state.1.clone(),
                                            backend_path,
                                            public_path: public_path.clone(),
                                            owner: self.owner.clone(),
                                            cursor,
                                        }),
                                    );
                                    Ok((public_path, seed))
                                })
                                .collect::<Result<Vec<_>, vfsi_core::OpError>>()?;
                            Ok((listing, next, children))
                        })
                        .map_err(|error| error.with_context("vlistdir_pages", paths[index])),
                );
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("all routed page inputs assigned"))
            .collect())
    }

    pub(crate) fn read_files_native<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::ReadAllOptions,
    ) -> vfsi_core::VecResult<Vec<u8>> {
        let mounts = read_mounts(false);
        let resolved: Vec<_> = paths
            .iter()
            .map(|path| self.resolve(path.as_ref(), &mounts))
            .collect();
        let mut output = Vec::with_capacity(paths.len());
        let mut remaining = options.max_total_bytes();
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
            let options = crate::ReadAllOptions::new().with_max_total_bytes(remaining);
            let buffers = match &resolved[start].route {
                Route::Posix => self.mounted.read_files_native(&batch, options),
                Route::Nfs(connection) => connection.client.read_files_native(&batch, options),
            };
            for (index, outcome) in route_results(buffers, end - start).into_iter().enumerate() {
                let bytes = match &outcome {
                    Ok(data) => data.len(),
                    Err(error) => match error.progress() {
                        Some(vfsi_core::IoProgress::Read(prefix)) => prefix.bytes_read(),
                        _ => 0,
                    },
                };
                if bytes > remaining {
                    output.push(Err(vfsi_core::OpError::unknown(
                        VfError::invalid_reply(None, "routed whole-file byte budget exceeded"),
                        None,
                    )));
                } else {
                    remaining -= bytes;
                    output.push(outcome.map_err(|error| {
                        error.with_context("vread", paths[start + index].as_ref())
                    }));
                }
            }
            start = end;
        }
        Ok(output)
    }

    pub(crate) fn vopen_impl(&self, requests: &[OpenOp]) -> vfsi_core::VecResult<AutoFile> {
        vfsi_core::internal::validate_open_requests(requests)?;
        let mounts = read_mounts(true);
        let resolved = self.resolve_open_batch(requests, &mounts);
        self.vopen_resolved(requests, &resolved)
    }

    pub(super) fn vopen_resolved(
        &self,
        requests: &[OpenOp],
        resolved: &[Resolved],
    ) -> vfsi_core::VecResult<AutoFile> {
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
            macro_rules! route {
                ($client:expr, $variant:ident) => {
                    route_results($client.vopen(&batch), batch.len())
                        .into_iter()
                        .enumerate()
                        .map(|(relative, outcome)| {
                            let index = start + relative;
                            outcome
                                .map(|file| {
                                    AutoFile::new(
                                        requests[index].path().to_path_buf(),
                                        resolved[index].route.clone(),
                                        AutoFileInner::$variant(file),
                                        &self.owner,
                                    )
                                })
                                .map_err(|error| {
                                    error.with_context("vopen", requests[index].path())
                                })
                        })
                };
            }
            match &resolved[start].route {
                Route::Posix => output.extend(route!(self.mounted, Posix)),
                Route::Nfs(connection) => output.extend(route!(connection.client, Nfs)),
            }
            start = end;
        }
        Ok(output)
    }

    fn check_owner(&self, file: &AutoFile, index: usize) -> VfResult<()> {
        if !Arc::ptr_eq(&self.owner, &file.owner) {
            return Err(VfError::from_kind(index, std::io::ErrorKind::InvalidInput));
        }
        file.check_credentials()
            .map_err(|error| error.with_index(index))
    }

    /// Consume a batch with an explicit aggregate read budget.
    pub(crate) fn vread_impl<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, AutoFile>>,
        options: crate::ReadOptions,
    ) -> vfsi_core::VecResult<crate::ReadResult> {
        crate::read::consume_ops(
            ops,
            options.max_total_bytes_or(self.limits.max_read_bytes()),
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
    ) -> vfsi_core::VecResult<ReadResult> {
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
                self.read_files_native(
                    paths,
                    crate::ReadAllOptions::new().with_max_total_bytes(bytes),
                )
            },
        )
    }

    fn vread_with_limit_projected_native<'a, T>(
        &self,
        requests: &[T],
        max_bytes: usize,
        project: impl for<'r> Fn(&'r T) -> &'r AutoRead<'a>,
    ) -> vfsi_core::VecResult<ReadResult> {
        use vfsi_core::OpError;
        let mut requested = 0usize;
        let mut output: Vec<_> = (0..requests.len()).map(|_| None).collect();
        for (index, request) in requests.iter().map(&project).enumerate() {
            requested = requested
                .checked_add(request.length)
                .filter(|bytes| *bytes <= max_bytes)
                .ok_or_else(|| VfError::errno(index, libc::EFBIG as u32))?;
            if let Err(error) = self.check_owner(request.file, index) {
                output[index] = Some(Err(
                    OpError::not_attempted(error).with_context("vread", &request.file.path)
                ));
            }
        }
        let mut start = 0;
        while start < requests.len() {
            let route = &project(&requests[start]).file.route;
            let mut end = start + 1;
            while end < requests.len() && route.same_backend(&project(&requests[end]).file.route) {
                end += 1;
            }
            let valid = || {
                requests[start..end]
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| output[start + index].is_none())
                    .map(|(_, request)| project(request))
            };
            let results = match route {
                Route::Posix => {
                    let batch: Vec<_> = valid()
                        .map(|request| {
                            let AutoFileInner::Posix(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at(request.offset, request.length)
                        })
                        .collect();
                    if batch.is_empty() {
                        Ok(Vec::new())
                    } else {
                        self.mounted.vread_with_limit_native(&batch, max_bytes)
                    }
                }
                Route::Nfs(connection) => {
                    let batch: Vec<_> = valid()
                        .map(|request| {
                            let AutoFileInner::Nfs(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at(request.offset, request.length)
                        })
                        .collect();
                    if batch.is_empty() {
                        Ok(Vec::new())
                    } else {
                        connection.client.vread_with_limit_native(&batch, max_bytes)
                    }
                }
            };
            let count = output[start..end]
                .iter()
                .filter(|slot| slot.is_none())
                .count();
            let mut results = route_results(results, count).into_iter();
            for (index, slot) in output[start..end].iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(results.next().expect("validated read route count").map_err(
                        |error| {
                            error
                                .with_context("vread", &project(&requests[start + index]).file.path)
                        },
                    ));
                }
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every routed read input assigned"))
            .collect())
    }

    fn vread_into_with_limit_native(
        &self,
        requests: &mut [AutoReadInto<'_>],
        max_bytes: usize,
    ) -> vfsi_core::VecResult<ReadIntoResult> {
        use vfsi_core::OpError;
        let mut requested = 0usize;
        let mut output: Vec<_> = (0..requests.len()).map(|_| None).collect();
        for (index, request) in requests.iter().enumerate() {
            requested = requested
                .checked_add(request.buffer.len())
                .filter(|bytes| *bytes <= max_bytes)
                .ok_or_else(|| VfError::errno(index, libc::EFBIG as u32))?;
            if let Err(error) = self.check_owner(request.file, index) {
                output[index] = Some(Err(
                    OpError::not_attempted(error).with_context("vread", &request.file.path)
                ));
            }
        }
        let mut start = 0;
        while start < requests.len() {
            let route = requests[start].file.route.clone();
            let mut end = start + 1;
            while end < requests.len() && route.same_backend(&requests[end].file.route) {
                end += 1;
            }
            let results = match &route {
                Route::Posix => {
                    let mut batch: Vec<_> = requests[start..end]
                        .iter_mut()
                        .enumerate()
                        .filter(|(index, _)| output[start + index].is_none())
                        .map(|(_, request)| {
                            let AutoFileInner::Posix(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at_into(request.offset, &mut *request.buffer)
                        })
                        .collect();
                    if batch.is_empty() {
                        Ok(Vec::new())
                    } else {
                        self.mounted
                            .vread_into_with_limit_native(&mut batch, max_bytes)
                    }
                }
                Route::Nfs(connection) => {
                    let mut batch: Vec<_> = requests[start..end]
                        .iter_mut()
                        .enumerate()
                        .filter(|(index, _)| output[start + index].is_none())
                        .map(|(_, request)| {
                            let AutoFileInner::Nfs(file) = &request.file.inner else {
                                unreachable!()
                            };
                            file.read_request_at_into(request.offset, &mut *request.buffer)
                        })
                        .collect();
                    if batch.is_empty() {
                        Ok(Vec::new())
                    } else {
                        connection
                            .client
                            .vread_into_with_limit_native(&mut batch, max_bytes)
                    }
                }
            };
            let count = output[start..end]
                .iter()
                .filter(|slot| slot.is_none())
                .count();
            let mut results = route_results(results, count).into_iter();
            for (index, slot) in output[start..end].iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(
                        results
                            .next()
                            .expect("validated buffered route count")
                            .map_err(|error| {
                                error.with_context("vread", &requests[start + index].file.path)
                            }),
                    );
                }
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every routed buffer assigned"))
            .collect())
    }

    pub(crate) fn write_partial_native(
        &self,
        requests: &[crate::WriteOp<'_, AutoFile>],
    ) -> vfsi_core::VecResult<WriteResult> {
        self.write_vector(requests, false)
    }

    pub(crate) fn write_complete(
        &self,
        requests: &[crate::WriteOp<'_, AutoFile>],
    ) -> vfsi_core::VecResult<WriteResult> {
        self.write_vector(requests, true)
    }

    fn write_vector<'a>(
        &self,
        requests: &[crate::WriteOp<'a, AutoFile>],
        complete: bool,
    ) -> vfsi_core::VecResult<WriteResult> {
        use vfsi_core::OpError;
        let mut output: Vec<_> = requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                self.check_owner(request.file(), index).err().map(|cause| {
                    Err(OpError::not_attempted(cause)
                        .with_context("vwrite_native", &request.file().path))
                })
            })
            .collect();
        let mut start = 0;
        while start < requests.len() {
            let route = &requests[start].file().route;
            let mut end = start + 1;
            while end < requests.len() && route.same_backend(&requests[end].file().route) {
                end += 1;
            }
            let batch: Vec<_> = requests[start..end]
                .iter()
                .enumerate()
                .filter(|(index, _)| output[start + index].is_none())
                .map(|(_, request)| request)
                .collect();
            let count = batch.len();
            macro_rules! dispatch {
                ($client:expr, $variant:ident) => {{
                    let project = |request: &&crate::WriteOp<'a, AutoFile>| {
                        let AutoFileInner::$variant(file) = &request.file().inner else {
                            unreachable!("validated route")
                        };
                        file.write_request_at(request.offset(), request.data())
                    };
                    if complete {
                        $client.vwrite_all_mapped_native(&batch, project)
                    } else {
                        $client.vwrite_mapped_native(&batch, project)
                    }
                }};
            }
            let results = if batch.is_empty() {
                Ok(Vec::new())
            } else {
                match route {
                    Route::Posix => dispatch!(self.mounted, Posix),
                    Route::Nfs(connection) => dispatch!(connection.client, Nfs),
                }
            };
            let mut results = route_results(results, count).into_iter();
            for (index, slot) in output[start..end].iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(
                        results
                            .next()
                            .expect("one outcome per routed input")
                            .map_err(|error| {
                                error.with_context(
                                    "vwrite_native",
                                    &requests[start + index].file().path,
                                )
                            }),
                    );
                }
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every write assigned"))
            .collect())
    }

    /// Close independent cohorts, retaining every per-input outcome.
    pub(crate) fn vclose_impl(&self, files: &mut [AutoFile]) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        let mut output: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(index, file)| match self.check_owner(file, index) {
                Err(error) => Some(Err(
                    OpError::not_attempted(error).with_context("vclose", &file.path)
                )),
                Ok(()) if file.is_closed() => Some(Ok(())),
                Ok(()) => None,
            })
            .collect();
        let mut start = 0;
        while start < files.len() {
            let route = files[start].route.clone();
            let mut end = start + 1;
            while end < files.len() && route.same_backend(&files[end].route) {
                end += 1;
            }
            macro_rules! batch {
                ($variant:ident) => {{
                    files[start..end]
                        .iter_mut()
                        .enumerate()
                        .filter(|(index, _)| output[start + index].is_none())
                        .map(|(_, file)| match &mut file.inner {
                            AutoFileInner::$variant(inner) => inner,
                            _ => unreachable!("validated route"),
                        })
                }};
            }
            let count = output[start..end]
                .iter()
                .filter(|slot| slot.is_none())
                .count();
            let result = match &route {
                Route::Posix => self.mounted.vclose(batch!(Posix)),
                Route::Nfs(connection) => connection.client.vclose(batch!(Nfs)),
            };
            let mut outcomes = route_results(result, count).into_iter();
            for (index, slot) in output[start..end].iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(
                        outcomes
                            .next()
                            .expect("validated close count")
                            .map_err(|error| {
                                error.with_context("vclose", &files[start + index].path)
                            }),
                    );
                }
            }
            start = end;
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every close route assigned"))
            .collect())
    }

    /// Rename adjacent pairs on the same backend as a vector, preserving order.
    /// Options are atomic per pair; unsupported semantics are never emulated.
    pub(crate) fn vrename_impl<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: crate::RenameMode,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mounts = read_mounts(false);
        let sources: Vec<_> = pairs
            .iter()
            .map(|(from, _)| self.resolve_entry(from.as_ref(), &mounts))
            .collect();
        let targets: Vec<_> = pairs
            .iter()
            .map(|(_, to)| self.resolve_entry(to.as_ref(), &mounts))
            .collect();
        let mut slots = (0..pairs.len()).map(|_| None).collect::<Vec<_>>();
        for (index, (source, target)) in sources.iter().zip(&targets).enumerate() {
            let cause = if !source.route.same_backend(&target.route) {
                Some(VfError::errno(None, libc::EXDEV as u32))
            } else if options != crate::RenameMode::Replace && !matches!(source.route, Route::Posix)
            {
                Some(VfError::unsupported(None))
            } else {
                None
            };
            if let Some(cause) = cause {
                slots[index] =
                    Some(Err(OpError::not_attempted(cause)
                        .with_context("vrename", pairs[index].0.as_ref())));
            }
        }
        let mut start = 0;
        while start < pairs.len() {
            let end = cohort_end(&sources, start);
            let indices: Vec<_> = (start..end)
                .filter(|&index| slots[index].is_none())
                .collect();
            let batch: Vec<_> = indices
                .iter()
                .map(|&index| (sources[index].path.as_path(), targets[index].path.as_path()))
                .collect();
            let result = if batch.is_empty() {
                Ok(Vec::new())
            } else {
                match &sources[start].route {
                    Route::Posix => self.mounted.vrename(&batch, options),
                    Route::Nfs(connection) => connection.client.vrename(&batch, options),
                }
            };
            for (index, result) in indices
                .iter()
                .copied()
                .zip(route_results(result, indices.len()))
            {
                slots[index] = Some(
                    result.map_err(|error| error.with_context("vrename", pairs[index].0.as_ref())),
                );
            }
            start = end;
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all rename routes assigned"))
            .collect())
    }
}

fn cohort_end(resolved: &[Resolved], start: usize) -> usize {
    let mut end = start + 1;
    while end < resolved.len() && resolved[start].route.same_backend(&resolved[end].route) {
        end += 1;
    }
    end
}

impl super::Auto {
    pub(crate) fn vlistdir_pages_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
        cursors: Vec<Option<vfsi_core::api::DirectoryCursor>>,
        options: vfsi_core::api::ListDirOptions,
        page_size: std::num::NonZeroUsize,
    ) -> vfsi_core::VecResult<vfsi_core::api::DirectoryPage> {
        let options = options.resolve(self.limits);
        vfsi_core::api::internal::native_directory_pages(
            &self.owner,
            paths,
            cursors,
            options,
            page_size,
            |paths| self.directory_page_batch_size(paths),
            |paths, states, size, probe| {
                self.read_dir_pages_with_fields(
                    paths,
                    options.fields(),
                    states,
                    size,
                    probe,
                    options.allow_symlink_components(),
                )
            },
        )
    }
}

fn route_results<T>(result: vfsi_core::VecResult<T>, count: usize) -> Vec<vfsi_core::OpResult<T>> {
    vfsi_core::internal::vec_result_slots(result, count)
}
