use std::io::{self, Read, Seek, SeekFrom as IoSeekFrom, Write};
use std::path::Path;

use crate::{ReadOp, SeekFrom, VecFs, VfFile, VfOffset, WriteOp};

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

/// Idiomatic Rust open options for a VFSI filesystem.
#[derive(Clone, Debug)]
pub struct VfOpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    mode: u32,
}

impl Default for VfOpenOptions {
    fn default() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            mode: 0o666,
        }
    }
}

impl VfOpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, value: bool) -> &mut Self {
        self.read = value;
        self
    }

    pub fn write(&mut self, value: bool) -> &mut Self {
        self.write = value;
        self
    }

    pub fn append(&mut self, value: bool) -> &mut Self {
        self.append = value;
        self
    }

    pub fn truncate(&mut self, value: bool) -> &mut Self {
        self.truncate = value;
        self
    }

    pub fn create(&mut self, value: bool) -> &mut Self {
        self.create = value;
        self
    }

    pub fn create_new(&mut self, value: bool) -> &mut Self {
        self.create_new = value;
        self
    }

    /// Unix permission bits used when a file is created.
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    fn flags(&self) -> io::Result<i32> {
        let writable = self.write || self.append;
        let mut flags = match (self.read, writable) {
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
        if self.truncate && !writable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "truncate requires write or append access",
            ));
        }
        if (self.create || self.create_new) && !writable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "create requires write or append access",
            ));
        }
        if self.append {
            flags |= libc::O_APPEND;
        }
        if self.truncate {
            flags |= libc::O_TRUNC;
        }
        if self.create || self.create_new {
            flags |= libc::O_CREAT;
        }
        if self.create_new {
            flags |= libc::O_EXCL;
        }
        Ok(flags)
    }

    /// Open a file whose lifetime is tied to the mutable filesystem borrow.
    pub fn open<'a, F: VecFs + ?Sized>(
        &self,
        filesystem: &'a mut F,
        path: impl AsRef<Path>,
    ) -> io::Result<VfFileHandle<'a, F>> {
        let file = filesystem
            .open(path.as_ref(), self.flags()?, self.mode)
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
pub struct VfFileHandle<'a, F: VecFs + ?Sized> {
    filesystem: &'a mut F,
    file: Option<VfFile>,
}

impl<F: VecFs + ?Sized> VfFileHandle<'_, F> {
    pub fn descriptor(&self) -> &VfFile {
        self.file.as_ref().expect("open handle has a descriptor")
    }

    pub fn close(mut self) -> io::Result<()> {
        let file = self.file.take().expect("open handle has a descriptor");
        self.filesystem.close(&file).map_err(io_error)
    }
}

impl<F: VecFs + ?Sized> Read for VfFileHandle<'_, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let file = self.file.as_ref().expect("open handle").clone();
        let mut results = self
            .filesystem
            .readv(&[ReadOp::new(file, VfOffset::Cur, buffer.len())])
            .map_err(io_error)?;
        let result = results.pop().expect("one read result");
        buffer[..result.data.len()].copy_from_slice(&result.data);
        Ok(result.data.len())
    }
}

impl<F: VecFs + ?Sized> Write for VfFileHandle<'_, F> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let file = self.file.as_ref().expect("open handle").clone();
        let mut results = self
            .filesystem
            .writev(&[WriteOp::new(file, VfOffset::Cur, buffer.to_vec())])
            .map_err(io_error)?;
        Ok(results.pop().expect("one write result").written)
    }

    fn flush(&mut self) -> io::Result<()> {
        // VFSI writes are synchronous. The NFS backend requests FILE_SYNC4,
        // so successful writes have already reached stable server storage.
        Ok(())
    }
}

impl<F: VecFs + ?Sized> Seek for VfFileHandle<'_, F> {
    fn seek(&mut self, position: IoSeekFrom) -> io::Result<u64> {
        let file = self.file.as_ref().expect("open handle").clone();
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
            .fseek(&file, offset, whence)
            .map_err(io_error)?;
        u64::try_from(result)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative seek result"))
    }
}

impl<F: VecFs + ?Sized> Drop for VfFileHandle<'_, F> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = self.filesystem.close(&file);
        }
    }
}
