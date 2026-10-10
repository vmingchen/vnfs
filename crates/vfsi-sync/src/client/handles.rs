//! Owned file and directory lifetimes and borrowed I/O requests.

use super::*;

/// Owned, handle-rooted directory. Dropping it queues backend cleanup;
/// [`close`](Self::close) reports cleanup errors explicitly.
pub struct FsDir<F: VectorBackend> {
    pub(super) inner: Arc<SharedBackend<F>>,
    pub(super) dir: Option<VfDir>,
    pub(super) path: PathBuf,
}

impl<F: VectorBackend> fmt::Debug for FsDir<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsDir")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<F: VectorBackend> FsDir<F> {
    pub fn is_closed(&self) -> bool {
        self.dir.is_none()
    }
    /// Name used at open, retained for diagnostics; not updated after rename.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn close(mut self) -> VfResult<()> {
        self.try_close()
    }

    /// Keep cleanup ownership on failure so the caller can retry explicitly.
    pub fn try_close(&mut self) -> VfResult<()> {
        let Some(dir) = self.dir.as_ref() else {
            return Ok(());
        };
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .close_dir_impl(dir)?;
        self.dir = None;
        Ok(())
    }
}

impl<F: VectorBackend> Drop for FsDir<F> {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            self.inner.defer(
                CleanupTarget::Directory(dir, std::mem::take(&mut self.path)),
                |backend, target| {
                    let CleanupTarget::Directory(dir, path) = target else {
                        unreachable!()
                    };
                    backend
                        .close_dir_impl(dir)
                        .map_err(|error| error.with_context("close_dir", path))
                },
            );
        }
    }
}

/// Owned RAII file which does not borrow the client.
///
/// File operations use the owning client's `Vfsi` implementation. Dropping an
/// open file queues cleanup without waiting for the backend lock. Dropping the
/// final connection owner can perform synchronous teardown. Use `try_close`
/// when the close result matters.
pub struct FsFile<F: HandleBackend> {
    pub(super) inner: Arc<SharedBackend<F>>,
    pub(super) file: Option<VfFile>,
    // Retained open policy; never exposed through the portable handle API.
    pub(super) append: bool,
    pub(super) path: PathBuf,
}

impl<F: HandleBackend> fmt::Debug for FsFile<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsFile")
            .field("path", &self.path)
            .field("is_open", &self.file.is_some())
            .finish()
    }
}

impl<F: HandleBackend> FsFile<F> {
    /// Whether explicit close has completed successfully on this handle.
    pub fn is_closed(&self) -> bool {
        self.file.is_none()
    }
    pub(super) fn raw(&self) -> VfResult<&VfFile> {
        self.file
            .as_ref()
            .ok_or_else(|| VfError::client(0, crate::ERR_EBADF))
    }

    pub fn read_request_at(&self, offset: u64, length: usize) -> FsRead<'_, F> {
        FsRead {
            file: self,
            offset: VfOffset::At(offset),
            length,
        }
    }

    /// Name used at open, retained for diagnostics; not a current namespace
    /// lookup or proof of identity. Handle operations continue to use the
    /// opened object even if its pathname changes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> FsWrite<'a, F> {
        FsWrite::new(self, VfOffset::At(offset), data)
    }

    pub fn read_request_at_into<'a>(
        &'a self,
        offset: u64,
        buffer: &'a mut [u8],
    ) -> FsReadInto<'a, F> {
        FsReadInto {
            file: self,
            offset: VfOffset::At(offset),
            buffer,
        }
    }

    /// Attempt to close without consuming the handle. A failed close leaves
    /// the handle armed so the caller can reconcile or retry cleanup explicitly.
    /// After an ambiguous close failure, remote state is unknown: do not resume
    /// data I/O just because `is_closed()` is false. This is local ownership,
    /// not proof that the server still has the descriptor open.
    pub fn try_close(&mut self) -> VfResult<()> {
        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .close_impl(file)
            .map_err(|error| error.with_context("close", &self.path))?;
        self.file = None;
        Ok(())
    }

    /// Consume and close the handle. On failure, `Drop` queues cleanup for
    /// a later operation or explicit cleanup drain; use
    /// [`try_close`](Self::try_close) to retain control.
    pub fn close(mut self) -> VfResult<()> {
        self.try_close()
    }
}

/// Typed read request for [`FsClient::vread_native`].
pub struct FsRead<'a, F: HandleBackend> {
    pub(super) file: &'a FsFile<F>,
    pub(super) offset: VfOffset,
    pub(super) length: usize,
}

/// Typed borrowed write request for [`FsClient::vwrite_native`].
pub type FsWrite<'a, F> = vfsi_core::internal::WriteRequest<&'a FsFile<F>, &'a [u8], VfOffset, ()>;

/// Typed vector read into caller-provided storage.
pub struct FsReadInto<'a, F: HandleBackend> {
    pub(super) file: &'a FsFile<F>,
    pub(super) offset: VfOffset,
    pub(super) buffer: &'a mut [u8],
}

impl<F: HandleBackend> Drop for FsFile<F> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            self.inner.defer(
                CleanupTarget::File(file, std::mem::take(&mut self.path)),
                |backend, target| {
                    let CleanupTarget::File(file, path) = target else {
                        unreachable!()
                    };
                    backend
                        .close_deferred(file)
                        .map_err(|error| error.with_context("close", path))
                },
            );
        }
    }
}
