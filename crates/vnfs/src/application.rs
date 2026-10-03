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
    /// Borrowed positional read request; constructing it performs no I/O.
    type ReadRequest<'a>
    where
        Self: 'a;
    /// Borrowed positional request into a caller-owned destination buffer.
    type ReadIntoRequest<'a>
    where
        Self: 'a;
    /// Borrowed positional write request, retaining the input data's lifetime.
    type WriteRequest<'a>
    where
        Self: 'a;
    /// Diagnostic name captured at open; not current path or object identity.
    fn path(&self) -> &Path;
    /// Query the opened object, even if its original pathname was renamed.
    fn metadata(&self) -> Result<Metadata>;
    /// Prepare a non-cursor-changing read for this handle's owning client.
    fn read_request_at(&self, offset: u64, length: usize) -> Self::ReadRequest<'_>;
    /// Borrow both the handle and destination until the vector call completes.
    fn read_request_at_into<'a>(
        &'a self,
        offset: u64,
        buffer: &'a mut [u8],
    ) -> Self::ReadIntoRequest<'a>;
    /// Prepare a positional write; the data remains borrowed, not copied here.
    fn write_request_at<'a>(&'a self, offset: u64, data: &'a [u8]) -> Self::WriteRequest<'a>;
    /// Read up to the buffer's length without changing the cursor; may be short.
    fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize>;
    /// Write up to the input's length without changing the cursor; may be short.
    fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize>;
    /// Cursor-based read retaining structured errors, unlike the `Read` adapter.
    fn read_native(&mut self, buffer: &mut [u8]) -> Result<usize>;
    /// Collect from the current cursor with a caller-selected payload budget.
    /// A failed read may advance the cursor; exceeding the limit is an error,
    /// not successful truncation. Standard `Read::read_to_end` is not bounded.
    fn read_to_end_with_limit(&mut self, max_bytes: usize) -> Result<Vec<u8>>;
    /// Cursor-based possibly short write retaining structured errors.
    fn write_native(&mut self, buffer: &[u8]) -> Result<usize>;
    /// Change the cursor, retaining structured errors unlike `Seek`.
    fn seek_native(&mut self, position: SeekFrom) -> Result<u64>;
    /// Request durability of file data; errors are not discarded.
    fn sync_data(&self) -> Result<()>;
    /// Request durability of file data and metadata supported by the backend.
    fn sync_all(&self) -> Result<()>;
    /// Close without surrendering cleanup ownership on failure. After an
    /// ambiguous failure, reconcile/close rather than resume ordinary I/O.
    fn try_close(&mut self) -> Result<()>;
    /// Local close ownership only, not proof of remote liveness after failure.
    fn is_closed(&self) -> bool;
    /// Consuming close: on failure the handle is lost and Drop retries cleanup
    /// best-effort. Prefer `try_close` when cleanup failures need reconciliation.
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
    /// Incremental no-follow traversal with enter/leave events and pruning.
    /// Listings are bounded by the remaining aggregate budget; the callback
    /// runs before entering each directory, so pruning avoids its listing.
    fn walk_events_with_options(
        &self,
        root: impl AsRef<Path>,
        fields: crate::MetadataFields,
        options: crate::WalkOptions,
        sort_by_name: bool,
        callback: impl FnMut(&crate::WalkEvent) -> Result<crate::WalkControl>,
    ) -> Result<crate::TraversalCompletion> {
        let root = root.as_ref();
        let fields = fields | crate::MetadataFields::MODE;
        let metadata = self.symlink_metadata_with_fields(root, fields)?;
        vfsi_sync::walk_events(
            crate::DirEntry::new(root.to_path_buf(), metadata),
            options,
            sort_by_name,
            |path, limits| {
                let mut listings = self.read_dirs_with_options(&[path], fields, limits)?;
                if listings.len() != 1 {
                    return Err(crate::Error::transport(
                        None,
                        "invalid directory result count",
                    ));
                }
                Ok(listings.remove(0).entries)
            },
            callback,
        )
    }
    /// Owned handle; vectors must contain handles belonging to this client.
    type File: FileHandle + 'static;
    /// Collection/batch defaults, not a process memory cap or file-reader cap.
    fn limits(&self) -> ResourceLimits;
    /// Open read-only. Paths are relative to this client's configured namespace.
    fn open(&self, path: impl AsRef<Path>) -> Result<Self::File>;
    /// Open with an explicit access/create/truncate request; effects are eager.
    fn open_with(&self, request: OpenRequest) -> Result<Self::File>;
    /// Create or truncate a file and open for writing; does not create parents.
    fn create(&self, path: impl AsRef<Path>) -> Result<Self::File>;
    /// Strict ordered OPEN results. Failure releases returned handles but cannot
    /// undo files created/truncated earlier; `index()` is not a progress count.
    fn openv(&self, requests: &[OpenRequest]) -> Result<Vec<Self::File>>;
    /// Positional possibly short reads in input order, with explicit EOF.
    /// Aggregate owned results are bounded by the client's read policy.
    fn readv<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::ReadRequest<'a>],
    ) -> Result<Vec<ReadResult>>;
    /// Positional reads into borrowed buffers. Aggregate buffer lengths are
    /// bounded too, since a backend may require an owned-buffer fallback.
    fn readv_into<'a>(
        &self,
        requests: &mut [<Self::File as FileHandle>::ReadIntoRequest<'a>],
    ) -> Result<Vec<ReadIntoResult>>;
    /// Possibly short writes in input order on success. Failure can follow
    /// partial mutations; neither a rollback nor an automatic retry is promised.
    fn writev<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
    ) -> Result<Vec<WriteResult>>;
    /// Finish short positional writes after whole-batch local preflight.
    /// Backend failures can still follow mutations; aliasing paths are the
    /// caller's responsibility and do not imply transactional ordering.
    fn write_allv<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
    ) -> Result<Vec<WriteResult>>;
    /// Retain handles for failed cleanup; completed groups can already be
    /// closed. A lost reply leaves remote state ambiguous, not safely open.
    fn try_closev(&self, files: &mut [Self::File]) -> Result<()>;
    /// Consume all handles. Errors cannot return cleanup ownership; Drop is
    /// best-effort. Prefer `try_closev` when close errors require reconciliation.
    fn closev(&self, files: Vec<Self::File>) -> Result<()>;
    /// Collect one complete opened object within the client's read limit.
    fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>>;
    /// Override that scalar payload limit; an oversized file is an error.
    fn read_with_limit(&self, path: impl AsRef<Path>, bytes: usize) -> Result<Vec<u8>>;
    /// Whole-file path reads, bounded in aggregate; no cross-file snapshot.
    fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Vec<u8>>>;
    /// Whole-file vector reads with an explicit combined payload budget.
    fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: ReadAllOptions,
    ) -> Result<Vec<Vec<u8>>>;
    /// Replace a file completely, creating/truncating eagerly; not atomic replace.
    fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()>;
    /// Replace files in vector phases. Success completes payloads; errors may
    /// follow create/truncate/write effects and never authorize blind replay.
    fn write_files<P: AsRef<Path>, B: AsRef<[u8]>>(&self, entries: &[(P, B)]) -> Result<()>;
    /// Query a path following its final symlink; unavailable fields remain None.
    fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata>;
    /// Query the final symlink itself instead of following it.
    fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata>;
    /// Create one directory; its parent must exist.
    fn create_dir(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Create missing parents; an error can leave some directories created.
    fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Remove one file or symlink, not the symlink target.
    fn remove_file(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Remove one empty directory; not a recursive operation.
    fn remove_dir(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Recursively remove a tree without following directory symlinks.
    /// Path-based removal is not a security sandbox or an atomic transaction.
    fn remove_dir_all(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Recursively empty a directory while keeping its root.
    fn remove_dir_contents(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Rename within supported namespaces; cross-filesystem moves can fail.
    fn rename(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()>;
    /// Copy a file's contents; this is not recursive tree copying.
    fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()>;
    /// Collect directory listings under one aggregate entry/path-byte policy.
    fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>>;
    /// Select metadata fields and override aggregate listing bounds.
    fn read_dirs_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        fields: crate::MetadataFields,
        options: crate::ReadDirOptions,
    ) -> Result<Vec<DirectoryListing>>;
    /// No-follow metadata with explicit fields; absent values remain None.
    fn symlink_metadata_with_fields(
        &self,
        path: impl AsRef<Path>,
        fields: crate::MetadataFields,
    ) -> Result<Metadata>;
    /// Strict no-follow metadata results in input order.
    fn symlink_metadatav(&self, paths: &[&Path]) -> Result<Vec<Metadata>>;
    /// Strict file-copy batches; a failed call can have copied earlier files.
    fn copy_files<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()>;
    /// Ordered removal with explicit retry/error policy; no rollback promise.
    fn remove_paths_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
        options: crate::RemoveOptions,
    ) -> Result<()>;
    /// Collect a bounded tree, without following symlinks; no snapshot promise.
    fn walk_with_options(
        &self,
        path: impl AsRef<Path>,
        fields: crate::MetadataFields,
        options: crate::WalkOptions,
    ) -> Result<Vec<DirectoryListing>>;
    /// Visit bounded directory pages outside the backend lock. `Break(())`
    /// stops the entire walk, not one subtree. Order is backend-defined.
    fn visit_walk_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::WalkOptions,
        callback: impl FnMut(&crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion>;
    /// Visit one directory; `Break(())` returns Stopped, exhaustion returns
    /// Complete, and callback errors propagate. The callback may reenter.
    fn visit_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::ReadDirOptions,
        callback: impl FnMut(crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion>;
    /// Stream from offset zero outside the backend lock. `false` stops after
    /// the delivered chunk; completion reports the next offset. Not a snapshot.
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
        fn read_to_end_with_limit(&mut self, max_bytes: usize) -> Result<Vec<u8>> {
            <$file>::read_to_end_with_limit(self, max_bytes)
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

impl<F: vfsi_sync::FileSystem> FileHandle for vfsi_sync::FsFile<F> {
    type ReadRequest<'a>
        = vfsi_sync::FsRead<'a, F>
    where
        Self: 'a;
    type ReadIntoRequest<'a>
        = vfsi_sync::FsReadInto<'a, F>
    where
        Self: 'a;
    type WriteRequest<'a>
        = vfsi_sync::FsWrite<'a, F>
    where
        Self: 'a;
    file_methods!(vfsi_sync::FsFile<F>);
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
            callback: impl FnMut(&crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
        ) -> Result<crate::TraversalCompletion> {
            <$client>::visit_walk_with_options($receiver(self), path, options, callback)
        }
        fn visit_dir_with_options(
            &self,
            path: impl AsRef<Path>,
            options: crate::ReadDirOptions,
            callback: impl FnMut(crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
        ) -> Result<crate::TraversalCompletion> {
            <$client>::visit_dir_with_options($receiver(self), path, options, callback)
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
    Client for vfsi_sync::FsClient<F>
{
    type File = vfsi_sync::FsFile<F>;
    client_methods!(vfsi_sync::FsClient<F>, std::convert::identity);
}

#[cfg(feature = "nfs")]
impl FileHandle for crate::NfsFile {
    type ReadRequest<'a> = crate::NfsRead<'a>;
    type ReadIntoRequest<'a> = crate::NfsReadInto<'a>;
    type WriteRequest<'a> = crate::NfsWrite<'a>;
    file_methods!(crate::NfsFile);
}
#[cfg(feature = "nfs")]
impl Client for crate::NfsClient {
    type File = crate::NfsFile;
    client_methods!(crate::NfsClient, std::convert::identity);
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
        type File = crate::MountedFile;
        client_methods!(crate::Mounted, std::convert::identity);
    }
    impl FileHandle for crate::MountedFile {
        type ReadRequest<'a> = crate::MountedRead<'a>;
        type ReadIntoRequest<'a> = crate::MountedReadInto<'a>;
        type WriteRequest<'a> = crate::MountedWrite<'a>;
        file_methods!(crate::MountedFile);
    }
}
