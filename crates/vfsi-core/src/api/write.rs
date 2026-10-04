//! Portable borrowed positional writes.
use crate::api::FileHandle;

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct WriteFlags {
    write_all: bool,
    #[bits(7)]
    _reserved: u8,
}

/// Policy for positional vector writes. Defaults to reporting short writes.
/// Completion is not atomicity, durability, or permission to replay failed RPCs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteOptions {
    flags: WriteFlags,
}
impl WriteOptions {
    /// Report each request's accepted byte count without completing short writes.
    pub const fn new() -> Self {
        Self {
            flags: WriteFlags::new(),
        }
    }
    /// Complete successful short writes at their remaining offsets when true.
    /// Stop on any error, including ambiguous transport failures; never replay it.
    /// This does not flush data to stable storage or make the batch atomic.
    pub const fn write_all(mut self, complete: bool) -> Self {
        self.flags.set_write_all(complete);
        self
    }
    /// Whether successful short writes should be completed.
    pub const fn writes_all(self) -> bool {
        self.flags.write_all()
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
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.vwrite(&[WriteOp::at(&file, 0, b"hello")], vfsi_core::api::WriteOptions::new().write_all(true));
    /// let close = fs.close_files(vec![file]);
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

#[cfg(test)]
mod option_layout_tests {
    use super::*;
    #[test]
    fn packed_completion_flag_keeps_const_defaults_and_toggles() {
        const COMPLETE: WriteOptions = WriteOptions::new().write_all(true);
        assert_eq!(std::mem::size_of::<WriteOptions>(), 1);
        assert!(!WriteOptions::default().writes_all());
        assert!(COMPLETE.writes_all());
        assert!(!COMPLETE.write_all(false).writes_all());
        assert!(COMPLETE.write_all(false).write_all(true).writes_all());
    }
}
