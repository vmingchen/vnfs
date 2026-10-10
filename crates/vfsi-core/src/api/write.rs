//! Portable borrowed positional writes.

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
/// Uses the same storage definition as the native [`crate::WriteOp`].
#[repr(transparent)]
pub struct WriteOp<'a, H>(crate::internal::WriteRequest<&'a H, &'a [u8], u64, ()>);

impl<H> Copy for WriteOp<'_, H> {}
impl<H> Clone for WriteOp<'_, H> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<H> std::fmt::Debug for WriteOp<'_, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl<'a, H> WriteOp<'a, H> {
    /// Borrow a handle and payload at an absolute byte offset.
    pub fn new(file: &'a H, offset: u64, data: &'a [u8]) -> Self {
        Self(crate::internal::WriteRequest::new(file, offset, data))
    }
    /// Borrow a handle and payload at an absolute byte offset.
    pub fn at(file: &'a H, offset: u64, data: &'a [u8]) -> Self {
        Self::new(file, offset, data)
    }
    pub fn file(&self) -> &'a H {
        self.0.file()
    }
    pub fn offset(&self) -> u64 {
        self.0.offset()
    }
    pub fn data(&self) -> &'a [u8] {
        self.0.data()
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
