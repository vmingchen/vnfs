//! Namespace mutation and copy dispatch.

use super::*;

impl<F: VectorBackend> FsClient<F> {
    /// Rename independent source/destination pairs in one vector phase.
    /// Requested atomic destination semantics are applied per pair; the vector
    /// is not transactional. Unsupported semantics are never emulated.
    pub(crate) fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::RenameOptions,
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let requests: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                (
                    VfFile::from_os_path(from.as_ref()),
                    VfFile::from_os_path(to.as_ref()),
                )
            })
            .collect();
        self.lock()?
            .vrename_with_options_impl(&requests, options)
            .map_err(|error| match error.index() {
                Some(index) if index < pairs.len() => {
                    error.with_context("vrename", pairs[index].0.as_ref())
                }
                Some(_) => {
                    VfError::transport(None, "rename backend returned an invalid error index")
                }
                None => error,
            })
    }

    /// Create directories with per-request Unix permission bits.
    pub(crate) fn vmkdir<P: AsRef<Path>>(
        &self,
        directories: &[vfsi_core::MkDirOp<P>],
    ) -> VfResult<()> {
        let mut seen = HashSet::with_capacity(directories.len());
        for (index, op) in directories.iter().enumerate() {
            if !seen.insert(op.path()) {
                return Err(
                    VfError::client(index, crate::ERR_INVAL).with_context("vmkdir", op.path())
                );
            }
        }
        if directories.is_empty() {
            return Ok(());
        }
        let dirs: Vec<_> = directories
            .iter()
            .map(|op| crate::VfAttrs {
                file: VfFile::from_os_path(op.path()),
                masks: AttrMask::MODE,
                mode: op.mode(),
                ..Default::default()
            })
            .collect();
        self.lock()?
            .vmkdir_impl(&dirs)
            .map_err(|error| match error.index() {
                Some(index) if index < directories.len() => {
                    error.with_context("vmkdir", directories[index].path())
                }
                Some(_) => {
                    VfError::transport(None, "mkdir backend returned an invalid error index")
                }
                None => error,
            })
    }

    /// Create symbolic links in one native backend vector.
    pub(crate) fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let targets: Vec<_> = pairs.iter().map(|(target, _)| target.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        self.lock()?
            .vsymlink_impl(&targets, &links)
            .map_err(|error| link_error(error, &links, "vsymlink"))
    }

    /// Read targets without converting Unix path bytes through UTF-8.
    pub(crate) fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        let targets = self
            .lock()?
            .vreadlink_impl(&paths)
            .map_err(|error| link_error(error, &paths, "vreadlink"))?;
        if targets.len() != paths.len() {
            return Err(VfError::transport(
                None,
                "readlink backend returned an invalid result count",
            ));
        }
        Ok(targets
            .into_iter()
            .map(crate::backend::bytes_to_path)
            .collect())
    }

    /// Create hard links in one native backend vector.
    pub(crate) fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> VfResult<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let sources: Vec<_> = pairs.iter().map(|(source, _)| source.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        self.lock()?
            .vhardlink_impl(&sources, &links)
            .map_err(|error| link_error(error, &links, "vhardlink"))
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Copy whole files in request order. A successful prefix may remain if
    /// a later request fails; this operation does not provide atomicity.
    pub(crate) fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::CopyOption,
    ) -> VfResult<()> {
        let extents: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                crate::ExtentPair::from_os_paths(from.as_ref(), 0, to.as_ref(), 0, None)
            })
            .collect();
        self.lock()?.vcopy_impl(&extents, options).map_err(|error| {
            error
                .index()
                .and_then(|index| pairs.get(index))
                .map_or(error.clone(), |(_, to)| {
                    error.with_context("vcopy", to.as_ref())
                })
        })
    }
}
