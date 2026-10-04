//! Portable borrowed positional writes.
use crate::FileHandle;

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
    /// let result = fs.write_allv(&[WriteOp::at(&file, 0, b"hello")]);
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
