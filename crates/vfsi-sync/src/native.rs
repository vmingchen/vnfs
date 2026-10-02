//! Rust-native scalar and vector contracts.
//!
//! [`VecFs`] remains the compatibility/backend implementation trait. New
//! applications should bound generic code by these smaller interfaces.

use crate::traits::{validate_read_into_results, validate_read_results, validate_write_results};
use crate::*;

/// Core synchronous scalar filesystem operations.
pub trait FileSystem {
    fn capabilities(&self) -> Capabilities;
    /// Read a complete opened file with a logical payload limit. Specialized backends
    /// override this to share their optimized vector whole-file path.
    fn read_file(&mut self, file: &VfFile, max_bytes: usize) -> VfResult<Vec<u8>> {
        let mut output = Vec::new();
        loop {
            let remaining = max_bytes.saturating_sub(output.len());
            let request = ReadOp::new(
                file.clone(),
                VfOffset::At(output.len() as u64),
                remaining.clamp(1, 1024 * 1024),
            );
            let result = self.read_one(&request)?;
            validate_read_results(
                "read_file",
                std::slice::from_ref(&request),
                std::slice::from_ref(&result),
            )?;
            if result.data.len() > remaining {
                return Err(VfError::client(0, libc::EFBIG as u32));
            }
            if result.data.is_empty() && !result.eof {
                return Err(VfError::client(0, ERR_IO));
            }
            output.extend_from_slice(&result.data);
            if result.eof {
                return Ok(output);
            }
        }
    }
    fn open_one(&mut self, request: &OpenRequest) -> VfResult<VfFile>;
    fn close_one(&mut self, file: &VfFile) -> VfResult<()>;
    fn sync_data(&mut self, file: &VfFile) -> VfResult<()>;
    fn sync_all(&mut self, file: &VfFile) -> VfResult<()>;
    fn read_one(&mut self, request: &ReadOp) -> VfResult<ReadResult>;
    /// Read directly into caller storage when supported by the backend.
    fn read_one_into(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        if request.length != buffer.len() {
            return Err(VfError::client(0, ERR_INVAL));
        }
        let result = self.read_one(request)?;
        if result.data.len() > buffer.len() {
            return Err(VfError::client(0, ERR_IO));
        }
        validate_read_results(
            "read_one_into",
            std::slice::from_ref(request),
            std::slice::from_ref(&result),
        )?;
        buffer[..result.data.len()].copy_from_slice(&result.data);
        Ok(ReadIntoResult {
            file: result.file,
            offset: result.offset,
            read: result.data.len(),
            eof: result.eof,
        })
    }
    fn write_one(&mut self, request: WriteOpRef<'_>) -> VfResult<WriteResult>;
    fn seek_one(&mut self, file: &VfFile, position: std::io::SeekFrom) -> VfResult<u64>;
    fn metadata(&mut self, query: MetadataQuery) -> VfResult<VfAttrs>;
    fn set_attributes(&mut self, update: SetAttributes) -> VfResult<()>;
}

/// Path metadata operations independent of open descriptors.
pub trait MetadataFileSystem: FileSystem {
    fn metadata_path(&mut self, path: &std::path::Path, follow: bool) -> VfResult<Metadata>;
    fn set_metadata_path(
        &mut self,
        path: &std::path::Path,
        update: MetadataUpdate,
        follow: bool,
    ) -> VfResult<()>;
}

/// Directory creation and enumeration.
pub trait DirectoryFileSystem: FileSystem {
    fn create_dir_one(&mut self, path: &std::path::Path, mode: u32) -> VfResult<()>;
    fn read_dir_one(
        &mut self,
        path: &std::path::Path,
        options: ReadDirOptions,
    ) -> VfResult<Vec<DirEntry>>;

    /// Return one bounded page without invoking application code under a
    /// mutable backend borrow. The continuation token is backend-specific.
    fn read_dir_page(
        &mut self,
        path: &std::path::Path,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)>;
}

/// Namespace mutations shared by files and directories.
pub trait NamespaceFileSystem: FileSystem {
    fn remove_one(&mut self, path: &std::path::Path, recursive: bool) -> VfResult<()>;
    /// Remove the contents of a directory, keeping the directory itself.
    fn remove_dir_contents(&mut self, path: &std::path::Path) -> VfResult<()>;
    fn rename_one(&mut self, from: &std::path::Path, to: &std::path::Path) -> VfResult<()>;
}

/// Symbolic and hard-link operations.
pub trait LinkFileSystem: FileSystem {
    fn symlink_one(&mut self, target: &std::path::Path, link: &std::path::Path) -> VfResult<()>;
    fn hard_link_one(&mut self, source: &std::path::Path, link: &std::path::Path) -> VfResult<()>;
    fn read_link_one(&mut self, path: &std::path::Path) -> VfResult<std::path::PathBuf>;
}

/// File-copy operations. Backends may accelerate this server-side.
pub trait CopyFileSystem: FileSystem {
    fn copy_one(&mut self, source: &std::path::Path, destination: &std::path::Path)
    -> VfResult<()>;
}

/// Complete scalar filesystem surface used by ordinary native applications.
pub trait NativeFileSystem:
    FileSystem
    + MetadataFileSystem
    + DirectoryFileSystem
    + NamespaceFileSystem
    + LinkFileSystem
    + CopyFileSystem
{
}

impl<T> NativeFileSystem for T where
    T: FileSystem
        + MetadataFileSystem
        + DirectoryFileSystem
        + NamespaceFileSystem
        + LinkFileSystem
        + CopyFileSystem
        + ?Sized
{
}

/// Optimized ordered vectors. This is deliberately separate from the scalar
/// contract so a backend can implement scalar semantics without pretending
/// to support native batching.
pub trait VectorFileSystem: FileSystem {
    /// Backend adapter used by [`FsClient::openv`](crate::FsClient::openv).
    ///
    /// Success must return exactly one handle per request, in request order.
    /// On failure, the implementation must release every handle it confirmed
    /// open before returning; the strict application API cannot receive a
    /// partial handle vector and perform that cleanup itself.
    #[doc(hidden)]
    fn open_many(&mut self, requests: &[OpenRequest]) -> VfResult<Vec<VfFile>>;

    /// Close each handle in request order and attribute failures accordingly.
    #[doc(hidden)]
    fn close_many(&mut self, files: &[VfFile]) -> VfResult<()>;

    /// Return exactly one result per request, in request order.
    #[doc(hidden)]
    fn read_many(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>>;

    /// Vector read into caller storage. Backends can override the copying
    /// fallback without changing the owned-result `read_many` contract.
    fn read_many_into(
        &mut self,
        requests: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        if requests.len() != buffers.len() {
            return Err(VfError::client(0, ERR_INVAL));
        }
        for (index, (request, buffer)) in requests.iter().zip(buffers.iter()).enumerate() {
            if request.length != buffer.len() {
                return Err(VfError::client(index, ERR_INVAL));
            }
        }
        let results = self.read_many(requests)?;
        validate_read_results("read_many_into", requests, &results)?;
        Ok(results
            .into_iter()
            .zip(buffers.iter_mut())
            .map(|(result, buffer)| {
                buffer[..result.data.len()].copy_from_slice(&result.data);
                ReadIntoResult {
                    file: result.file,
                    offset: result.offset,
                    read: result.data.len(),
                    eof: result.eof,
                }
            })
            .collect())
    }

    /// Return exactly one result per request, in request order.
    #[doc(hidden)]
    fn write_many(&mut self, requests: &[WriteOpRef<'_>]) -> VfResult<Vec<WriteResult>>;
}

fn metadata_mask() -> AttrMask {
    AttrMask::MODE
        | AttrMask::SIZE
        | AttrMask::NLINK
        | AttrMask::FILEID
        | AttrMask::UID
        | AttrMask::GID
        | AttrMask::ATIME
        | AttrMask::MTIME
        | AttrMask::CTIME
        | AttrMask::CHANGE
}

impl<T: VecFs + ?Sized> FileSystem for T {
    fn capabilities(&self) -> Capabilities {
        self.typed_capabilities()
    }

    fn read_file(&mut self, file: &VfFile, max_bytes: usize) -> VfResult<Vec<u8>> {
        self.read_allv_with_options(
            std::slice::from_ref(file),
            ReadAllOptions::new().max_total_bytes(max_bytes),
        )
        .and_then(|mut results| {
            if results.len() != 1 {
                return Err(VfError::transport(
                    None,
                    "read_file backend returned an invalid result count",
                ));
            }
            let data = results.pop().expect("validated result count");
            if data.len() > max_bytes {
                return Err(VfError::client(0, libc::EFBIG as u32));
            }
            Ok(data)
        })
    }

    fn open_one(&mut self, request: &OpenRequest) -> VfResult<VfFile> {
        self.open(
            request.path.as_path(),
            vfsi_core::open_flags_to_libc(request.flags)?,
            request.mode,
        )
        .map_err(|error| error.with_context("open", &request.path))
    }

    fn close_one(&mut self, file: &VfFile) -> VfResult<()> {
        self.close(file)
    }
    fn sync_data(&mut self, file: &VfFile) -> VfResult<()> {
        VecFs::sync_data(self, file)
    }
    fn sync_all(&mut self, file: &VfFile) -> VfResult<()> {
        VecFs::sync_all(self, file)
    }

    fn read_one(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        let mut results = self.readv(std::slice::from_ref(request))?;
        validate_read_results("read_one", std::slice::from_ref(request), &results)?;
        Ok(results.pop().expect("validated one result"))
    }

    fn read_one_into(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        let mut results = self.readv_into(std::slice::from_ref(request), &mut [buffer])?;
        validate_read_into_results("read_one_into", std::slice::from_ref(request), &results)?;
        Ok(results.pop().expect("validated one result"))
    }

    fn write_one(&mut self, request: WriteOpRef<'_>) -> VfResult<WriteResult> {
        let mut results = self.writev_borrowed(std::slice::from_ref(&request))?;
        validate_write_results("write_one", std::slice::from_ref(&request), &results)?;
        Ok(results.pop().expect("validated one result"))
    }

    fn seek_one(&mut self, file: &VfFile, position: std::io::SeekFrom) -> VfResult<u64> {
        let (offset, whence) = match position {
            std::io::SeekFrom::Start(offset) => (
                i64::try_from(offset).map_err(|_| VfError::failure(0, libc::EOVERFLOW as u32))?,
                SeekFrom::Set,
            ),
            std::io::SeekFrom::End(offset) => (offset, SeekFrom::End),
            std::io::SeekFrom::Current(offset) => (offset, SeekFrom::Cur),
        };
        u64::try_from(self.fseek(file, offset, whence)?).map_err(|_| VfError::failure(0, ERR_INVAL))
    }

    fn metadata(&mut self, query: MetadataQuery) -> VfResult<VfAttrs> {
        let path = query.file.path().map(std::path::Path::to_path_buf);
        let mut attrs = VfAttrs {
            file: query.file,
            masks: query.attributes,
            ..VfAttrs::default()
        };
        let result = if query.follow_symlinks {
            self.getattrsv(std::slice::from_mut(&mut attrs))
        } else {
            self.lgetattrsv(std::slice::from_mut(&mut attrs))
        };
        result.map_err(|error| match path {
            Some(path) => error.with_context("metadata", path),
            None => error,
        })?;
        Ok(attrs)
    }

    fn set_attributes(&mut self, update: SetAttributes) -> VfResult<()> {
        let follow = update.follow_symlinks;
        let attrs = update.into_legacy();
        let path = attrs.file.path().map(std::path::Path::to_path_buf);
        let result = if follow {
            self.setattrsv(&[attrs])
        } else {
            self.lsetattrsv(&[attrs])
        };
        result.map_err(|error| match path {
            Some(path) => error.with_context("set_attributes", path),
            None => error,
        })
    }
}

impl<T: VecFs + ?Sized> MetadataFileSystem for T {
    fn metadata_path(&mut self, path: &std::path::Path, follow: bool) -> VfResult<Metadata> {
        let mut attributes = VfAttrs {
            file: VfFile::from_os_path(path),
            masks: metadata_mask(),
            ..VfAttrs::default()
        };
        let result = if follow {
            self.getattrsv(std::slice::from_mut(&mut attributes))
        } else {
            self.lgetattrsv(std::slice::from_mut(&mut attributes))
        };
        result
            .map_err(|error| error.with_context("metadata", path))
            .map(|()| vfsi_core::metadata_from_attrs(attributes))
    }

    fn set_metadata_path(
        &mut self,
        path: &std::path::Path,
        update: MetadataUpdate,
        follow: bool,
    ) -> VfResult<()> {
        let mut attributes = SetAttributes::new(VfFile::from_os_path(path));
        attributes.follow_symlinks = follow;
        attributes.mode = update.permissions.map(Permissions::mode);
        attributes.size = update.len;
        attributes.atime = update.accessed.map(system_time_parts).transpose()?;
        attributes.mtime = update.modified.map(system_time_parts).transpose()?;
        self.set_attributes(attributes)
            .map_err(|error| error.with_context("set_metadata", path))
    }
}

fn system_time_parts(time: std::time::SystemTime) -> VfResult<(i64, u32)> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => Ok((
            i64::try_from(duration.as_secs())
                .map_err(|_| VfError::client(0, libc::EOVERFLOW as u32))?,
            duration.subsec_nanos(),
        )),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs())
                .map_err(|_| VfError::client(0, libc::EOVERFLOW as u32))?;
            if duration.subsec_nanos() == 0 {
                Ok((-seconds, 0))
            } else {
                let seconds = seconds
                    .checked_add(1)
                    .ok_or_else(|| VfError::client(0, libc::EOVERFLOW as u32))?;
                Ok((-seconds, 1_000_000_000 - duration.subsec_nanos()))
            }
        }
    }
}

impl<T: VecFs + ?Sized> DirectoryFileSystem for T {
    fn create_dir_one(&mut self, path: &std::path::Path, mode: u32) -> VfResult<()> {
        self.mkdir(path, mode)
            .map_err(|error| error.with_context("create_dir", path))
    }

    fn read_dir_one(
        &mut self,
        path: &std::path::Path,
        options: ReadDirOptions,
    ) -> VfResult<Vec<DirEntry>> {
        let requested = options.entry_limit().saturating_add(1);
        let entries = self
            .listdir(path, metadata_mask(), requested, false)
            .map_err(|error| error.with_context("read_dir", path))?;
        if entries.len() > options.entry_limit() {
            return Err(VfError::failure(options.entry_limit(), libc::EFBIG as u32)
                .with_context("read_dir", path));
        }
        let mut path_bytes = 0usize;
        entries
            .into_iter()
            .enumerate()
            .map(|(index, attributes)| {
                let entry_path = attributes
                    .file
                    .path()
                    .map(std::path::Path::to_path_buf)
                    .ok_or_else(|| VfError::client(index, ERR_IO).with_context("read_dir", path))?;
                path_bytes = path_bytes
                    .checked_add(entry_path.as_os_str().len())
                    .ok_or_else(|| {
                        VfError::failure(index, libc::EFBIG as u32).with_context("read_dir", path)
                    })?;
                if path_bytes > options.path_byte_limit() {
                    return Err(
                        VfError::failure(index, libc::EFBIG as u32).with_context("read_dir", path)
                    );
                }
                Ok(DirEntry::new(
                    entry_path,
                    vfsi_core::metadata_from_attrs(attributes),
                ))
            })
            .collect()
    }

    fn read_dir_page(
        &mut self,
        path: &std::path::Path,
        cursor: Option<DirPageCursor>,
        page_size: usize,
        max_entries: usize,
    ) -> VfResult<(Vec<DirEntry>, Option<DirPageCursor>)> {
        let (attributes, next) = self
            .listdir_page(path, metadata_mask(), cursor, page_size, max_entries)
            .map_err(|error| error.with_context("visit_dir", path))?;
        if attributes.len() > page_size {
            return Err(VfError::transport(
                None,
                "listdir_page returned more entries than requested",
            ));
        }
        let entries = attributes
            .into_iter()
            .enumerate()
            .map(|(index, attributes)| {
                let entry_path = attributes
                    .file
                    .path()
                    .map(std::path::Path::to_path_buf)
                    .ok_or_else(|| {
                        VfError::client(index, ERR_IO).with_context("visit_dir", path)
                    })?;
                Ok(DirEntry::new(
                    entry_path,
                    vfsi_core::metadata_from_attrs(attributes),
                ))
            })
            .collect::<VfResult<Vec<_>>>()?;
        Ok((entries, next))
    }
}

impl<T: VecFs + ?Sized> NamespaceFileSystem for T {
    fn remove_one(&mut self, path: &std::path::Path, recursive: bool) -> VfResult<()> {
        self.rm(&[path], recursive)
            .map_err(|error| error.with_context("remove", path))
    }

    fn remove_dir_contents(&mut self, path: &std::path::Path) -> VfResult<()> {
        self.rm_contents(path)
            .map_err(|error| error.with_context("remove_dir_contents", path))
    }

    fn rename_one(&mut self, from: &std::path::Path, to: &std::path::Path) -> VfResult<()> {
        self.renamev(&[(VfFile::from_os_path(from), VfFile::from_os_path(to))])
            .map_err(|error| error.with_context("rename", from))
    }
}

impl<T: VecFs + ?Sized> LinkFileSystem for T {
    fn symlink_one(&mut self, target: &std::path::Path, link: &std::path::Path) -> VfResult<()> {
        self.symlink(target, link)
            .map_err(|error| error.with_context("symlink", link))
    }

    fn hard_link_one(&mut self, source: &std::path::Path, link: &std::path::Path) -> VfResult<()> {
        self.hardlinkv(&[source], &[link])
            .map_err(|error| error.with_context("hard_link", link))
    }

    fn read_link_one(&mut self, path: &std::path::Path) -> VfResult<std::path::PathBuf> {
        self.readlink(path)
            .map(bytes_to_path)
            .map_err(|error| error.with_context("read_link", path))
    }
}

#[cfg(unix)]
fn bytes_to_path(bytes: Vec<u8>) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> std::path::PathBuf {
    String::from_utf8_lossy(&bytes).into_owned().into()
}

impl<T: VecFs + ?Sized> CopyFileSystem for T {
    fn copy_one(
        &mut self,
        source: &std::path::Path,
        destination: &std::path::Path,
    ) -> VfResult<()> {
        self.copyv(&[ExtentPair::from_os_paths(source, 0, destination, 0, None)])
            .map_err(|error| error.with_context("copy", source))
    }
}

impl<T: VecFs + ?Sized> VectorFileSystem for T {
    fn open_many(&mut self, requests: &[OpenRequest]) -> VfResult<Vec<VfFile>> {
        let paths: Vec<&std::path::Path> = requests
            .iter()
            .map(|request| request.path.as_path())
            .collect();
        let flags = translate_open_flags(requests)?;
        let modes: Vec<u32> = requests.iter().map(|request| request.mode).collect();
        VecFs::openv(self, &paths, &flags, &modes)
    }

    fn close_many(&mut self, files: &[VfFile]) -> VfResult<()> {
        self.closev(files)
    }

    fn read_many(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        self.readv(requests)
    }

    fn read_many_into(
        &mut self,
        requests: &[ReadOp],
        buffers: &mut [&mut [u8]],
    ) -> VfResult<Vec<ReadIntoResult>> {
        self.readv_into(requests, buffers)
    }

    fn write_many(&mut self, requests: &[WriteOpRef<'_>]) -> VfResult<Vec<WriteResult>> {
        self.writev_borrowed(requests)
    }
}

fn translate_open_flags(requests: &[OpenRequest]) -> VfResult<Vec<i32>> {
    requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            vfsi_core::open_flags_to_libc(request.flags).map_err(|error| error.with_index(index))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_open_flags_retain_their_request_index() {
        let error = translate_open_flags(&[
            OpenRequest::new("/valid", OpenFlags::READ),
            OpenRequest::new("/invalid", OpenFlags::empty()),
        ])
        .unwrap_err();
        assert_eq!(error.index(), Some(1));
    }
}
