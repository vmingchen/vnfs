use std::io::{self, Read, Seek, SeekFrom as IoSeekFrom, Write};
use std::path::Path;

use crate::traits::{validate_read_results, validate_write_results};
use crate::{Backend, ReadOp, SeekFrom, VfFile, VfOffset};

fn io_error(error: crate::VfError) -> io::Error {
    let kind = match error.err_no() {
        crate::ERR_NOENT => io::ErrorKind::NotFound,
        crate::ERR_EXIST => io::ErrorKind::AlreadyExists,
        crate::ERR_ACCES => io::ErrorKind::PermissionDenied,
        crate::ERR_NOTDIR => io::ErrorKind::NotADirectory,
        crate::ERR_ISDIR => io::ErrorKind::IsADirectory,
        crate::ERR_INVAL | crate::ERR_EBADF => io::ErrorKind::InvalidInput,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

#[bitfields::bitfield(u8)]
#[derive(PartialEq, Eq)]
struct VfOpenFlags {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    #[bits(2)]
    _reserved: u8,
}

/// Idiomatic Rust open options for a VFSI filesystem.
#[derive(Clone, Debug)]
pub struct VfOpenOptions {
    flags: VfOpenFlags,
    mode: u32,
}

impl Default for VfOpenOptions {
    fn default() -> Self {
        Self {
            flags: VfOpenFlags::new(),
            mode: 0o666,
        }
    }
}

impl VfOpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, value: bool) -> &mut Self {
        self.flags.set_read(value);
        self
    }

    pub fn write(&mut self, value: bool) -> &mut Self {
        self.flags.set_write(value);
        self
    }

    pub fn append(&mut self, value: bool) -> &mut Self {
        self.flags.set_append(value);
        self
    }

    pub fn truncate(&mut self, value: bool) -> &mut Self {
        self.flags.set_truncate(value);
        self
    }

    pub fn create(&mut self, value: bool) -> &mut Self {
        self.flags.set_create(value);
        self
    }

    pub fn create_new(&mut self, value: bool) -> &mut Self {
        self.flags.set_create_new(value);
        self
    }

    /// Unix permission bits used when a file is created.
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    fn flags(&self) -> io::Result<i32> {
        let writable = self.flags.write() || self.flags.append();
        let mut flags = match (self.flags.read(), writable) {
            (true, true) => libc::O_RDWR,
            (false, true) => libc::O_WRONLY,
            (true, false) => libc::O_RDONLY,
            (false, false) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "at least one of read, write, or append must be enabled",
                ));
            }
        };
        if self.flags.truncate() && !writable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "truncate requires write or append access",
            ));
        }
        if (self.flags.create() || self.flags.create_new()) && !writable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "create requires write or append access",
            ));
        }
        if self.flags.append() {
            flags |= libc::O_APPEND;
        }
        if self.flags.truncate() {
            flags |= libc::O_TRUNC;
        }
        if self.flags.create() || self.flags.create_new() {
            flags |= libc::O_CREAT;
        }
        if self.flags.create_new() {
            flags |= libc::O_EXCL;
        }
        Ok(flags)
    }

    /// Open a file whose lifetime is tied to the mutable filesystem borrow.
    pub fn open<'a, F: Backend + ?Sized>(
        &self,
        filesystem: &'a mut F,
        path: impl AsRef<Path>,
    ) -> io::Result<VfFileHandle<'a, F>> {
        let file = filesystem
            .open_raw_impl(path.as_ref(), self.flags()?, self.mode)
            .map_err(io_error)?;
        Ok(VfFileHandle {
            filesystem,
            file: Some(file),
        })
    }
}

/// RAII descriptor adapter implementing standard synchronous Rust I/O traits.
///
/// Dropping the handle closes the remote descriptor on a best-effort basis.
/// Use [`close`](Self::close) when a close error must be observed.
pub struct VfFileHandle<'a, F: Backend + ?Sized> {
    filesystem: &'a mut F,
    file: Option<VfFile>,
}

impl<F: Backend + ?Sized> VfFileHandle<'_, F> {
    /// Descriptor of an open handle. Use [`try_descriptor`](Self::try_descriptor)
    /// when the handle might already have been closed with `try_close`.
    pub fn descriptor(&self) -> &VfFile {
        self.file.as_ref().expect("open handle has a descriptor")
    }

    pub fn try_descriptor(&self) -> io::Result<&VfFile> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file handle is closed"))
    }

    /// Attempt CLOSE while retaining this handle if the backend reports an
    /// error. A second attempt should follow the backend's recovery policy.
    pub fn try_close(&mut self) -> io::Result<()> {
        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        self.filesystem.close_impl(file).map_err(io_error)?;
        self.file = None;
        Ok(())
    }

    /// Consume and close the handle. On failure, `Drop` attempts cleanup.
    pub fn close(mut self) -> io::Result<()> {
        self.try_close()
    }
}

impl<F: Backend + ?Sized> Read for VfFileHandle<'_, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let file = self.try_descriptor()?.clone();
        let requests = [ReadOp::new(file, VfOffset::Cur, buffer.len())];
        let mut results = self.filesystem.vread_impl(&requests).map_err(io_error)?;
        validate_read_results("Read::read", &requests, &results).map_err(io_error)?;
        let result = results.pop().expect("validated one read result");
        buffer[..result.data.len()].copy_from_slice(&result.data);
        Ok(result.data.len())
    }
}

impl<F: Backend + ?Sized> Write for VfFileHandle<'_, F> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let file = self.try_descriptor()?.clone();
        let requests = [crate::WriteOp::new(&file, VfOffset::Cur, buffer)];
        let mut results = self.filesystem.vwrite_impl(&requests).map_err(io_error)?;
        validate_write_results("Write::write", &requests, &results).map_err(io_error)?;
        Ok(results.pop().expect("validated one write result").written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let file = self.try_descriptor()?.clone();
        self.filesystem.sync_data(&file).map_err(io_error)
    }
}

impl<F: Backend + ?Sized> Seek for VfFileHandle<'_, F> {
    fn seek(&mut self, position: IoSeekFrom) -> io::Result<u64> {
        let file = self.try_descriptor()?.clone();
        let (offset, whence) = match position {
            IoSeekFrom::Start(offset) => (
                i64::try_from(offset).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "seek offset exceeds i64")
                })?,
                SeekFrom::Set,
            ),
            IoSeekFrom::End(offset) => (offset, SeekFrom::End),
            IoSeekFrom::Current(offset) => (offset, SeekFrom::Cur),
        };
        let result = self
            .filesystem
            .seek_raw_impl(&file, offset, whence)
            .map_err(io_error)?;
        u64::try_from(result)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative seek result"))
    }
}

impl<F: Backend + ?Sized> Drop for VfFileHandle<'_, F> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = self.filesystem.close_impl(&file);
        }
    }
}

#[cfg(test)]
mod option_layout_tests {
    use super::*;
    #[test]
    fn packed_open_flags_match_all_access_and_creation_combinations() {
        assert_eq!(std::mem::size_of::<VfOpenFlags>(), 1);
        assert_eq!(std::mem::size_of::<VfOpenOptions>(), 8);
        for bits in 0..64 {
            let [read, write, append, truncate, create, exclusive] =
                [0, 1, 2, 3, 4, 5].map(|bit| bits & (1 << bit) != 0);
            let mut options = VfOpenOptions::new();
            options
                .read(read)
                .write(write)
                .append(append)
                .truncate(truncate)
                .create(create)
                .create_new(exclusive);
            let writable = write || append;
            let invalid = (!read && !writable) || ((truncate || create || exclusive) && !writable);
            let result = options.flags();
            if invalid {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
                continue;
            }
            let mut expected = match (read, writable) {
                (true, true) => libc::O_RDWR,
                (false, true) => libc::O_WRONLY,
                _ => libc::O_RDONLY,
            };
            if append {
                expected |= libc::O_APPEND;
            }
            if truncate {
                expected |= libc::O_TRUNC;
            }
            if create || exclusive {
                expected |= libc::O_CREAT;
            }
            if exclusive {
                expected |= libc::O_EXCL;
            }
            assert_eq!(result.unwrap(), expected);
            options
                .read(true)
                .append(false)
                .truncate(false)
                .create(false)
                .create_new(false);
            assert_eq!(
                options.flags().unwrap()
                    & (libc::O_APPEND | libc::O_TRUNC | libc::O_CREAT | libc::O_EXCL),
                0
            );
        }
    }
}
