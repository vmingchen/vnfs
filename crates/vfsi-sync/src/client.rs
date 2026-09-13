//! Owned, shareable synchronous client and file handles.

use std::io::{self, Read, Seek, SeekFrom as IoSeekFrom, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{
    FileSystem, OpenRequest, ReadOp, ReadResult, VectorFileSystem, VfFile, VfOffset, WriteOpRef,
    WriteResult,
};

fn io_error(error: crate::VfError) -> io::Error {
    error.into()
}

fn poisoned() -> io::Error {
    io::Error::other("VFSI client lock was poisoned")
}

/// Cloneable owner of one synchronous backend connection.
///
/// Backend access is serialized because the legacy protocol implementations
/// keep session and descriptor state internally. Clone the client freely;
/// independently opened [`FsFile`] values can coexist and move to worker
/// threads when the backend is `Send`.
pub struct FsClient<F> {
    inner: Arc<Mutex<F>>,
}

impl<F> Clone for FsClient<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<F> FsClient<F> {
    pub fn new(filesystem: F) -> Self {
        Self {
            inner: Arc::new(Mutex::new(filesystem)),
        }
    }

    pub fn lock(&self) -> io::Result<MutexGuard<'_, F>> {
        self.inner.lock().map_err(|_| poisoned())
    }

    /// Run a vectorized operation while holding the backend session lock.
    pub fn with<R>(&self, operation: impl FnOnce(&mut F) -> R) -> io::Result<R> {
        let mut filesystem = self.lock()?;
        Ok(operation(&mut filesystem))
    }

    pub fn into_inner(self) -> Result<F, Self> {
        match Arc::try_unwrap(self.inner) {
            Ok(mutex) => match mutex.into_inner() {
                Ok(filesystem) => Ok(filesystem),
                Err(poisoned) => Ok(poisoned.into_inner()),
            },
            Err(inner) => Err(Self { inner }),
        }
    }
}

impl<F: FileSystem> FsClient<F> {
    pub fn open(&self, request: OpenRequest) -> io::Result<FsFile<F>> {
        let file = self.lock()?.open_one(&request).map_err(io_error)?;
        Ok(FsFile {
            inner: Arc::clone(&self.inner),
            file: Some(file),
        })
    }
}

impl<F: VectorFileSystem> FsClient<F> {
    /// Open a cohort in one backend vector request without parallel slices.
    pub fn open_many(&self, requests: &[OpenRequest]) -> io::Result<Vec<FsFile<F>>> {
        let files = self.lock()?.open_many(requests).map_err(io_error)?;
        Ok(files
            .into_iter()
            .map(|file| FsFile {
                inner: Arc::clone(&self.inner),
                file: Some(file),
            })
            .collect())
    }

    /// Read from several owned files in one backend vector call. Requests
    /// from another client are rejected instead of sending a forged handle.
    pub fn read_many(&self, requests: &[FsRead<'_, F>]) -> io::Result<Vec<ReadResult>> {
        let mut reads = Vec::with_capacity(requests.len());
        for request in requests {
            if !Arc::ptr_eq(&self.inner, &request.file.inner) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file belongs to another client",
                ));
            }
            reads.push(ReadOp::new(
                request.file.raw().clone(),
                request.offset,
                request.length,
            ));
        }
        self.lock()?.read_many(&reads).map_err(io_error)
    }

    /// Fill caller-provided buffers with one vector read. The current
    /// protocol decoders own reply buffers, so this removes the public result
    /// allocation but may still require one decode-to-destination copy.
    pub fn read_many_into(&self, requests: &mut [FsReadInto<'_, F>]) -> io::Result<Vec<usize>> {
        let mut reads = Vec::with_capacity(requests.len());
        for request in requests.iter() {
            if !Arc::ptr_eq(&self.inner, &request.file.inner) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file belongs to another client",
                ));
            }
            reads.push(ReadOp::new(
                request.file.raw().clone(),
                request.offset,
                request.buffer.len(),
            ));
        }
        let results = self.lock()?.read_many(&reads).map_err(io_error)?;
        if results.len() != requests.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backend returned the wrong number of read results",
            ));
        }
        let mut lengths = Vec::with_capacity(results.len());
        for (request, result) in requests.iter_mut().zip(results) {
            if result.data.len() > request.buffer.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "backend returned more data than requested",
                ));
            }
            request.buffer[..result.data.len()].copy_from_slice(&result.data);
            lengths.push(result.data.len());
        }
        Ok(lengths)
    }

    /// Write to several owned files without requiring caller-owned buffers to
    /// be copied at the API boundary.
    pub fn write_many(&self, requests: &[FsWrite<'_, F>]) -> io::Result<Vec<WriteResult>> {
        for request in requests {
            if !Arc::ptr_eq(&self.inner, &request.file.inner) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file belongs to another client",
                ));
            }
        }
        let writes: Vec<WriteOpRef<'_>> = requests
            .iter()
            .map(|request| WriteOpRef::new(request.file.raw(), request.offset, request.data))
            .collect();
        self.lock()?.write_many(&writes).map_err(io_error)
    }
}

/// Owned RAII file which does not borrow the client.
pub struct FsFile<F: FileSystem> {
    inner: Arc<Mutex<F>>,
    file: Option<VfFile>,
}

impl<F: FileSystem> FsFile<F> {
    fn raw(&self) -> &VfFile {
        self.file.as_ref().expect("open file")
    }

    pub fn read_at(&self, offset: u64, length: usize) -> FsRead<'_, F> {
        FsRead {
            file: self,
            offset: VfOffset::At(offset),
            length,
        }
    }

    pub fn write_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> FsWrite<'a, F> {
        FsWrite {
            file: self,
            offset: VfOffset::At(offset),
            data,
        }
    }

    pub fn read_at_into<'a>(&'a self, offset: u64, buffer: &'a mut [u8]) -> FsReadInto<'a, F> {
        FsReadInto {
            file: self,
            offset: VfOffset::At(offset),
            buffer,
        }
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .sync_data(self.raw())
            .map_err(io_error)
    }

    pub fn sync_all(&self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .sync_all(self.raw())
            .map_err(io_error)
    }

    pub fn close(mut self) -> io::Result<()> {
        let file = self.file.take().expect("open file");
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .close_one(&file)
            .map_err(io_error)
    }
}

impl<F: FileSystem> Read for FsFile<F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let result = self
            .inner
            .lock()
            .map_err(|_| poisoned())?
            .read_one(&ReadOp::new(
                self.raw().clone(),
                VfOffset::Cur,
                buffer.len(),
            ))
            .map_err(io_error)?;
        if result.data.len() > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backend returned more data than requested",
            ));
        }
        buffer[..result.data.len()].copy_from_slice(&result.data);
        Ok(result.data.len())
    }
}

impl<F: FileSystem> Write for FsFile<F> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let file = self.raw().clone();
        let write = WriteOpRef::new(&file, VfOffset::Cur, buffer);
        let result = self
            .inner
            .lock()
            .map_err(|_| poisoned())?
            .write_one(write)
            .map_err(io_error)?;
        if result.written > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backend reported writing more data than supplied",
            ));
        }
        Ok(result.written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}

impl<F: FileSystem> Seek for FsFile<F> {
    fn seek(&mut self, position: IoSeekFrom) -> io::Result<u64> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .seek_one(self.raw(), position)
            .map_err(io_error)
    }
}

/// Typed read against a file owned by an [`FsClient`].
pub struct FsRead<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    length: usize,
}

/// Typed borrowed write against a file owned by an [`FsClient`].
pub struct FsWrite<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    data: &'a [u8],
}

/// Typed vector read into caller-provided storage.
pub struct FsReadInto<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    buffer: &'a mut [u8],
}

impl<F: FileSystem> Drop for FsFile<F> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take()
            && let Ok(mut filesystem) = self.inner.lock()
        {
            let _ = filesystem.close_one(&file);
        }
    }
}
