//! Directory traversal and retained-directory operations.

use super::*;

impl<F: VectorBackend> FsClient<F> {
    fn removal_metadata(&self, path: &Path, operation: &'static str) -> VfResult<Attrs> {
        let mut filesystem = self.lock()?;
        let follow = !filesystem.capabilities().contains(Capabilities::LSTAT);
        filesystem
            .metadata_path_impl(path, follow)
            .map_err(|error| error.with_context(operation, path))
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Lazy between directories, with selective no-follow metadata and pruning.
    /// Enter runs before any listing; Leave follows even a pruned directory.
    /// Sorting buffers one bounded directory, not the whole tree. The callback
    /// runs outside the backend lock. Limits include the starting object.
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
    ) -> VfResult<Vec<crate::DirectoryPage>> {
        let pages = self.lock()?.vlistdir_pages_impl(
            paths,
            fields | AttrMask::MODE | AttrMask::SIZE,
            cursors,
            page_size,
            max_entries,
            follow_symlinks,
        )?;
        if pages.len() != paths.len() {
            return Err(VfError::transport(
                None,
                "directory pages returned an invalid result count",
            ));
        }
        pages
            .into_iter()
            .zip(paths)
            .enumerate()
            .map(|(index, ((attrs, next, children), path))| {
                if children.len() != attrs.len()
                    || attrs.len() > page_size
                    || (attrs.is_empty() && next.is_some())
                {
                    return Err(VfError::transport(
                        Some(index),
                        "invalid directory page progress",
                    ));
                }
                let mut seeds = Vec::new();
                let entries = attrs
                    .into_iter()
                    .zip(children)
                    .map(|(attrs, child)| {
                        let entry_path = attrs
                            .file
                            .path()
                            .ok_or_else(|| {
                                VfError::transport(Some(index), "directory entry has no path")
                            })?
                            .to_path_buf();
                        if entry_path.parent() != Some(*path) {
                            return Err(VfError::transport(
                                Some(index),
                                "directory entry is outside its parent",
                            ));
                        }
                        if let Some(child) = child {
                            seeds.push((entry_path.clone(), child));
                        }
                        Ok(DirEntry::new(
                            entry_path,
                            vfsi_core::metadata_from_attrs(attrs),
                        ))
                    })
                    .collect::<VfResult<Vec<_>>>()?;
                Ok((
                    DirectoryListing {
                        path: path.to_path_buf(),
                        entries,
                    },
                    next,
                    seeds,
                ))
            })
            .collect()
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Create `path` if missing, otherwise empty it. Errors if it exists and is
    /// not a directory (a symlink to a directory is not a directory here).
    pub fn ensure_empty_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.ensure_empty_dir_impl(path.as_ref())
    }

    /// Empty a directory while keeping it, with explicit removal policy.
    pub(crate) fn remove_dir_contents_impl(
        &self,
        path: impl AsRef<Path>,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let path = path.as_ref();
        if !self
            .removal_metadata(path, "remove_dir_contents_impl")?
            .is_dir()
        {
            return Err(
                VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir_contents", path)
            );
        }
        self.lock()?
            .remove_dir_contents_path_with_options_impl(path, options)
            .map_err(|error| error.with_context("remove_dir_contents", path))
    }

    pub(crate) fn vopen_dirs_impl<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<FsDir<F>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut backend = self.lock()?;
        let mut output = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            let path = path.as_ref();
            let dir = backend.open_dir_impl(path).map_err(|error| {
                crate::application::vector_index(error, index).with_context("vopen_dirs", path)
            })?;
            if !matches!(dir, VfDir::Descriptor { .. }) {
                let _ = backend.close_dir_impl(&dir);
                return Err(VfError::unsupported(index).with_context("vopen_dirs", path));
            }
            output.push(FsDir {
                inner: Arc::clone(&self.inner),
                dir: Some(dir),
                path: path.to_path_buf(),
            });
        }
        Ok(output)
    }

    /// Preflight the entire vector before acquiring the mutation lock.
    pub(crate) fn vremove_dir_contents_impl(
        &self,
        dirs: &[&FsDir<F>],
        options: RemoveOptions,
    ) -> VfResult<()> {
        for (index, dir) in dirs.iter().enumerate() {
            if !Arc::ptr_eq(&self.inner, &dir.inner) || dir.is_closed() {
                return Err(VfError::client(index, crate::ERR_EBADF)
                    .with_context("vremove_dir_contents", &dir.path));
            }
        }
        if dirs.is_empty() {
            return Ok(());
        }
        let mut backend = self.lock()?;
        let mut first_error = None;
        for (index, dir) in dirs.iter().enumerate() {
            if let Err(error) = backend.remove_dir_contents_handle_with_options_impl(
                dir.dir.as_ref().expect("preflighted"),
                options,
            ) {
                let error = crate::application::vector_index(error, index)
                    .with_context("vremove_dir_contents", &dir.path);
                if error.is_transport() || !options.continues_on_error() {
                    return Err(error);
                }
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Remove paths in request order, optionally recursing into directories.
    /// A successful prefix may remain if a later path fails.
    #[doc(hidden)]
    pub fn vremove_native<P: AsRef<Path>>(&self, paths: &[P], recursive: bool) -> VfResult<()> {
        self.vremove_impl(paths, recursive, RemoveOptions::default())
    }

    /// Remove paths with explicit error, batching, and retry policy.
    #[doc(hidden)]
    pub fn vremove_impl<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
        options: RemoveOptions,
    ) -> VfResult<()> {
        let paths: Vec<&Path> = paths.iter().map(AsRef::as_ref).collect();
        self.lock()?
            .remove_paths_with_options_impl(&paths, recursive, options)
            .map_err(|error| {
                error
                    .index()
                    .and_then(|index| paths.get(index))
                    .map_or(error.clone(), |path| {
                        error.with_context("vremove_native", path)
                    })
            })
    }
}
