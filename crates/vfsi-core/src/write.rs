use crate::{Fd, VfFile, VfOffset};
use std::path::Path;

#[bitfields::bitfield(u8)]
struct NativeWriteFlags {
    creation: bool,
    truncate: bool,
    #[bits(6)]
    _reserved: u8,
}

/// A native write with owned or borrowed target and payload storage.
/// Positioning is always [`VfOffset`], with packed create/truncate flags.
/// The fixed policy types let constructors infer storage without annotations.
///
/// ```
/// use vfsi_core::{VfFile, VfOffset, WriteOp};
/// let current = WriteOp::new(VfFile::from_fd(7), VfOffset::Cur, vec![1, 2]);
/// let positional = WriteOp::at(VfFile::from_fd(7), 11, vec![3, 4]);
/// assert_eq!(current.offset(), VfOffset::Cur);
/// assert_eq!(positional.offset(), VfOffset::At(11));
/// assert_eq!(positional.borrowed().data(), &[3, 4]);
/// ```
pub type WriteOp<T = VfFile, D = Vec<u8>> = WriteRequest<T, D, VfOffset, u8>;

/// One write operation, independent of how its target and payload are stored.
///
/// Native adapters may own a `VfFile` and `Vec<u8>` while preparing a batch;
/// execution borrows both through [`WriteRequest::borrowed`]. The portable API uses
/// the same type with a borrowed application handle and an absolute `u64`
/// offset. Borrowing for dispatch never clones a handle or copies payload bytes.
///
/// The final storage parameter is `u8` for native path flags and `()` for
/// portable/retained-handle requests, so positional operations pay no space
/// overhead for path-only creation and truncation flags.
#[derive(Clone, Copy)]
pub struct WriteRequest<T, D, O, P> {
    file: T,
    offset: O,
    data: D,
    flags: P,
}

impl<T, D, O, P: Default> WriteRequest<T, D, O, P> {
    /// Prepare an operation without I/O; the offset type determines positioning.
    pub fn new(file: T, offset: O, data: D) -> Self {
        Self {
            file,
            offset,
            data,
            flags: P::default(),
        }
    }
}

impl<T, D> WriteRequest<T, D, VfOffset, u8> {
    /// Whether the native path write creates a missing file.
    pub fn creates(&self) -> bool {
        NativeWriteFlags::from_bits(self.flags).creation()
    }
    /// Whether the native path write truncates before writing.
    pub fn truncates(&self) -> bool {
        NativeWriteFlags::from_bits(self.flags).truncate()
    }
    /// Create a missing native path when executing this operation.
    pub fn with_creation(mut self) -> Self {
        let mut flags = NativeWriteFlags::from_bits(self.flags);
        flags.set_creation(true);
        self.flags = flags.into_bits();
        self
    }
    /// Truncate a native path before writing, fused into the native write.
    pub fn with_truncate(mut self) -> Self {
        let mut flags = NativeWriteFlags::from_bits(self.flags);
        flags.set_truncate(true);
        self.flags = flags.into_bits();
        self
    }
}

impl<T, D: AsRef<[u8]>, O: Copy, P: Copy> WriteRequest<T, D, O, P> {
    /// Borrow preparation storage for execution, preserving all options.
    pub fn borrowed(&self) -> WriteRequest<&T, &[u8], O, P> {
        WriteRequest {
            file: &self.file,
            offset: self.offset,
            data: self.data.as_ref(),
            flags: self.flags,
        }
    }
}

impl<T, D, O: Copy, P> WriteRequest<T, D, O, P> {
    /// Positioning policy, or an absolute offset for portable operations.
    pub fn offset(&self) -> O {
        self.offset
    }
}

impl<D, O, P> WriteRequest<VfFile, D, O, P> {
    /// Target storage owned while preparing a native request.
    pub fn file(&self) -> &VfFile {
        &self.file
    }
}
impl<T, O, P> WriteRequest<T, Vec<u8>, O, P> {
    /// Payload storage owned while preparing a native request.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl<'a, T, O, P> WriteRequest<&'a T, &'a [u8], O, P> {
    /// Borrowed target. Constructing an operation never opens or closes it.
    pub fn file(&self) -> &'a T {
        self.file
    }
    /// Borrowed payload; ownership remains with the caller.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

impl<T, D, O: From<u64>, P: Default> WriteRequest<T, D, O, P> {
    /// Prepare an absolute-offset write; validation happens at dispatch.
    /// Portable writes do not change the handle's cursor. Native append
    /// descriptors retain the backend's append positioning semantics.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp, WriteOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.vwrite(&[WriteOp::at(&file, 0, b"hello")], WriteOptions::new().write_all(true));
    /// let close = fs.close_files(vec![file]);
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn at(file: T, offset: u64, data: D) -> Self {
        Self::new(file, offset.into(), data)
    }
}

impl WriteOp {
    pub fn from_path(path: &str, offset: VfOffset, data: Vec<u8>) -> Self {
        Self::new(VfFile::from_path(path), offset, data)
    }
    pub fn from_os_path(path: &Path, offset: VfOffset, data: Vec<u8>) -> Self {
        Self::new(VfFile::from_os_path(path), offset, data)
    }
    pub fn from_fd(fd: Fd, offset: VfOffset, data: Vec<u8>) -> Self {
        Self::new(VfFile::from_fd(fd), offset, data)
    }
}

impl<T, D: AsRef<[u8]>, O: std::fmt::Debug, P> std::fmt::Debug for WriteRequest<T, D, O, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteOp")
            .field("offset", &self.offset)
            .field("length", &self.data.as_ref().len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_storage_is_borrowed_without_copying_or_losing_native_options() {
        let file = VfFile::from_os_path(Path::new("/output"));
        for offset in [VfOffset::At(7), VfOffset::Cur, VfOffset::End] {
            for (create, truncate) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut op = WriteOp::new(file.clone(), offset, vec![0x5a; 1024]);
                if create {
                    op = op.with_creation();
                }
                if truncate {
                    op = op.with_truncate();
                }
                let native: WriteOp<&VfFile, &[u8]> = op.borrowed();
                assert!(std::ptr::eq(native.file(), op.file()));
                assert_eq!(native.data().as_ptr(), op.data().as_ptr());
                assert_eq!(native.data().len(), op.data().len());
                assert_eq!(native.offset(), offset);
                assert_eq!(native.creates(), create);
                assert_eq!(native.truncates(), truncate);
                // Preparing multiple dispatch vectors does not consume storage.
                assert_eq!(op.borrowed().data().as_ptr(), native.data().as_ptr());
            }
        }
        let empty = WriteOp::from_fd(7, VfOffset::Cur, Vec::new());
        assert!(empty.borrowed().data().is_empty());
    }

    #[test]
    fn portable_operations_are_copyable_without_handle_traits_or_extra_flags_storage() {
        struct NonClone;
        let file = NonClone;
        let payload = b"payload";
        let op = crate::api::WriteOp::at(&file, 11, payload);
        let copy = op;
        assert!(std::ptr::eq(op.file(), copy.file()));
        assert_eq!(op.data().as_ptr(), payload.as_ptr());
        assert_eq!(copy.data().as_ptr(), payload.as_ptr());
        assert_eq!(copy.offset(), 11);
        assert_eq!(
            std::mem::size_of_val(&op),
            std::mem::size_of::<(&NonClone, u64, &[u8])>()
        );
        assert_eq!(std::mem::size_of::<NativeWriteFlags>(), 1);
        // Handle requests also carry no path-only flags, even with native offsets.
        type HandleOp<'a> = WriteRequest<&'a NonClone, &'a [u8], VfOffset, ()>;
        assert_eq!(
            std::mem::size_of::<HandleOp<'_>>(),
            std::mem::size_of::<(&NonClone, VfOffset, &[u8])>()
        );
        assert!(!format!("{op:?}").is_empty());
    }
}
