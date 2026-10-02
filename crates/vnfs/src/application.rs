//! Protocol-independent application contracts. Backend implementer traits
//! remain in `backend`; generic application helpers need only these traits.

use crate::{
    DirectoryListing, Metadata, OpenRequest, ReadAllOptions, ReadIntoResult, ReadResult,
    ResourceLimits, Result, WriteResult,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Owned application file. Borrowed requests preserve the file's lifetime;
/// clients validate connection ownership before dispatching a vector.
pub trait FileHandle: Read + Write + Seek {
    type ReadRequest<'a>
    where
        Self: 'a;
    type ReadIntoRequest<'a>
    where
        Self: 'a;
    type WriteRequest<'a>
    where
        Self: 'a;
    fn path(&self) -> &Path;
    fn metadata(&self) -> Result<Metadata>;
    fn read_request_at(&self, offset: u64, length: usize) -> Self::ReadRequest<'_>;
    fn read_request_at_into<'a>(
        &'a self,
        offset: u64,
        buffer: &'a mut [u8],
    ) -> Self::ReadIntoRequest<'a>;
    fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> Self::WriteRequest<'a>;
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize>;
    fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize>;
    fn read_native(&mut self, buffer: &mut [u8]) -> Result<usize>;
    fn write_native(&mut self, buffer: &[u8]) -> Result<usize>;
    fn seek_native(&mut self, position: SeekFrom) -> Result<u64>;
    fn sync_data(&self) -> Result<()>;
    fn sync_all(&self) -> Result<()>;
    fn try_close(&mut self) -> Result<()>;
    fn is_closed(&self) -> bool;
    fn close(self) -> Result<()>
    where
        Self: Sized;
}

/// Shared scalar and bulk application contract for direct and routed clients.
/// Construction, builders and protocol-specific diagnostics stay concrete.
/// Vectors are strict, ordered results on success, not transactions: errors
/// can follow partially completed requests and never imply rollback.
/// This uses borrowed request GATs, so it is for static generic dispatch, not
/// `dyn Client`. It adds no boxing, data copies or serial-loop fallbacks.
pub trait Client {
    type File: FileHandle + 'static;
    fn limits(&self) -> ResourceLimits;
    fn open(&self, path: impl AsRef<Path>) -> Result<Self::File>;
    fn open_with(&self, request: OpenRequest) -> Result<Self::File>;
    fn create(&self, path: impl AsRef<Path>) -> Result<Self::File>;
    fn openv(&self, requests: &[OpenRequest]) -> Result<Vec<Self::File>>;
    fn readv<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::ReadRequest<'a>],
    ) -> Result<Vec<ReadResult>>;
    fn readv_into<'a>(
        &self,
        requests: &mut [<Self::File as FileHandle>::ReadIntoRequest<'a>],
    ) -> Result<Vec<ReadIntoResult>>;
    fn writev<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
    ) -> Result<Vec<WriteResult>>;
    fn write_allv<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
    ) -> Result<Vec<WriteResult>>;
    fn try_closev(&self, files: &mut [Self::File]) -> Result<()>;
    fn closev(&self, files: Vec<Self::File>) -> Result<()>;
    fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>>;
    fn read_with_limit(&self, path: impl AsRef<Path>, bytes: usize) -> Result<Vec<u8>>;
    fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Vec<u8>>>;
    fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: ReadAllOptions,
    ) -> Result<Vec<Vec<u8>>>;
    fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()>;
    fn write_files<P: AsRef<Path>, B: AsRef<[u8]>>(&self, entries: &[(P, B)]) -> Result<()>;
    fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata>;
    fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata>;
    fn create_dir(&self, path: impl AsRef<Path>) -> Result<()>;
    fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<()>;
    fn remove_file(&self, path: impl AsRef<Path>) -> Result<()>;
    fn remove_dir(&self, path: impl AsRef<Path>) -> Result<()>;
    fn remove_dir_all(&self, path: impl AsRef<Path>) -> Result<()>;
    fn remove_dir_contents(&self, path: impl AsRef<Path>) -> Result<()>;
    fn rename(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()>;
    fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()>;
    fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>>;
    fn read_dirs_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        fields: crate::MetadataFields,
        options: crate::ReadDirOptions,
    ) -> Result<Vec<DirectoryListing>>;
    fn symlink_metadata_with_fields(
        &self,
        path: impl AsRef<Path>,
        fields: crate::MetadataFields,
    ) -> Result<Metadata>;
    fn symlink_metadatav(&self, paths: &[&Path]) -> Result<Vec<Metadata>>;
    fn copy_files<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()>;
    fn remove_paths_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
        options: crate::RemoveOptions,
    ) -> Result<()>;
    fn walk_with_options(
        &self,
        path: impl AsRef<Path>,
        fields: crate::MetadataFields,
        options: crate::WalkOptions,
    ) -> Result<Vec<DirectoryListing>>;
    fn visit_walk_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::WalkOptions,
        callback: impl FnMut(&crate::DirEntry) -> Result<bool>,
    ) -> Result<crate::TraversalCompletion>;
    fn read_stream_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::ReadStreamOptions,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::StreamCompletion>;
}

macro_rules! file_methods {
    ($file:ty) => {
        fn path(&self) -> &Path {
            <$file>::path(self)
        }
        fn metadata(&self) -> Result<Metadata> {
            <$file>::metadata(self)
        }
        fn read_request_at(&self, offset: u64, length: usize) -> Self::ReadRequest<'_> {
            <$file>::read_request_at(self, offset, length)
        }
        fn read_request_at_into<'a>(
            &'a self,
            offset: u64,
            buffer: &'a mut [u8],
        ) -> Self::ReadIntoRequest<'a> {
            <$file>::read_request_at_into(self, offset, buffer)
        }
        fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> Self::WriteRequest<'a> {
            <$file>::write_request_at(self, offset, data)
        }
        fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize> {
            <$file>::read_at(self, buffer, offset)
        }
        fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize> {
            <$file>::write_at(self, buffer, offset)
        }
        fn read_native(&mut self, buffer: &mut [u8]) -> Result<usize> {
            <$file>::read_native(self, buffer)
        }
        fn write_native(&mut self, buffer: &[u8]) -> Result<usize> {
            <$file>::write_native(self, buffer)
        }
        fn seek_native(&mut self, position: SeekFrom) -> Result<u64> {
            <$file>::seek_native(self, position)
        }
        fn sync_data(&self) -> Result<()> {
            <$file>::sync_data(self)
        }
        fn sync_all(&self) -> Result<()> {
            <$file>::sync_all(self)
        }
        fn try_close(&mut self) -> Result<()> {
            <$file>::try_close(self)
        }
        fn is_closed(&self) -> bool {
            <$file>::is_closed(self)
        }
        fn close(self) -> Result<()> {
            <$file>::close(self)
        }
    };
}

impl<F: vfsi_sync::FileSystem> FileHandle for crate::FsFile<F> {
    type ReadRequest<'a>
        = crate::FsRead<'a, F>
    where
        Self: 'a;
    type ReadIntoRequest<'a>
        = crate::FsReadInto<'a, F>
    where
        Self: 'a;
    type WriteRequest<'a>
        = crate::FsWrite<'a, F>
    where
        Self: 'a;
    file_methods!(crate::FsFile<F>);
}

macro_rules! client_methods {
    ($client:ty, $receiver:path) => {
        fn limits(&self) -> ResourceLimits {
            <$client>::limits($receiver(self))
        }
        fn open(&self, path: impl AsRef<Path>) -> Result<Self::File> {
            <$client>::open($receiver(self), path)
        }
        fn open_with(&self, request: OpenRequest) -> Result<Self::File> {
            <$client>::open_with($receiver(self), request)
        }
        fn create(&self, path: impl AsRef<Path>) -> Result<Self::File> {
            <$client>::create($receiver(self), path)
        }
        fn openv(&self, requests: &[OpenRequest]) -> Result<Vec<Self::File>> {
            <$client>::openv($receiver(self), requests)
        }
        fn readv<'a>(
            &self,
            requests: &[<Self::File as FileHandle>::ReadRequest<'a>],
        ) -> Result<Vec<ReadResult>> {
            <$client>::readv($receiver(self), requests)
        }
        fn readv_into<'a>(
            &self,
            requests: &mut [<Self::File as FileHandle>::ReadIntoRequest<'a>],
        ) -> Result<Vec<ReadIntoResult>> {
            <$client>::readv_into($receiver(self), requests)
        }
        fn writev<'a>(
            &self,
            requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
        ) -> Result<Vec<WriteResult>> {
            <$client>::writev($receiver(self), requests)
        }
        fn write_allv<'a>(
            &self,
            requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
        ) -> Result<Vec<WriteResult>> {
            <$client>::write_allv($receiver(self), requests)
        }
        fn try_closev(&self, files: &mut [Self::File]) -> Result<()> {
            <$client>::try_closev($receiver(self), files)
        }
        fn closev(&self, files: Vec<Self::File>) -> Result<()> {
            <$client>::closev($receiver(self), files)
        }
        fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
            <$client>::read($receiver(self), path)
        }
        fn read_with_limit(&self, path: impl AsRef<Path>, bytes: usize) -> Result<Vec<u8>> {
            <$client>::read_with_limit($receiver(self), path, bytes)
        }
        fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Vec<u8>>> {
            <$client>::read_files($receiver(self), paths)
        }
        fn read_files_with_options<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: ReadAllOptions,
        ) -> Result<Vec<Vec<u8>>> {
            <$client>::read_files_with_options($receiver(self), paths, options)
        }
        fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
            <$client>::write($receiver(self), path, data)
        }
        fn write_files<P: AsRef<Path>, B: AsRef<[u8]>>(&self, entries: &[(P, B)]) -> Result<()> {
            <$client>::write_files($receiver(self), entries)
        }
        fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata> {
            <$client>::metadata($receiver(self), path)
        }
        fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata> {
            <$client>::symlink_metadata($receiver(self), path)
        }
        fn create_dir(&self, path: impl AsRef<Path>) -> Result<()> {
            <$client>::create_dir($receiver(self), path)
        }
        fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
            <$client>::create_dir_all($receiver(self), path)
        }
        fn remove_file(&self, path: impl AsRef<Path>) -> Result<()> {
            <$client>::remove_file($receiver(self), path)
        }
        fn remove_dir(&self, path: impl AsRef<Path>) -> Result<()> {
            <$client>::remove_dir($receiver(self), path)
        }
        fn remove_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
            <$client>::remove_dir_all($receiver(self), path)
        }
        fn remove_dir_contents(&self, path: impl AsRef<Path>) -> Result<()> {
            <$client>::remove_dir_contents($receiver(self), path)
        }
        fn rename(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
            <$client>::rename($receiver(self), source, destination)
        }
        fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
            <$client>::copy($receiver(self), source, destination)
        }
        fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>> {
            <$client>::read_dirs($receiver(self), paths)
        }
        fn read_dirs_with_options<P: AsRef<Path>>(
            &self,
            paths: &[P],
            fields: crate::MetadataFields,
            options: crate::ReadDirOptions,
        ) -> Result<Vec<DirectoryListing>> {
            <$client>::read_dirs_with_options($receiver(self), paths, fields, options)
        }
        fn symlink_metadata_with_fields(
            &self,
            path: impl AsRef<Path>,
            fields: crate::MetadataFields,
        ) -> Result<Metadata> {
            <$client>::symlink_metadata_with_fields($receiver(self), path, fields)
        }
        fn symlink_metadatav(&self, paths: &[&Path]) -> Result<Vec<Metadata>> {
            <$client>::symlink_metadatav($receiver(self), paths)
        }
        fn copy_files<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::copy_files($receiver(self), pairs)
        }
        fn remove_paths_with_options<P: AsRef<Path>>(
            &self,
            paths: &[P],
            recursive: bool,
            options: crate::RemoveOptions,
        ) -> Result<()> {
            <$client>::remove_paths_with_options($receiver(self), paths, recursive, options)
        }
        fn walk_with_options(
            &self,
            path: impl AsRef<Path>,
            fields: crate::MetadataFields,
            options: crate::WalkOptions,
        ) -> Result<Vec<DirectoryListing>> {
            <$client>::walk_with_options($receiver(self), path, fields, options)
        }
        fn visit_walk_with_options(
            &self,
            path: impl AsRef<Path>,
            options: crate::WalkOptions,
            callback: impl FnMut(&crate::DirEntry) -> Result<bool>,
        ) -> Result<crate::TraversalCompletion> {
            <$client>::visit_walk_with_options($receiver(self), path, options, callback)
        }
        fn read_stream_with_options(
            &self,
            path: impl AsRef<Path>,
            options: crate::ReadStreamOptions,
            callback: impl FnMut(u64, &[u8]) -> Result<bool>,
        ) -> Result<crate::StreamCompletion> {
            <$client>::read_stream_with_options($receiver(self), path, options, callback)
        }
    };
}

impl<F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static>
    Client for crate::FsClient<F>
{
    type File = crate::FsFile<F>;
    client_methods!(crate::FsClient<F>, std::convert::identity);
}

#[cfg(all(feature = "auto", target_os = "linux"))]
mod routed {
    use super::*;
    impl FileHandle for crate::AutoFile {
        type ReadRequest<'a> = crate::AutoRead<'a>;
        type ReadIntoRequest<'a> = crate::AutoReadInto<'a>;
        type WriteRequest<'a> = crate::AutoWrite<'a>;
        file_methods!(crate::AutoFile);
    }
    impl Client for crate::AutoClient {
        type File = crate::AutoFile;
        client_methods!(crate::AutoClient, std::convert::identity);
    }
    impl Client for crate::Auto {
        type File = crate::AutoFile;
        client_methods!(crate::AutoClient, std::ops::Deref::deref);
    }
    impl Client for crate::Mounted {
        type File = crate::FsFile<vfsi_local::DummyVecFs>;
        client_methods!(
            crate::FsClient<vfsi_local::DummyVecFs>,
            std::ops::Deref::deref
        );
    }
}
