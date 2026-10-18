//! Owned, shareable synchronous client and file handles.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfsi_core::api::internal::OwnedReadResult as FsReadResult;
use vfsi_core::api::{
    DirectoryListing, ReadIntoResult as FsReadIntoResult, ResourceLimits, StreamCompletion,
    WriteResult as FsWriteResult,
};

use crate::backend::{HandleBackend, VectorBackend};
use crate::traits::{validate_read_into_results, validate_read_results, validate_write_results};
use crate::{
    AttrMask, Attrs, Capabilities, DirEntry, FileOperand, OpenFlags, OpenOp, ReadAllOptions,
    ReadOp, ReadResult, RemoveOptions, StreamOptions, VfDir, VfError, VfOffset, VfResult, WriteOp,
    WriteResult,
};

fn with_indexed_context<'a>(
    error: VfError,
    operation: &'static str,
    path: impl FnOnce(usize) -> Option<&'a Path>,
) -> VfError {
    match error.index().and_then(path) {
        Some(path) => error.with_context(operation, path),
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
    VfError::client(0, crate::ERR_IO)
}

fn wrong_result_count(operation: &str, expected: usize, actual: usize) -> VfError {
    VfError::transport_with_kind(
        None,
        crate::TransportKind::InvalidReply,
        format!("{operation} backend returned {actual} results for {expected} requests"),
    )
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
    /// Open a path read-only.
    pub(crate) fn open(&self, path: impl AsRef<Path>) -> VfResult<OpenFile<F>> {
        self.open_with_native(OpenOp::new(path.as_ref(), OpenFlags::READ))
    }

    pub(crate) fn open_with_native(&self, request: OpenOp) -> VfResult<OpenFile<F>> {
        vfsi_core::internal::validate_open_requests(std::slice::from_ref(&request))?;
        let file = self.lock()?.open_impl(&request)?;
        Ok(OpenFile {
            inner: Arc::clone(&self.inner),
            file: Some(file),
            append: request.flags().contains(OpenFlags::APPEND),
            path: request.into_path(),
        })
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Open an ordered vector of files.
    ///
    /// Success returns one RAII handle per request. Failure returns no
    /// handles; VFSI does not promise transactional rollback of other
    /// filesystem effects such as file creation.
    pub(crate) fn vopen(&self, requests: &[OpenOp]) -> VfResult<Vec<OpenFile<F>>> {
        vfsi_core::internal::validate_open_requests(requests)?;
        let mut filesystem = self.lock()?;
        let files = filesystem.vopen_impl(requests).map_err(|error| {
            with_indexed_context(error, "vopen", |index| {
                requests.get(index).map(|request| request.path())
            })
        })?;
        if files.len() != requests.len() {
            let error = wrong_result_count("vopen", requests.len(), files.len());
            for (index, file) in files.into_iter().enumerate() {
                if filesystem.close_impl(&file).is_err() {
                    // Keep failed cleanup under the same RAII/deferred-close
                    // ownership as ordinary handles, preserving the shape error.
                    drop(OpenFile {
                        inner: Arc::clone(&self.inner),
                        file: Some(file),
                        append: false,
                        path: requests
                            .get(index)
                            .map(|request| request.path().to_path_buf())
                            .unwrap_or_default(),
                    });
                }
            }
            return Err(error);
        }
        drop(filesystem);
        Ok(files
            .into_iter()
            .zip(requests)
            .map(|(file, request)| OpenFile {
                inner: Arc::clone(&self.inner),
                file: Some(file),
                append: request.flags().contains(OpenFlags::APPEND),
                path: request.path().to_path_buf(),
            })
            .collect())
    }

    /// Try to close a group through one vector operation without consuming
    /// the handles. On failure, all handles remain armed: the backend may
    /// have closed a prefix, so callers must reconcile before retrying.
    #[doc(hidden)]
    pub fn vclose<'a>(&self, files: impl IntoIterator<Item = &'a mut OpenFile<F>>) -> VfResult<()>
    where
        F: 'a,
    {
        let mut files: Vec<_> = files.into_iter().collect();
        for (index, file) in files.iter().enumerate() {
            self.validate_owner(file, index)?;
        }
        let positions: Vec<_> = files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| (!file.is_closed()).then_some(index))
            .collect();
        files.retain(|file| !file.is_closed());
        if files.is_empty() {
            return Ok(());
        }
        let descriptors: Vec<FileOperand> = files
            .iter()
            .map(|file| file.raw().cloned())
            .collect::<VfResult<_>>()?;
        self.lock()?.vclose_impl(&descriptors).map_err(|error| {
            with_indexed_context(error, "vclose_owned", |index| {
                files.get(index).map(|file| file.path())
            })
            .map_index(|index| positions.get(index).copied().unwrap_or(index))
        })?;
        for file in &mut files {
            file.file = None;
        }
        Ok(())
    }

    /// Close a group of files through one vector operation.
    ///
    /// On failure, the handles are dropped and the backend receives
    /// best-effort scalar cleanup attempts. Use [`vclose`](Self::vclose)
    /// to retain the handles after an error.
    #[doc(hidden)]
    pub fn vclose_owned(&self, mut files: Vec<OpenFile<F>>) -> VfResult<()> {
        self.vclose(&mut files)
    }

    fn validate_owner(&self, file: &OpenFile<F>, index: usize) -> VfResult<()> {
        if Arc::ptr_eq(&self.inner, &file.inner) {
            Ok(())
        } else {
            Err(VfError::client(index, crate::ERR_INVAL))
        }
    }
}

fn link_error(error: VfError, paths: &[&Path], operation: &'static str) -> VfError {
    match error.index() {
        Some(index) if index < paths.len() => error.with_context(operation, paths[index]),
        Some(_) => VfError::transport(None, "link backend returned an invalid error index"),
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
