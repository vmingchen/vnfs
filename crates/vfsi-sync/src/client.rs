//! Owned, shareable synchronous client and file handles.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom as IoSeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::traits::{validate_read_results, validate_write_results};
use crate::{
    AttrMask, Capabilities, CopyFileSystem, DEFAULT_READ_MAX_BYTES, DirEntry, DirectoryFileSystem,
    FileSystem, LinkFileSystem, Metadata, MetadataFileSystem, MetadataQuery, MetadataUpdate,
    NamespaceFileSystem, OpenFlags, OpenRequest, Permissions, ReadOp, ReadResult, SetAttributes,
    VectorFileSystem, VfError, VfFile, VfOffset, VfResult, WriteOpRef, WriteResult,
};

fn io_error(error: VfError) -> io::Error {
    error.into()
}

fn poisoned() -> VfError {
    VfError::client(0, crate::ERR_IO)
}

fn wrong_result_count(operation: &str, expected: usize, actual: usize) -> VfError {
    VfError::transport(
        None,
        format!("{operation} backend returned {actual} results for {expected} requests"),
    )
}

/// Cloneable owner of one synchronous backend connection.
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

impl<F> fmt::Debug for FsClient<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FsClient").finish_non_exhaustive()
    }
}

impl<F> FsClient<F> {
    pub fn new(filesystem: F) -> Self {
        Self {
            inner: Arc::new(Mutex::new(filesystem)),
        }
    }

    fn lock(&self) -> VfResult<std::sync::MutexGuard<'_, F>> {
        self.inner.lock().map_err(|_| poisoned())
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
    /// Open a path read-only.
    pub fn open(&self, path: impl AsRef<Path>) -> VfResult<FsFile<F>> {
        self.open_with(OpenRequest::new(path.as_ref(), OpenFlags::READ))
    }

    pub fn open_with(&self, request: OpenRequest) -> VfResult<FsFile<F>> {
        let file = self.lock()?.open_one(&request)?;
        Ok(FsFile {
            inner: Arc::clone(&self.inner),
            file: Some(file),
            path: request.path,
        })
    }

    pub fn open_options(&self) -> OpenOptions<'_, F> {
        OpenOptions::new(self)
    }

    pub fn create(&self, path: impl AsRef<Path>) -> VfResult<FsFile<F>> {
        self.open_with(OpenRequest::new(
            path.as_ref(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        ))
    }

    pub fn read(&self, path: impl AsRef<Path>) -> VfResult<Vec<u8>> {
        self.read_with_limit(path, DEFAULT_READ_MAX_BYTES)
    }

    /// Read one complete file while limiting the returned allocation.
    ///
    /// Use [`FsFile::read_native`] or [`std::io::Read`] to stream files that
    /// should not be held in one allocation.
    pub fn read_with_limit(&self, path: impl AsRef<Path>, max_bytes: usize) -> VfResult<Vec<u8>> {
        let path = path.as_ref();
        let mut file = self.open(path)?;
        let mut output = Vec::new();
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let remaining = max_bytes.saturating_sub(output.len());
            let request = if remaining == 0 {
                1
            } else {
                remaining.min(chunk.len())
            };
            let read = file.read_native(&mut chunk[..request])?;
            if read == 0 {
                break;
            }
            if read > remaining {
                return Err(VfError::failure(0, libc::EFBIG as u32).with_context("read", path));
            }
            output.extend_from_slice(&chunk[..read]);
        }
        file.close()?;
        Ok(output)
    }

    pub fn read_to_string(&self, path: impl AsRef<Path>) -> VfResult<String> {
        self.read_to_string_with_limit(path, DEFAULT_READ_MAX_BYTES)
    }

    /// Read one complete UTF-8 file with a caller-selected allocation limit.
    pub fn read_to_string_with_limit(
        &self,
        path: impl AsRef<Path>,
        max_bytes: usize,
    ) -> VfResult<String> {
        let path = path.as_ref();
        String::from_utf8(self.read_with_limit(path, max_bytes)?)
            .map_err(|_| VfError::client(0, crate::ERR_INVAL).with_context("read_to_string", path))
    }

    pub fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> VfResult<()> {
        let path = path.as_ref();
        let mut file = self.create(path)?;
        let mut written = 0;
        while written < data.len() {
            let count = file.write_native(&data[written..])?;
            if count == 0 {
                return Err(VfError::client(0, crate::ERR_IO).with_context("write", path));
            }
            written += count;
        }
        file.close()
    }
}

impl<F: MetadataFileSystem> FsClient<F> {
    pub fn metadata(&self, path: impl AsRef<Path>) -> VfResult<Metadata> {
        self.lock()?.metadata_path(path.as_ref(), true)
    }

    pub fn symlink_metadata(&self, path: impl AsRef<Path>) -> VfResult<Metadata> {
        self.lock()?.metadata_path(path.as_ref(), false)
    }

    pub fn set_metadata(&self, path: impl AsRef<Path>) -> SetMetadata<'_, F> {
        SetMetadata {
            client: self,
            path: path.as_ref().to_path_buf(),
            update: MetadataUpdate::new(),
            follow: true,
        }
    }
}

impl<F: DirectoryFileSystem> FsClient<F> {
    pub fn create_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        self.create_dir_with_mode(path, 0o777)
    }

    pub fn create_dir_with_mode(&self, path: impl AsRef<Path>, mode: u32) -> VfResult<()> {
        self.lock()?.create_dir_one(path.as_ref(), mode)
    }

    pub fn read_dir(&self, path: impl AsRef<Path>) -> VfResult<Vec<DirEntry>> {
        self.lock()?.read_dir_one(path.as_ref())
    }
}

impl<F: DirectoryFileSystem + MetadataFileSystem> FsClient<F> {
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        let mut current = PathBuf::new();
        for component in path.components() {
            match component {
                Component::RootDir => {
                    current.push(Path::new("/"));
                    continue;
                }
                Component::Normal(part) => current.push(part),
                Component::CurDir => continue,
                Component::ParentDir => {
                    // Preserve `..` so the backend resolves it relative to
                    // its own rooted namespace. Lexically popping `/` would
                    // accidentally turn a later absolute component into a
                    // current-directory-relative path.
                    current.push("..");
                    continue;
                }
                Component::Prefix(_) => return Err(VfError::client(0, crate::ERR_INVAL)),
            }
            match self.create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.err_no() == crate::ERR_EXIST => {
                    if !self.metadata(&current)?.is_dir() {
                        return Err(VfError::client(0, crate::ERR_NOTDIR)
                            .with_context("create_dir_all", &current));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

impl<F: NamespaceFileSystem + MetadataFileSystem> FsClient<F> {
    fn removal_metadata(&self, path: &Path, operation: &'static str) -> VfResult<Metadata> {
        let mut filesystem = self.lock()?;
        let follow = !filesystem.capabilities().contains(Capabilities::LSTAT);
        filesystem
            .metadata_path(path, follow)
            .map_err(|error| error.with_context(operation, path))
    }

    pub fn remove_file(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        if self.removal_metadata(path, "remove_file")?.is_dir() {
            return Err(VfError::client(0, crate::ERR_ISDIR).with_context("remove_file", path));
        }
        self.lock()?.remove_one(path, false)
    }

    pub fn remove_dir(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        if !self.removal_metadata(path, "remove_dir")?.is_dir() {
            return Err(VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir", path));
        }
        self.lock()?.remove_one(path, false)
    }

    pub fn remove_dir_all(&self, path: impl AsRef<Path>) -> VfResult<()> {
        let path = path.as_ref();
        if !self.removal_metadata(path, "remove_dir_all")?.is_dir() {
            return Err(VfError::client(0, crate::ERR_NOTDIR).with_context("remove_dir_all", path));
        }
        self.lock()?.remove_one(path, true)
    }

    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.rename_one(from.as_ref(), to.as_ref())
    }
}

impl<F: LinkFileSystem> FsClient<F> {
    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.symlink_one(target.as_ref(), link.as_ref())
    }

    pub fn hard_link(&self, source: impl AsRef<Path>, link: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.hard_link_one(source.as_ref(), link.as_ref())
    }

    pub fn read_link(&self, path: impl AsRef<Path>) -> VfResult<PathBuf> {
        self.lock()?.read_link_one(path.as_ref())
    }
}

impl<F: CopyFileSystem> FsClient<F> {
    pub fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> VfResult<()> {
        self.lock()?.copy_one(source.as_ref(), destination.as_ref())
    }
}

impl<F: VectorFileSystem> FsClient<F> {
    /// Open an ordered vector of files.
    ///
    /// Success returns one RAII handle per request. Failure returns no
    /// handles; VFSI does not promise transactional rollback of other
    /// filesystem effects such as file creation.
    pub fn openv(&self, requests: &[OpenRequest]) -> VfResult<Vec<FsFile<F>>> {
        let mut filesystem = self.lock()?;
        let files = filesystem.open_many(requests).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("openv", &request.path)
                })
        })?;
        if files.len() != requests.len() {
            let error = wrong_result_count("openv", requests.len(), files.len());
            for file in &files {
                let _ = filesystem.close_one(file);
            }
            return Err(error);
        }
        drop(filesystem);
        Ok(files
            .into_iter()
            .zip(requests)
            .map(|(file, request)| FsFile {
                inner: Arc::clone(&self.inner),
                file: Some(file),
                path: request.path.clone(),
            })
            .collect())
    }

    /// Close a group of files through one vector operation.
    ///
    /// On failure, each handle remains armed for best-effort cleanup on drop;
    /// explicitly closed prefix handles may consequently receive a harmless
    /// second close attempt.
    pub fn closev(&self, mut files: Vec<FsFile<F>>) -> VfResult<()> {
        for (index, file) in files.iter().enumerate() {
            self.validate_owner(file, index)?;
        }
        let descriptors: Vec<VfFile> = files.iter().map(|file| file.raw().clone()).collect();
        self.lock()?.close_many(&descriptors).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| files.get(index))
                .map_or(error.clone(), |file| {
                    error.with_context("closev", file.path())
                })
        })?;
        for file in &mut files {
            file.file = None;
        }
        Ok(())
    }

    pub fn readv(&self, requests: &[FsRead<'_, F>]) -> VfResult<Vec<ReadResult>> {
        let reads = self.read_ops(requests)?;
        let results = self.lock()?.read_many(&reads).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("readv", request.file.path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count("readv", requests.len(), results.len()));
        }
        validate_read_results("readv", &reads, &results).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("readv", request.file.path())
                })
        })?;
        Ok(results)
    }

    fn read_ops(&self, requests: &[FsRead<'_, F>]) -> VfResult<Vec<ReadOp>> {
        let mut reads = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().enumerate() {
            self.validate_owner(request.file, index)?;
            reads.push(ReadOp::new(
                request.file.raw().clone(),
                request.offset,
                request.length,
            ));
        }
        Ok(reads)
    }

    pub fn readv_into(&self, requests: &mut [FsReadInto<'_, F>]) -> VfResult<Vec<usize>> {
        let mut reads = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().enumerate() {
            self.validate_owner(request.file, index)?;
            reads.push(ReadOp::new(
                request.file.raw().clone(),
                request.offset,
                request.buffer.len(),
            ));
        }
        let results = self.lock()?.read_many(&reads).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("readv_into", request.file.path())
                })
        })?;
        validate_read_results("readv_into", &reads, &results).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("readv_into", request.file.path())
                })
        })?;
        let mut lengths = Vec::with_capacity(results.len());
        for (index, (request, result)) in requests.iter_mut().zip(results).enumerate() {
            if result.data.len() > request.buffer.len() {
                return Err(VfError::client(index, crate::ERR_IO)
                    .with_context("readv_into", request.file.path()));
            }
            request.buffer[..result.data.len()].copy_from_slice(&result.data);
            lengths.push(result.data.len());
        }
        Ok(lengths)
    }

    pub fn writev(&self, requests: &[FsWrite<'_, F>]) -> VfResult<Vec<WriteResult>> {
        let writes = self.write_ops(requests)?;
        let results = self.lock()?.write_many(&writes).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("writev", request.file.path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count("writev", requests.len(), results.len()));
        }
        validate_write_results("writev", &writes, &results).map_err(|error| {
            error
                .index_opt()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("writev", request.file.path())
                })
        })?;
        Ok(results)
    }

    fn write_ops<'a>(&self, requests: &'a [FsWrite<'a, F>]) -> VfResult<Vec<WriteOpRef<'a>>> {
        let mut writes = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().enumerate() {
            self.validate_owner(request.file, index)?;
            writes.push(WriteOpRef::new(
                request.file.raw(),
                request.offset,
                request.data,
            ));
        }
        Ok(writes)
    }

    fn validate_owner(&self, file: &FsFile<F>, index: usize) -> VfResult<()> {
        if Arc::ptr_eq(&self.inner, &file.inner) {
            Ok(())
        } else {
            Err(VfError::client(index, crate::ERR_INVAL))
        }
    }
}

/// `std::fs::OpenOptions`-style builder tied to an [`FsClient`].
pub struct OpenOptions<'a, F: FileSystem> {
    client: &'a FsClient<F>,
    flags: OpenFlags,
    mode: u32,
}

impl<F: FileSystem> Clone for OpenOptions<'_, F> {
    fn clone(&self) -> Self {
        Self {
            client: self.client,
            flags: self.flags,
            mode: self.mode,
        }
    }
}

impl<F: FileSystem> fmt::Debug for OpenOptions<'_, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenOptions")
            .field("flags", &self.flags)
            .field("mode", &format_args!("{:#o}", self.mode))
            .finish()
    }
}

impl<'a, F: FileSystem> OpenOptions<'a, F> {
    fn new(client: &'a FsClient<F>) -> Self {
        Self {
            client,
            flags: OpenFlags::empty(),
            mode: 0o666,
        }
    }

    pub fn read(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::READ, enabled);
        self
    }

    pub fn write(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::WRITE, enabled);
        self
    }

    pub fn append(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::APPEND, enabled);
        self
    }

    pub fn truncate(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::TRUNCATE, enabled);
        self
    }

    pub fn create(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE, enabled);
        self
    }

    pub fn create_new(&mut self, enabled: bool) -> &mut Self {
        self.flags.set(OpenFlags::CREATE_NEW, enabled);
        self
    }

    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> VfResult<FsFile<F>> {
        self.client
            .open_with(OpenRequest::new(path.as_ref(), self.flags).mode(self.mode))
    }
}

impl<F: VectorFileSystem> OpenOptions<'_, F> {
    pub fn openv<P: AsRef<Path>>(&self, paths: &[P]) -> VfResult<Vec<FsFile<F>>> {
        let requests: Vec<OpenRequest> = paths
            .iter()
            .map(|path| OpenRequest::new(path.as_ref(), self.flags).mode(self.mode))
            .collect();
        self.client.openv(&requests)
    }
}

/// Builder for an atomic metadata update request.
pub struct SetMetadata<'a, F: MetadataFileSystem> {
    client: &'a FsClient<F>,
    path: PathBuf,
    update: MetadataUpdate,
    follow: bool,
}

impl<F: MetadataFileSystem> Clone for SetMetadata<'_, F> {
    fn clone(&self) -> Self {
        Self {
            client: self.client,
            path: self.path.clone(),
            update: self.update.clone(),
            follow: self.follow,
        }
    }
}

impl<F: MetadataFileSystem> fmt::Debug for SetMetadata<'_, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetMetadata")
            .field("path", &self.path)
            .field("update", &self.update)
            .field("follow", &self.follow)
            .finish_non_exhaustive()
    }
}

impl<F: MetadataFileSystem> SetMetadata<'_, F> {
    pub fn permissions(&mut self, permissions: Permissions) -> &mut Self {
        self.update.permissions = Some(permissions);
        self
    }

    pub fn len(&mut self, len: u64) -> &mut Self {
        self.update.len = Some(len);
        self
    }

    pub fn accessed(&mut self, accessed: SystemTime) -> &mut Self {
        self.update.accessed = Some(accessed);
        self
    }

    pub fn modified(&mut self, modified: SystemTime) -> &mut Self {
        self.update.modified = Some(modified);
        self
    }

    pub fn follow_symlinks(&mut self, follow: bool) -> &mut Self {
        self.follow = follow;
        self
    }

    pub fn apply(&self) -> VfResult<()> {
        self.client
            .lock()?
            .set_metadata_path(&self.path, self.update.clone(), self.follow)
    }
}

/// Owned RAII file which does not borrow the client.
pub struct FsFile<F: FileSystem> {
    inner: Arc<Mutex<F>>,
    file: Option<VfFile>,
    path: PathBuf,
}

impl<F: FileSystem> fmt::Debug for FsFile<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsFile")
            .field("path", &self.path)
            .field("is_open", &self.file.is_some())
            .finish()
    }
}

impl<F: FileSystem> FsFile<F> {
    fn raw(&self) -> &VfFile {
        self.file.as_ref().expect("open file")
    }

    /// Positional read which does not alter the file cursor.
    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> VfResult<usize> {
        self.read_into(buffer, VfOffset::At(offset))
    }

    /// Positional write which does not alter the file cursor.
    pub fn write_at(&self, buffer: &[u8], offset: u64) -> VfResult<usize> {
        self.write_from(buffer, VfOffset::At(offset))
    }

    pub fn read_request_at(&self, offset: u64, length: usize) -> FsRead<'_, F> {
        FsRead {
            file: self,
            offset: VfOffset::At(offset),
            length,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Query metadata for the open object without resolving its path again.
    pub fn metadata(&self) -> VfResult<Metadata> {
        let attributes = AttrMask::MODE
            | AttrMask::SIZE
            | AttrMask::NLINK
            | AttrMask::FILEID
            | AttrMask::UID
            | AttrMask::GID
            | AttrMask::ATIME
            | AttrMask::MTIME
            | AttrMask::CTIME
            | AttrMask::CHANGE;
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .metadata(MetadataQuery::new(self.raw().clone(), attributes))
            .map(Metadata::from)
            .map_err(|error| error.with_context("metadata", &self.path))
    }

    /// Truncate or extend the open file.
    pub fn set_len(&self, len: u64) -> VfResult<()> {
        let mut update = SetAttributes::new(self.raw().clone());
        update.size = Some(len);
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .set_attributes(update)
            .map_err(|error| error.with_context("set_len", &self.path))
    }

    /// Change permissions on the open file.
    pub fn set_permissions(&self, permissions: Permissions) -> VfResult<()> {
        let mut update = SetAttributes::new(self.raw().clone());
        update.mode = Some(permissions.mode());
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .set_attributes(update)
            .map_err(|error| error.with_context("set_permissions", &self.path))
    }

    pub fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> FsWrite<'a, F> {
        FsWrite {
            file: self,
            offset: VfOffset::At(offset),
            data,
        }
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

    pub fn read_native(&mut self, buffer: &mut [u8]) -> VfResult<usize> {
        self.read_into(buffer, VfOffset::Cur)
    }

    pub fn write_native(&mut self, buffer: &[u8]) -> VfResult<usize> {
        self.write_from(buffer, VfOffset::Cur)
    }

    fn read_into(&self, buffer: &mut [u8], offset: VfOffset) -> VfResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let result = self
            .inner
            .lock()
            .map_err(|_| poisoned())?
            .read_one(&ReadOp::new(self.raw().clone(), offset, buffer.len()))
            .map_err(|error| error.with_context("read", &self.path))?;
        if result.data.len() > buffer.len() {
            return Err(VfError::client(0, crate::ERR_IO).with_context("read", &self.path));
        }
        buffer[..result.data.len()].copy_from_slice(&result.data);
        Ok(result.data.len())
    }

    fn write_from(&self, buffer: &[u8], offset: VfOffset) -> VfResult<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let file = self.raw().clone();
        let result = self
            .inner
            .lock()
            .map_err(|_| poisoned())?
            .write_one(WriteOpRef::new(&file, offset, buffer))
            .map_err(|error| error.with_context("write", &self.path))?;
        if result.written > buffer.len() {
            return Err(VfError::client(0, crate::ERR_IO).with_context("write", &self.path));
        }
        Ok(result.written)
    }

    pub fn sync_data(&self) -> VfResult<()> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .sync_data(self.raw())
            .map_err(|error| error.with_context("sync_data", &self.path))
    }

    pub fn sync_all(&self) -> VfResult<()> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .sync_all(self.raw())
            .map_err(|error| error.with_context("sync_all", &self.path))
    }

    pub fn close(mut self) -> VfResult<()> {
        let file = self.file.as_ref().expect("open file");
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .close_one(file)
            .map_err(|error| error.with_context("close", &self.path))?;
        self.file = None;
        Ok(())
    }

    /// Seek while retaining [`VfError`] protocol and path information.
    pub fn seek_native(&mut self, position: IoSeekFrom) -> VfResult<u64> {
        self.inner
            .lock()
            .map_err(|_| poisoned())?
            .seek_one(self.raw(), position)
            .map_err(|error| error.with_context("seek", &self.path))
    }
}

impl<F: FileSystem> Read for FsFile<F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_native(buffer).map_err(io_error)
    }
}

impl<F: FileSystem> Write for FsFile<F> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_native(buffer).map_err(io_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sync_data().map_err(io_error)
    }
}

impl<F: FileSystem> Seek for FsFile<F> {
    fn seek(&mut self, position: IoSeekFrom) -> io::Result<u64> {
        self.seek_native(position).map_err(io_error)
    }
}

/// Typed read request for [`FsClient::readv`].
pub struct FsRead<'a, F: FileSystem> {
    file: &'a FsFile<F>,
    offset: VfOffset,
    length: usize,
}

/// Typed borrowed write request for [`FsClient::writev`].
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
