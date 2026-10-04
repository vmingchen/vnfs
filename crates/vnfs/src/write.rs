//! Portable borrowed positional writes.
use crate::FileHandle;

/// Policy for positional vector writes. Defaults to reporting short writes.
/// Completion is not atomicity, durability, or permission to replay failed RPCs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteOptions {
    write_all: bool,
}
impl WriteOptions {
    /// Report each request's accepted byte count without completing short writes.
    pub const fn new() -> Self {
        Self { write_all: false }
    }
    /// Complete successful short writes at their remaining offsets when true.
    /// Stop on any error, including ambiguous transport failures; never replay it.
    /// This does not flush data to stable storage or make the batch atomic.
    pub const fn write_all(mut self, complete: bool) -> Self {
        self.write_all = complete;
        self
    }
    /// Whether successful short writes should be completed.
    pub const fn writes_all(self) -> bool {
        self.write_all
    }
}

/// A positional write borrowing its handle and payload without copying either.
/// Construction performs no I/O or allocation. Does not change the file cursor.
pub struct WriteOp<'a, H: FileHandle + 'a> {
    file: &'a H,
    offset: u64,
    data: &'a [u8],
}
impl<H: FileHandle> Copy for WriteOp<'_, H> {}
impl<H: FileHandle> Clone for WriteOp<'_, H> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<H: FileHandle> std::fmt::Debug for WriteOp<'_, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteOp")
            .field("offset", &self.offset)
            .field("length", &self.data.len())
            .finish_non_exhaustive()
    }
}
impl<'a, H: FileHandle> WriteOp<'a, H> {
    /// Prepare a write at an absolute byte offset; validation happens at dispatch.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let file = fs.create_one("/output")?;
    /// let result = fs.writev_with_options(&[WriteOp::at(&file, 0, b"hello")], vnfs::WriteOptions::new().write_all(true));
    /// let close = fs.closev(vec![file]);
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn at(file: &'a H, offset: u64, data: &'a [u8]) -> Self {
        Self { file, offset, data }
    }
    /// The borrowed handle, available to third-party client implementations.
    pub fn file(&self) -> &'a H {
        self.file
    }
    /// Absolute byte offset, independent of the handle's cursor.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    /// Borrowed payload; no ownership transfer or copy occurs.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

// Keep low-level completion machinery private without leaking its historical
// operation name through application errors. Preserve status, index, and path.
pub(crate) fn public_write_error(error: crate::Error) -> crate::Error {
    if error.operation() == Some("write_allv")
        && let Some(path) = error.path().map(std::path::Path::to_path_buf)
    {
        error.with_context("writev", path)
    } else {
        error
    }
}

pub(crate) fn write_backend<'a, F>(
    client: &vfsi_sync::FsClient<F>,
    ops: &[WriteOp<'a, vfsi_sync::FsFile<F>>],
) -> crate::Result<Vec<crate::WriteResult>>
where
    F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static,
{
    client.writev_mapped(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}

pub(crate) fn write_backend_all<'a, F>(
    client: &vfsi_sync::FsClient<F>,
    ops: &[WriteOp<'a, vfsi_sync::FsFile<F>>],
) -> crate::Result<Vec<crate::WriteResult>>
where
    F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static,
{
    client.write_allv_mapped(ops, |op| op.file().write_request_at(op.offset(), op.data()))
}
