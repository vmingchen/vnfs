//! Owned, shareable synchronous client and file handles.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfsi_core::api::internal::OwnedReadResult as FsReadResult;
use vfsi_core::api::{
    DirectoryListing, ReadIntoResult as FsReadIntoResult, ResourceLimits,
    WriteResult as FsWriteResult,
};

use crate::backend::{HandleBackend, VectorBackend};
use crate::contracts::{validate_read_into_results, validate_read_results, validate_write_results};
use crate::{
    AttrMask, Attrs, Capabilities, DirEntry, FileOperand, OpenFlags, OpenOp, ReadAllOptions,
    ReadOp, ReadResult, RemoveOptions, VfDir, VfError, VfOffset, VfResult, WriteOp, WriteResult,
};

fn with_indexed_context<'a>(
    error: VfError,
    operation: &'static str,
    path: impl FnOnce(usize) -> Option<&'a Path>,
) -> VfError {
    match error.index() {
        Some(index) => match path(index) {
            Some(path) => error.with_context(operation, path),
            None => VfError::invalid_reply(
                None,
                format!("{operation} backend returned an invalid error index"),
            ),
        },
        None => error,
    }
}

fn read_result(result: ReadResult) -> FsReadResult {
    FsReadResult {
        offset: result.offset,
        data: result.data,
        eof: result.eof,
    }
}

fn write_result(result: WriteResult) -> FsWriteResult {
    FsWriteResult {
        offset: result.offset,
        bytes_written: result.bytes_written,
        stable: result.stable,
    }
}

fn poisoned() -> VfError {
    VfError::errno(0, crate::ERR_IO)
}

/// Cloneable owner of one synchronous backend connection.
/// Clones share a mutex and serialize backend calls; use separate connections
/// for parallel RPCs. Explicitly close handles to observe cleanup failures.
/// Handle Drop queues cleanup, drained before later operations or by
/// `drain_cleanup`. Dropping the final backend owner performs synchronous
/// teardown and may wait for pending closes and backend request timeouts.
pub struct FsClient<F: HandleBackend> {
    inner: Arc<SharedBackend<F>>,
    limits: ResourceLimits,
}

impl<F: HandleBackend> Clone for FsClient<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            limits: self.limits,
        }
    }
}

impl<F: HandleBackend> fmt::Debug for FsClient<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FsClient").finish_non_exhaustive()
    }
}

impl<F: HandleBackend> FsClient<F> {
    pub fn new(filesystem: F) -> Self {
        Self {
            inner: Arc::new(SharedBackend::new(filesystem)),
            limits: ResourceLimits::default(),
        }
    }

    /// Configure this client view and its future clones. Existing clones keep
    /// their policy; all views still share the same connection and ownership.
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }

    fn validate_owner(&self, file: &OpenFile<F>, index: usize) -> VfResult<()> {
        if Arc::ptr_eq(&self.inner, &file.inner) {
            Ok(())
        } else {
            Err(VfError::from_kind(index, std::io::ErrorKind::InvalidInput))
        }
    }

    fn lock(&self) -> VfResult<BackendGuard<'_, F>> {
        self.inner.lock().map_err(|_| poisoned())
    }

    pub fn into_inner(self) -> Result<F, Self> {
        if self.drain_cleanup().is_err() {
            return Err(self);
        }
        let limits = self.limits;
        match Arc::try_unwrap(self.inner) {
            Ok(shared) => shared.into_backend().map_err(|shared| Self {
                inner: Arc::new(shared),
                limits,
            }),
            Err(inner) => Err(Self { inner, limits }),
        }
    }

    /// Attempt queued cleanup in queue order, retaining each unsuccessful target.
    /// Completion states are conservative because scalar cleanup hooks expose no
    /// finer submission knowledge. Ordinary file operations are never replayed.
    pub fn drain_cleanup_outcomes(&self) -> vfsi_core::VecResult<()> {
        Ok(self
            .inner
            .lock_without_cleanup()
            .map_err(|_| poisoned())?
            .drain_cleanup_outcomes())
    }

    /// Take bounded diagnostics from previous cleanup attempts, including implicit
    /// drains before ordinary I/O. The second value counts older diagnostics
    /// evicted since the previous call. Taking diagnostics never retires targets.
    pub fn take_cleanup_errors(&self) -> (Vec<VfError>, u64) {
        self.inner.take_cleanup_errors()
    }

    /// Drain queued Drop cleanup and report the first failure. Failed targets
    /// remain owned for later cleanup. No new file operation is replayed.
    pub fn drain_cleanup(&self) -> VfResult<()> {
        self.inner
            .lock_without_cleanup()
            .map_err(|_| poisoned())?
            .drain_cleanup()
    }
}

impl<F: HandleBackend> FsClient<F> {
    pub(crate) fn capabilities(&self) -> VfResult<Capabilities> {
        Ok(self.lock()?.capabilities())
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Acquire one independent owned handle per successful input.
    pub(crate) fn vopen(&self, requests: &[OpenOp]) -> vfsi_core::VecResult<OpenFile<F>> {
        use vfsi_core::OpError;
        vfsi_core::internal::validate_open_requests(requests)?;
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut filesystem = self.lock()?;
        let mut outcomes = filesystem.vopen_impl(requests).map_err(|error| {
            with_indexed_context(error, "vopen", |index| {
                requests.get(index).map(|request| request.path())
            })
        })?;
        let own = |file, index: usize| OpenFile {
            inner: Arc::clone(&self.inner),
            file: Some(file),
            append: requests
                .get(index)
                .is_some_and(|request| request.flags().contains(OpenFlags::APPEND)),
            path: requests
                .get(index)
                .map(|request| request.path().to_path_buf())
                .unwrap_or_default(),
        };
        if outcomes.len() > requests.len() {
            let mut seen = HashSet::new();
            for (index, file) in outcomes
                .into_iter()
                .enumerate()
                .filter_map(|(index, outcome)| outcome.ok().map(|file| (index, file)))
            {
                if file.fd().is_some_and(|fd| fd >= 0 && seen.insert(fd)) {
                    drop(own(file, index));
                }
            }
            let output = requests
                .iter()
                .map(|request| {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "too many open outcomes"),
                        None,
                    )
                    .with_context("vopen", request.path()))
                })
                .collect();
            drop(filesystem);
            return Ok(output);
        }
        // Native allocators normally return increasing descriptor slots.
        // Avoid a hash allocation on that common path; unordered external
        // backends still receive full duplicate-owner validation.
        let mut previous = None;
        let increasing = outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().ok().and_then(FileOperand::fd))
            .all(|fd| {
                let valid = previous.is_none_or(|previous| previous < fd);
                previous = Some(fd);
                valid
            });
        if !increasing {
            let mut seen = HashMap::new();
            for index in 0..outcomes.len() {
                let Some(fd) = outcomes[index]
                    .as_ref()
                    .ok()
                    .and_then(FileOperand::fd)
                    .filter(|fd| *fd >= 0)
                else {
                    continue;
                };
                if let Some(&first) = seen.get(&fd) {
                    let cause = VfError::invalid_reply(None, "duplicate open descriptor");
                    let original = std::mem::replace(
                        &mut outcomes[first],
                        Err(OpError::unknown(cause.clone(), None)),
                    );
                    if let Ok(file) = original {
                        drop(own(file, first));
                    }
                    outcomes[index] = Err(OpError::unknown(cause, None));
                } else {
                    seen.insert(fd, index);
                }
            }
        }
        outcomes.resize_with(requests.len(), || {
            Err(OpError::unknown(
                VfError::invalid_reply(None, "missing open outcome"),
                None,
            ))
        });
        let output = outcomes
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                match result {
                    Ok(file) if file.fd().is_some_and(|fd| fd >= 0) => Ok(own(file, index)),
                    Ok(_) => Err(OpError::unknown(
                        VfError::invalid_reply(None, "open did not return a valid descriptor"),
                        None,
                    )),
                    Err(error) if error.progress().is_some() => Err(OpError::unknown(
                        VfError::invalid_reply(None, "open cannot report I/O byte progress"),
                        None,
                    )),
                    Err(error) => Err(error),
                }
                .map_err(|error| error.with_context("vopen", requests[index].path()))
            })
            .collect();
        drop(filesystem);
        Ok(output)
    }

    /// Close one native batch, preserving cleanup ownership per outcome.
    #[doc(hidden)]
    pub fn vclose<'a>(
        &self,
        files: impl IntoIterator<Item = &'a mut OpenFile<F>>,
    ) -> vfsi_core::VecResult<()>
    where
        F: 'a,
    {
        use vfsi_core::OpError;
        let mut files: Vec<_> = files.into_iter().collect();
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut output: Vec<_> = (0..files.len()).map(|_| None).collect();
        let mut positions = Vec::with_capacity(files.len());
        let mut descriptors = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            if let Err(error) = self.validate_owner(file, index) {
                output[index] = Some(Err(
                    OpError::not_attempted(error).with_context("vclose", file.path())
                ));
            } else if file.is_closed() {
                output[index] = Some(Ok(()));
            } else {
                positions.push(index);
                descriptors.push(file.raw()?.clone());
            }
        }
        if !descriptors.is_empty() {
            let result = self.lock()?.vclose_impl(&descriptors);
            let mut results = match result {
                Ok(results) if results.len() <= descriptors.len() => results,
                Ok(_) => descriptors
                    .iter()
                    .map(|_| {
                        Err(OpError::unknown(
                            VfError::invalid_reply(None, "too many close outcomes"),
                            None,
                        ))
                    })
                    .collect(),
                Err(error) => {
                    let error = if error
                        .index()
                        .is_some_and(|index| index >= descriptors.len())
                    {
                        VfError::invalid_reply(
                            None,
                            "close backend returned an invalid error index",
                        )
                    } else {
                        error
                    };
                    if positions.len() == files.len() {
                        return Err(with_indexed_context(error, "vclose", |index| {
                            files.get(index).map(|file| file.path())
                        }));
                    }
                    descriptors
                        .iter()
                        .map(|_| Err(OpError::not_attempted(error.clone())))
                        .collect()
                }
            };
            results.resize_with(descriptors.len(), || {
                Err(OpError::unknown(
                    VfError::invalid_reply(None, "missing close outcome"),
                    None,
                ))
            });
            for (index, result) in positions.into_iter().zip(results) {
                let result = match result {
                    Err(error) if error.progress().is_some() => Err(OpError::unknown(
                        VfError::invalid_reply(None, "close cannot report I/O byte progress"),
                        None,
                    )),
                    result => result,
                };
                if result.is_ok() {
                    files[index].file = None;
                }
                output[index] =
                    Some(result.map_err(|error| error.with_context("vclose", files[index].path())));
            }
        }
        Ok(output
            .into_iter()
            .map(|slot| slot.expect("every close input assigned"))
            .collect())
    }

    /// Consume handles after selecting the first error in input order.
    #[doc(hidden)]
    pub fn vclose_owned(&self, mut files: Vec<OpenFile<F>>) -> VfResult<()> {
        let expected = files.len();
        vfsi_core::internal::collect_vec_result(self.vclose(&mut files), expected).map(|_| ())
    }
}

fn link_error(error: VfError, paths: &[&Path], operation: &'static str) -> VfError {
    match error.index() {
        Some(index) if index < paths.len() => error.with_context(operation, paths[index]),
        Some(_) => VfError::invalid_reply(None, "link backend returned an invalid error index"),
        None => error,
    }
}

mod shared;
use shared::{BackendGuard, CleanupTarget, SharedBackend};
mod handles;
pub use handles::{FsDir, FsRead, FsReadInto, FsWrite, OpenFile};
mod directory;
mod metadata;
mod namespace;
mod read;
mod write;
