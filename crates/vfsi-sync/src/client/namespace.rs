//! Namespace mutation and copy dispatch.

use super::*;

impl<F: VectorBackend> FsClient<F> {
    /// Rename independent source/destination pairs in one vector phase.
    /// Requested atomic destination semantics are applied per pair; the vector
    /// is not transactional. Unsupported semantics are never emulated.
    pub(crate) fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::RenameMode,
    ) -> vfsi_core::VecResult<()> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::with_capacity(pairs.len());
        let requests: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                let source = from.as_ref();
                let destination = to.as_ref();
                paths.push(source);
                (
                    FileOperand::from_os_path(source),
                    FileOperand::from_os_path(destination),
                )
            })
            .collect();
        namespace_results(
            self.lock()?.vrename_with_options_impl(&requests, options),
            &paths,
            "vrename",
        )
    }

    /// Create independent directories, isolating duplicate-input rejections.
    pub(crate) fn vmkdir<P: AsRef<Path>>(
        &self,
        directories: &[vfsi_core::MkDirOp<P>],
    ) -> vfsi_core::VecResult<()> {
        use vfsi_core::OpError;
        if directories.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = directories.iter().map(|op| op.path()).collect();
        let mut seen = HashSet::with_capacity(paths.len());
        let mut raw = Vec::with_capacity(paths.len());
        let mut indices = Vec::new();
        let mut rejected = Vec::new();
        for (index, (op, path)) in directories.iter().zip(&paths).enumerate() {
            if !seen.insert(*path) {
                if rejected.is_empty() {
                    indices.extend(0..index);
                }
                rejected.push((
                    index,
                    OpError::not_attempted(VfError::from_kind(
                        None,
                        std::io::ErrorKind::InvalidInput,
                    ))
                    .with_context("vmkdir", path),
                ));
            } else {
                if !rejected.is_empty() {
                    indices.push(index);
                }
                raw.push(crate::VfAttrs {
                    file: FileOperand::from_os_path(path),
                    masks: AttrMask::MODE,
                    mode: op.mode(),
                    ..Default::default()
                });
            }
        }
        let result = self.lock()?.vmkdir_impl(&raw);
        if rejected.is_empty() {
            return namespace_results(result, &paths, "vmkdir");
        }
        let active_paths: Vec<_> = indices.iter().map(|&index| paths[index]).collect();
        let result = match result {
            Ok(result) => Ok(result),
            Err(cause) => {
                let cause = link_error(cause, &active_paths, "vmkdir");
                Ok(raw
                    .iter()
                    .map(|_| Err(OpError::not_attempted(cause.clone())))
                    .collect())
            }
        };
        let outcomes = namespace_results(result, &active_paths, "vmkdir")?;
        let mut slots = (0..paths.len()).map(|_| None).collect::<Vec<_>>();
        for (index, error) in rejected {
            slots[index] = Some(Err(error));
        }
        for (index, result) in indices.into_iter().zip(outcomes) {
            slots[index] = Some(result);
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("all directory inputs assigned"))
            .collect())
    }

    /// Create independent symbolic links in one native vector phase.
    pub(crate) fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> vfsi_core::VecResult<()> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let targets: Vec<_> = pairs.iter().map(|(target, _)| target.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        namespace_results(
            self.lock()?.vsymlink_impl(&targets, &links),
            &links,
            "vsymlink",
        )
    }

    /// Read independent targets without converting Unix path bytes through UTF-8.
    pub(crate) fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> vfsi_core::VecResult<PathBuf> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<_> = paths.iter().map(AsRef::as_ref).collect();
        namespace_results(self.lock()?.vreadlink_impl(&paths), &paths, "vreadlink").map(|results| {
            results
                .into_iter()
                .map(|result| result.map(crate::backend::bytes_to_path))
                .collect()
        })
    }

    /// Create independent hard links in one native vector phase.
    pub(crate) fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> vfsi_core::VecResult<()> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let sources: Vec<_> = pairs.iter().map(|(source, _)| source.as_ref()).collect();
        let links: Vec<_> = pairs.iter().map(|(_, link)| link.as_ref()).collect();
        namespace_results(
            self.lock()?.vhardlink_impl(&sources, &links),
            &links,
            "vhardlink",
        )
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Copy independent whole-file pairs, retaining one outcome per input.
    /// Sibling copies may succeed around failures; this operation is not atomic.
    pub(crate) fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: vfsi_core::api::CopyOptions,
    ) -> vfsi_core::VecResult<()> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let extents: Vec<_> = pairs
            .iter()
            .map(|(from, to)| {
                crate::ExtentPair::from_os_paths(from.as_ref(), 0, to.as_ref(), 0, None)
            })
            .collect();
        let paths: Vec<_> = extents.iter().map(|pair| pair.dst_path.as_path()).collect();
        namespace_results(self.lock()?.vcopy_impl(&extents, options), &paths, "vcopy")
    }
}

// Namespace and metadata operations cannot report byte progress. Preserve independently
// validated siblings, and attach each failure to the caller's original path.
pub(super) fn namespace_results<T>(
    result: vfsi_core::VecResult<T>,
    paths: &[&Path],
    operation: &'static str,
) -> vfsi_core::VecResult<T> {
    use vfsi_core::OpError;
    let mut outcomes = result.map_err(|error| link_error(error, paths, operation))?;
    if outcomes.len() > paths.len() {
        let cause = VfError::invalid_reply(None, format!("too many {operation} outcomes"));
        outcomes = paths
            .iter()
            .map(|_| Err(OpError::unknown(cause.clone(), None)))
            .collect();
    }
    outcomes.resize_with(paths.len(), || {
        Err(OpError::unknown(
            VfError::invalid_reply(None, format!("missing {operation} outcome")),
            None,
        ))
    });
    Ok(outcomes
        .into_iter()
        .zip(paths)
        .map(|(result, path)| {
            result.map_err(|error| {
                let error = if error.progress().is_some() {
                    OpError::unknown(
                        VfError::invalid_reply(
                            None,
                            format!("{operation} cannot report I/O byte progress"),
                        ),
                        None,
                    )
                } else {
                    error
                };
                error.with_context(operation, path)
            })
        })
        .collect())
}
