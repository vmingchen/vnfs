//! Directory traversal and retained-directory operations.

use super::*;

impl<F: VectorBackend> FsClient<F> {
    /// Maximum safe cohort size for incremental directory paging.
    #[doc(hidden)]
    pub fn directory_page_batch_size(&self) -> VfResult<usize> {
        Ok(self.lock()?.directory_page_batch_size().clamp(1, 32))
    }

    /// Fetch a bounded vector of directory pages, retaining backend cursors.
    #[doc(hidden)]
    pub fn read_dir_pages_with_fields(
        &self,
        paths: &[&Path],
        fields: AttrMask,
        cursors: Vec<Option<crate::DirPageCursor>>,
        page_size: usize,
        max_entries: usize,
        follow_symlinks: bool,
    ) -> vfsi_core::VecResult<crate::DirectoryPage> {
        use vfsi_core::OpError;
        if paths.len() != cursors.len() || page_size == 0 {
            return Err(VfError::from_kind(None, std::io::ErrorKind::InvalidInput));
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let pages = super::namespace::namespace_results(
            self.lock()?.vlistdir_pages_impl(
                paths,
                fields | AttrMask::MODE,
                cursors,
                page_size,
                max_entries,
                follow_symlinks,
            ),
            paths,
            "vlistdir_pages",
        )?;
        Ok(pages
            .into_iter()
            .zip(paths)
            .map(|(result, path)| {
                result.and_then(|(attrs, next, children)| {
                    let malformed = |message| {
                        OpError::unknown(VfError::invalid_reply(None, message), None)
                            .with_context("vlistdir_pages", path)
                    };
                    if children.len() != attrs.len()
                        || attrs.len() > page_size
                        || (attrs.is_empty() && next.is_some())
                    {
                        return Err(malformed("invalid directory page progress"));
                    }
                    let mut seeds = Vec::new();
                    let mut seeded = HashSet::new();
                    let entries = attrs
                        .into_iter()
                        .zip(children)
                        .map(|(attrs, child)| {
                            let entry_path = attrs
                                .file
                                .path()
                                .ok_or_else(|| malformed("directory entry has no path"))?
                                .to_path_buf();
                            if entry_path.parent() != Some(*path) {
                                return Err(malformed("directory entry is outside its parent"));
                            }
                            if let Some(child) = child {
                                if attrs.ftype != crate::VfType::Directory
                                    || !seeded.insert(entry_path.clone())
                                {
                                    return Err(malformed("invalid child directory seed"));
                                }
                                seeds.push((entry_path.clone(), child));
                            }
                            Ok(DirEntry::new(
                                entry_path,
                                vfsi_core::metadata_from_attrs(attrs),
                            ))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok((
                        DirectoryListing {
                            path: path.to_path_buf(),
                            entries,
                        },
                        next,
                        seeds,
                    ))
                })
            })
            .collect())
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Create `path` if missing, otherwise empty it. Errors if it exists and is
    /// not a directory (a symlink to a directory is not a directory here).
    pub fn ensure_empty_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        self.lock()?.ensure_empty_dir_impl(path)
    }

    pub(crate) fn vopen_dirs_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
    ) -> vfsi_core::VecResult<FsDir<F>> {
        use vfsi_core::OpError;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        let mut backend = self.lock()?;
        let mut results = backend
            .vopen_dirs_impl(&paths)
            .map_err(|cause| link_error(cause, &paths, "vopen_dirs"))?;
        let own = |dir, index: usize| FsDir {
            inner: Arc::clone(&self.inner),
            dir: Some(dir),
            path: paths
                .get(index)
                .map(|path| path.to_path_buf())
                .unwrap_or_default(),
        };
        let identity = |dir: &VfDir| match dir {
            VfDir::Descriptor { fd, owner } if *fd >= 0 => Some((*owner, *fd)),
            _ => None,
        };
        let mut previous = None;
        let increasing = results
            .iter()
            .filter_map(|result| result.as_ref().ok().and_then(identity))
            .all(|id| {
                let increasing = previous.is_none_or(|previous| previous < id);
                previous = Some(id);
                increasing
            });
        if !increasing {
            // Malformed duplicate resources have one cleanup owner. Neither
            // duplicate slot may escape as an independently owned handle.
            let mut seen = HashMap::new();
            for index in 0..results.len() {
                let Some(id) = results[index].as_ref().ok().and_then(identity) else {
                    continue;
                };
                if let Some(&first) = seen.get(&id) {
                    let cause = VfError::invalid_reply(None, "duplicate directory descriptor");
                    if let Ok(dir) = std::mem::replace(
                        &mut results[first],
                        Err(OpError::unknown(cause.clone(), None)),
                    ) {
                        drop(own(dir, first));
                    }
                    results[index] = Err(OpError::unknown(cause, None));
                } else {
                    seen.insert(id, index);
                }
            }
        }
        let owned = results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.and_then(|dir| match dir {
                    VfDir::Descriptor { fd, .. } if fd >= 0 => Ok(own(dir, index)),
                    VfDir::Path(_) => {
                        drop(own(dir, index));
                        Err(OpError::not_attempted(VfError::unsupported(None)))
                    }
                    _ => Err(OpError::unknown(
                        VfError::invalid_reply(None, "invalid directory descriptor"),
                        None,
                    )),
                })
            })
            .collect();
        let output = super::namespace::namespace_results(Ok(owned), &paths, "vopen_dirs");
        drop(backend);
        output
    }

    /// Isolate invalid owners before one native mutation dispatch.
    pub(crate) fn vremove_dir_contents_impl(
        &self,
        dirs: &[&FsDir<F>],
        options: RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if dirs.is_empty() {
            return Ok(Vec::new());
        }
        let mut slots: Vec<_> = (0..dirs.len()).map(|_| None).collect();
        let mut native = Vec::new();
        let mut indices = Vec::new();
        let mut paths = Vec::new();
        for (index, dir) in dirs.iter().enumerate() {
            if !Arc::ptr_eq(&self.inner, &dir.inner) || dir.is_closed() {
                slots[index] = Some(Err(OpError::not_attempted(VfError::errno(
                    None,
                    crate::ERR_EBADF,
                ))
                .with_context("vremove_dir_contents", &dir.path)));
            } else {
                native.push(dir.dir.as_ref().expect("live owned directory"));
                indices.push(index);
                paths.push(dir.path.as_path());
            }
        }
        if !native.is_empty() {
            let results = super::namespace::namespace_results(
                self.lock()?
                    .remove_dir_contents_handles_with_options_impl(&native, options),
                &paths,
                "vremove_dir_contents",
            )
            .unwrap_or_else(|cause| {
                native
                    .iter()
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect()
            });
            for (index, result) in indices.into_iter().zip(results) {
                slots[index] = Some(result);
            }
        }
        Ok(slots.into_iter().map(Option::unwrap).collect())
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Remove independent paths, optionally recursing into directories.
    /// Outcomes retain input positions; completion does not imply a prefix.
    #[doc(hidden)]
    pub fn vremove_native<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
    ) -> vfsi_core::VecResult<()> {
        self.vremove_impl(
            paths,
            if recursive {
                vfsi_core::api::RemoveMode::Tree
            } else {
                vfsi_core::api::RemoveMode::Entry
            },
            RemoveOptions::default(),
        )
    }

    /// Remove paths with explicit error, batching, and retry policy.
    #[doc(hidden)]
    pub fn vremove_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
        mode: vfsi_core::api::RemoveMode,
        options: RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        super::namespace::namespace_results(
            self.lock()?
                .remove_paths_with_options_impl(&paths, mode, options),
            &paths,
            "vremove",
        )
    }
}

impl<F: VectorBackend + 'static> FsClient<F> {
    /// Portable owned paging with complete client/path ownership preflight.
    #[doc(hidden)]
    pub fn vlistdir_pages_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
        cursors: Vec<Option<vfsi_core::api::DirectoryCursor>>,
        options: vfsi_core::api::ListDirOptions,
        page_size: std::num::NonZeroUsize,
    ) -> vfsi_core::VecResult<vfsi_core::api::DirectoryPage> {
        let options = options.resolve(self.limits());
        vfsi_core::api::internal::native_directory_pages(
            &self.inner.cursor_owner,
            paths,
            cursors,
            options,
            page_size,
            |_| self.directory_page_batch_size(),
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
