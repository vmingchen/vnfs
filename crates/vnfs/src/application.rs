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
///
/// # Choosing an operation
///
/// | Task | Start with |
/// | --- | --- |
/// | Complete small files | [`read_files`](Self::read_files), [`write_files`](Self::write_files) |
/// | Repeated/range I/O on owned handles | [`openv`](Self::openv), [`readv_into`](Self::readv_into), [`write_allv`](Self::write_allv) |
/// | A large file without collecting it | [`read_stream_with_options`](Self::read_stream_with_options) |
/// | Metadata for many directories | [`read_dirs_with_options`](Self::read_dirs_with_options) |
/// | Incremental traversal or pruning | [`visit_walk_with_options`](Self::visit_walk_with_options), [`walk_events_with_options`](Self::walk_events_with_options) |
///
/// Concrete clients such as `NfsClient` expose these operations as inherent
/// methods too. Use this trait for backend-independent application helpers:
///
/// ```no_run
/// use vnfs::Client;
/// fn load_parts(fs: &impl Client) -> vnfs::Result<Vec<Vec<u8>>> {
///     fs.read_files(&["/file-1", "/file-2"])
/// }
/// ```
///
/// # Shared semantics
///
/// Paths use the client's namespace, not necessarily the host filesystem.
/// A leading `/` refers to the configured root; this is not security confinement.
/// Handles in a vector must belong to this client's connection. Successful
/// vector results match input order; one vector may span several compounds.
/// A failed mutation can have partial effects. An error's index identifies an
/// input when known, **not** a committed-prefix count. Preserve the structured
/// [`crate::Error`] rather than blindly replaying a failed write.
///
/// Allocation/traversal defaults come from [`limits`](Self::limits). Explicit
/// per-call options override them; neither is a process-wide memory cap.
/// Calls are synchronous. Callback methods invoke user code outside the backend
/// lock and propagate callback errors, but do not provide a filesystem snapshot.
/// Writes are not automatically durable: use [`FileHandle::sync_data`] or
/// [`FileHandle::sync_all`] on an open handle when required. Drop closes handles
/// best-effort; explicit close methods let applications observe cleanup errors.
pub trait Client {
    /// Incremental no-follow traversal with enter/leave events and pruning.
    /// Listings are bounded by the remaining aggregate budget; the callback
    /// runs before entering each directory, so pruning avoids its listing.
    ///
    /// `Enter`/`Leave` surround directories (including pruned ones); symlinks
    /// are `Entry` events, never followed. `SkipSubtree` is meaningful on `Enter`;
    /// `Stop` stops the entire walk. `sort_by_name` sorts siblings, not the whole
    /// tree. Limits charge fetched entries, including the root, even if pruned.
    /// Unlike a flat visit, this retains a bounded active frontier/listing.
    ///
    /// # Example: skip `.git` before listing it
    ///
    /// ```no_run
    /// use vnfs::{Client, MetadataFields, WalkControl, WalkEventKind, WalkOptions};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let completion = fs.walk_events_with_options(
    ///     "/project", MetadataFields::MODE, WalkOptions::new(), true,
    ///     |event| {
    ///         if event.kind == WalkEventKind::Enter
    ///             && event.entry.file_name() == Some(std::ffi::OsStr::new(".git")) {
    ///             return Ok(WalkControl::SkipSubtree);
    ///         }
    ///         println!("{:?}: {}", event.kind, event.entry.path().display());
    ///         Ok(WalkControl::Continue)
    ///     },
    /// )?;
    /// # let _ = completion;
    /// # Ok(())
    /// # }
    /// ```
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
    ///
    /// This only inspects policy; it performs no filesystem I/O. Configure a
    /// concrete client's builder or `with_limits` to change its defaults.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) {
    /// let limits = fs.limits();
    /// println!("owned-read budget: {} bytes", limits.max_read_bytes);
    /// println!("walk depth: {}", limits.max_walk_depth);
    /// # }
    /// ```
    fn limits(&self) -> ResourceLimits;
    /// Open read-only. Paths are relative to this client's configured namespace.
    ///
    /// Use [`open_with`](Self::open_with) for write/create flags, or
    /// [`openv`](Self::openv) to batch many opens. The returned handle implements
    /// `std::io::Read`/`Write`/`Seek`; use native methods to retain structured errors.
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let mut file = fs.open("/config")?;
    /// let mut header = [0_u8; 128];
    /// let result = file.read_native(&mut header); // May be short.
    /// let close = file.close();
    /// let bytes = result?;
    /// close?;
    /// println!("read {bytes} bytes");
    /// # Ok(())
    /// # }
    /// ```
    fn open(&self, path: impl AsRef<Path>) -> Result<Self::File>;
    /// Open with an explicit access/create/truncate request; effects are eager.
    ///
    /// Creation/truncation occurs at open, not on the first write. `CREATE_NEW`
    /// rejects an existing path. This does not create missing parents.
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle, OpenFlags, OpenRequest};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let file = fs.open_with(OpenRequest::new(
    ///     "/new-file", OpenFlags::WRITE | OpenFlags::CREATE_NEW,
    /// ).mode(0o600))?;
    /// file.close()?;
    /// # Ok(())
    /// # }
    /// ```
    fn open_with(&self, request: OpenRequest) -> Result<Self::File>;
    /// Create or truncate a file and open for writing; does not create parents.
    ///
    /// Existing contents are discarded immediately. For exclusive creation,
    /// use [`open_with`](Self::open_with) with `WRITE | CREATE_NEW` instead.
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.write_allv(&[file.write_request_at(0, b"complete contents")]);
    /// let close = file.close();
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    fn create(&self, path: impl AsRef<Path>) -> Result<Self::File>;
    /// Strict ordered OPEN results. Failure releases returned handles but cannot
    /// undo files created/truncated earlier; `index()` is not a progress count.
    ///
    /// Different requests can use different flags/modes. All resulting handles
    /// belong to this client. Prefer this to a scalar loop when opening a cohort.
    ///
    /// ```no_run
    /// use vnfs::{Client, OpenFlags, OpenRequest};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let files = fs.openv(&[
    ///     OpenRequest::new("/file-1", OpenFlags::READ),
    ///     OpenRequest::new("/file-2", OpenFlags::READ),
    /// ])?;
    /// // files[0] corresponds to file-1; files[1] to file-2.
    /// fs.closev(files)?;
    /// # Ok(())
    /// # }
    /// ```
    fn openv(&self, requests: &[OpenRequest]) -> Result<Vec<Self::File>>;
    /// Positional possibly short reads in input order, with explicit EOF.
    /// Aggregate owned results are bounded by the client's read policy.
    ///
    /// Each request borrows a handle; its absolute offset does not advance the
    /// handle's cursor. A short result without `eof` is not completion: advance
    /// by the returned data length when reading the remaining range. For reuse
    /// of caller storage, prefer [`readv_into`](Self::readv_into).
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle, OpenFlags, OpenRequest};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let files = fs.openv(&[
    ///     OpenRequest::new("/file-1", OpenFlags::READ),
    ///     OpenRequest::new("/file-2", OpenFlags::READ),
    /// ])?;
    /// let result = fs.readv(&[
    ///     files[0].read_request_at(0, 4096),
    ///     files[1].read_request_at(8192, 4096),
    /// ]);
    /// let close = fs.closev(files);
    /// let results = result?;
    /// close?;
    /// for result in results {
    ///     println!("offset={} bytes={} eof={}", result.offset, result.data.len(), result.eof);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn readv<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::ReadRequest<'a>],
    ) -> Result<Vec<ReadResult>>;
    /// Positional reads into borrowed buffers. Aggregate buffer lengths are
    /// bounded too, since a backend may require an owned-buffer fallback.
    ///
    /// Use [`FileHandle::read_request_at_into`] to borrow each destination.
    /// The request list must be mutable; the file cursor is unchanged. After
    /// the requests go out of scope, inspect only `buffer[..result.read]`:
    /// trailing bytes are not part of this read. Results match request order.
    /// A short read without `eof` needs a follow-up at `offset + read`, not at
    /// `offset + buffer.len()`. Caller storage does not promise zero-copy RPC I/O.
    ///
    /// # Example: two ranges, one vector, reusable caller buffers
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle, OpenFlags, OpenRequest};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let files = fs.openv(&[
    ///     OpenRequest::new("/file-1", OpenFlags::READ),
    ///     OpenRequest::new("/file-2", OpenFlags::READ),
    /// ])?;
    /// let mut first = [0_u8; 4096];
    /// let mut second = [0_u8; 4096];
    /// let result = {
    ///     let mut requests = [
    ///         files[0].read_request_at_into(0, &mut first),
    ///         files[1].read_request_at_into(8192, &mut second),
    ///     ];
    ///     fs.readv_into(&mut requests)
    /// }; // Releases the mutable buffer borrows.
    /// let close = fs.closev(files); // Attempt cleanup even when reading failed.
    /// let results = result?;
    /// close?;
    /// println!("first: {:?}", &first[..results[0].read]);
    /// println!("second: {:?}", &second[..results[1].read]);
    /// # Ok(())
    /// # }
    /// ```
    fn readv_into<'a>(
        &self,
        requests: &mut [<Self::File as FileHandle>::ReadIntoRequest<'a>],
    ) -> Result<Vec<ReadIntoResult>>;
    /// Possibly short writes in input order on success. Failure can follow
    /// partial mutations; neither a rollback nor an automatic retry is promised.
    ///
    /// Requests borrow payloads and preserve each handle's cursor. Inspect
    /// `written` rather than assuming the full payload was accepted. Use
    /// [`write_allv`](Self::write_allv) for complete writes across the cohort.
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.writev(&[file.write_request_at(0, b"payload")]);
    /// let close = file.close();
    /// let results = result?;
    /// close?;
    /// println!("accepted {} of 7 bytes", results[0].written);
    /// # Ok(())
    /// # }
    /// ```
    fn writev<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
    ) -> Result<Vec<WriteResult>>;
    /// Finish short positional writes after whole-batch local preflight.
    /// Backend failures can still follow mutations; aliasing paths are the
    /// caller's responsibility and do not imply transactional ordering.
    ///
    /// On success each request's complete payload has been written. Successful
    /// short writes are completed at their remaining offsets; this does not
    /// authorize replay after an ambiguous failure or guarantee durability.
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle, OpenFlags, OpenRequest};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let files = fs.openv(&[
    ///     OpenRequest::new("/file-1", OpenFlags::WRITE),
    ///     OpenRequest::new("/file-2", OpenFlags::WRITE),
    /// ])?;
    /// let result = fs.write_allv(&[
    ///     files[0].write_request_at(0, b"hello"),
    ///     files[1].write_request_at(4096, b"world"),
    /// ]);
    /// let close = fs.closev(files);
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    fn write_allv<'a>(
        &self,
        requests: &[<Self::File as FileHandle>::WriteRequest<'a>],
    ) -> Result<Vec<WriteResult>>;
    /// Retain handles for failed cleanup; completed groups can already be
    /// closed. A lost reply leaves remote state ambiguous, not safely open.
    ///
    /// Unlike [`closev`](Self::closev), this borrows the handles. After an error,
    /// `is_closed()` reports local cleanup ownership only; reconcile/close the
    /// retained handles rather than resuming ordinary I/O as if nothing happened.
    ///
    /// ```no_run
    /// use vnfs::{Client, FileHandle, OpenFlags, OpenRequest};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let mut files = fs.openv(&[OpenRequest::new("/file-1", OpenFlags::READ)])?;
    /// if let Err(error) = fs.try_closev(&mut files) {
    ///     let retained = files.iter().filter(|file| !file.is_closed()).count();
    ///     eprintln!("{retained} handles retain cleanup ownership: {error}");
    ///     return Err(error); // Drop performs best-effort cleanup, not reconciliation.
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn try_closev(&self, files: &mut [Self::File]) -> Result<()>;
    /// Consume all handles. Errors cannot return cleanup ownership; Drop is
    /// best-effort. Prefer `try_closev` when close errors require reconciliation.
    ///
    /// This releases all local handles even on failure; it does not promise
    /// every remote CLOSE succeeded. Closing alone is not a durability barrier.
    ///
    /// ```no_run
    /// use vnfs::{Client, OpenFlags, OpenRequest};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let files = fs.openv(&[OpenRequest::new("/config", OpenFlags::READ)])?;
    /// fs.closev(files)?; // Observe a close error instead of discarding it in Drop.
    /// # Ok(())
    /// # }
    /// ```
    fn closev(&self, files: Vec<Self::File>) -> Result<()>;
    /// Collect one complete opened object within the client's read limit.
    ///
    /// The handle is closed internally. Exceeding `limits().max_read_bytes`
    /// returns an error, not truncated success; the default is 16 MiB. For many
    /// files use [`read_files`](Self::read_files); for large files use
    /// [`read_stream_with_options`](Self::read_stream_with_options).
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let contents = fs.read("/config")?;
    /// println!("{}", String::from_utf8_lossy(&contents));
    /// # Ok(())
    /// # }
    /// ```
    fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>>;
    /// Override that scalar payload limit; an oversized file is an error.
    ///
    /// `bytes` limits returned file data for this call, not peak process memory.
    /// Prefer streaming instead of raising the limit for arbitrarily large files.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let config = fs.read_with_limit("/config", 64 * 1024)?;
    /// println!("{} bytes", config.len());
    /// # Ok(())
    /// # }
    /// ```
    fn read_with_limit(&self, path: impl AsRef<Path>, bytes: usize) -> Result<Vec<u8>>;
    /// Whole-file path reads, bounded in aggregate; no cross-file snapshot.
    ///
    /// Results match input order. The client's `max_read_bytes` is the combined
    /// budget across all files (16 MiB by default), not a per-file allowance.
    /// Small files may share a compound; larger cohorts can span several RPCs.
    /// No file handles need to be opened or closed by the caller.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let paths = ["/file-1", "/file-2"];
    /// let contents = fs.read_files(&paths)?;
    /// for (path, bytes) in paths.iter().zip(contents) {
    ///     println!("{path}: {} bytes", bytes.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Vec<u8>>>;
    /// Whole-file vector reads with an explicit combined payload budget.
    ///
    /// Options override the client's read default for this call. Exceeding the
    /// aggregate budget is an error; no partial contents are returned as success.
    /// Use bounded cohorts or streaming if the total size is not known.
    ///
    /// ```no_run
    /// use vnfs::{Client, ReadAllOptions};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let contents = fs.read_files_with_options(
    ///     &["/file-1", "/file-2"],
    ///     ReadAllOptions::new().max_total_bytes(1024 * 1024),
    /// )?;
    /// # let _ = contents;
    /// # Ok(())
    /// # }
    /// ```
    fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: ReadAllOptions,
    ) -> Result<Vec<Vec<u8>>>;
    /// Replace a file completely, creating/truncating eagerly; not atomic replace.
    ///
    /// The parent must exist. Success writes all bytes and closes the internal
    /// handle; failure may leave a created, truncated, or partially written file.
    /// For exclusive creation or durability control use an explicit open handle.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.write("/output", b"replacement contents")?;
    /// # Ok(())
    /// # }
    /// ```
    fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()>;
    /// Replace files in vector phases. Success completes payloads; errors may
    /// follow create/truncate/write effects and never authorize blind replay.
    ///
    /// Parents must exist. Payloads are completed via vector open/write/close
    /// phases, not a scalar write loop or necessarily a single RPC. No atomic
    /// replacement, cross-file transaction, or durability guarantee is provided.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.write_files(&[
    ///     ("/file-1", b"hello".as_slice()),
    ///     ("/file-2", b"world".as_slice()),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    fn write_files<P: AsRef<Path>, B: AsRef<[u8]>>(&self, entries: &[(P, B)]) -> Result<()>;
    /// Query a path following its final symlink; unavailable fields remain None.
    ///
    /// To inspect the symlink itself use [`symlink_metadata`](Self::symlink_metadata).
    /// To identify an already opened object after rename, use [`FileHandle::metadata`].
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let metadata = fs.metadata("/file-1")?;
    /// println!("{} bytes; directory={}", metadata.len(), metadata.is_dir());
    /// # Ok(())
    /// # }
    /// ```
    fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata>;
    /// Query the final symlink itself instead of following it.
    ///
    /// This does not prevent following symlinks in ancestor components and is
    /// not a race-free namespace confinement primitive.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let metadata = fs.symlink_metadata("/link-or-file")?;
    /// println!("symlink={}", metadata.is_symlink());
    /// # Ok(())
    /// # }
    /// ```
    fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata>;
    /// Create one directory; its parent must exist.
    ///
    /// An existing entry is an error, even if already a directory. Use
    /// [`create_dir_all`](Self::create_dir_all) for missing parents/idempotent
    /// directory setup, or [`create_dirs`](Self::create_dirs) for independent siblings.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.create_dir("/fresh-directory")?;
    /// # Ok(())
    /// # }
    /// ```
    fn create_dir(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Strict vector directory creation. Parents must already exist; errors
    /// can follow completed mutations, and do not imply rollback.
    ///
    /// Submit independent siblings together. Do not rely on creating a parent
    /// and its child in the same vector; create parents first or use
    /// [`crate::helpers::TreeBuilder`] for dependency-aware fresh-tree creation.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.create_dir_all("/workspace")?;
    /// fs.create_dirs(&["/workspace/input", "/workspace/output"])?;
    /// # Ok(())
    /// # }
    /// ```
    fn create_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<()>;
    /// Create missing parents; an error can leave some directories created.
    ///
    /// Existing directories are accepted; an existing non-directory component
    /// is an error. This is not atomic and is not protected against concurrent
    /// changes to ancestor paths.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.create_dir_all("/workspace/results/2026")?;
    /// # Ok(())
    /// # }
    /// ```
    fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Remove one file or symlink, not the symlink target.
    ///
    /// Missing paths are errors. To remove a directory use
    /// [`remove_dir`](Self::remove_dir) or [`remove_dir_all`](Self::remove_dir_all).
    ///
    /// ```no_run
    /// use vnfs::{Client, ErrorKind};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// match fs.remove_file("/temporary-file") {
    ///     Ok(()) => (),
    ///     Err(error) if error.kind() == ErrorKind::NotFound => (),
    ///     Err(error) => return Err(error),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn remove_file(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Remove one empty directory; not a recursive operation.
    ///
    /// Nonempty directories fail. Use [`remove_dir_contents`](Self::remove_dir_contents)
    /// to empty a directory while retaining it, or [`remove_dir_all`](Self::remove_dir_all)
    /// to remove its tree. Only operate on paths whose removal you intend.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.remove_dir("/empty-temporary-directory")?;
    /// # Ok(())
    /// # }
    /// ```
    fn remove_dir(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Recursively remove a tree without following directory symlinks.
    /// Path-based removal is not a security sandbox or an atomic transaction.
    ///
    /// A failure can leave a partly removed tree. Use trusted ancestor paths;
    /// concurrent namespace substitutions are not a confinement guarantee.
    /// This is destructive: do not use it to prepare an arbitrary existing
    /// directory for an example or benchmark.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// // Remove only the tree that this application intentionally owns.
    /// fs.remove_dir_all("/owned-temporary-tree")?;
    /// # Ok(())
    /// # }
    /// ```
    fn remove_dir_all(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Recursively empty a directory while keeping its root.
    ///
    /// The root must be a directory, not a symlink. Child symlinks are removed
    /// without following their targets. Errors can leave partially removed
    /// contents; retaining the root does not make the operation transactional.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.remove_dir_contents("/owned-scratch-directory")?;
    /// // The scratch directory remains available for subsequent work.
    /// # Ok(())
    /// # }
    /// ```
    fn remove_dir_contents(&self, path: impl AsRef<Path>) -> Result<()>;
    /// Rename within supported namespaces; cross-filesystem moves can fail.
    ///
    /// Both paths use this client's namespace. Replacement semantics follow
    /// the backend/filesystem; this is not a cross-file vector transaction or
    /// a guarantee of durable directory updates. No copy fallback is implied.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.rename("/old-name", "/new-name")?;
    /// # Ok(())
    /// # }
    /// ```
    fn rename(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()>;
    /// Copy a file's contents; this is not recursive tree copying.
    ///
    /// Destination creation/replacement and failures follow backend semantics.
    /// A failure can leave a partial destination. Do not assume metadata,
    /// sparse layout, or durability is preserved. Server-side copy is used
    /// only where supported; callers need not build protocol COPY requests.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.copy("/source", "/destination")?;
    /// # Ok(())
    /// # }
    /// ```
    fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()>;
    /// Collect directory listings under one aggregate entry/path-byte policy.
    ///
    /// There is one listing per input directory, in input order; entry order is
    /// backend-defined. Entries include metadata, avoiding a separate scalar
    /// stat per child. The default policy comes from this client's limits.
    /// Use [`read_dirs_with_options`](Self::read_dirs_with_options) to select
    /// attributes, or a visitor instead of collecting large listings.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// for listing in fs.read_dirs(&["/input", "/output"])? {
    ///     for entry in listing.entries {
    ///         println!("{}: {} bytes", entry.path().display(), entry.metadata().len());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>>;
    /// Select metadata fields and override aggregate listing bounds.
    ///
    /// Limits apply across the returned listings, not independently per
    /// directory. Request only the attributes the application needs. Optional
    /// metadata may still be unavailable; a collection error does not return
    /// a successful partial listing. Explicit options override client defaults.
    ///
    /// ```no_run
    /// use vnfs::{Client, MetadataFields, ReadDirOptions};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let listings = fs.read_dirs_with_options(
    ///     &["/input", "/output"], MetadataFields::MODE | MetadataFields::SIZE,
    ///     ReadDirOptions::new().max_entries(10_000).max_path_bytes(1024 * 1024),
    /// )?;
    /// for listing in listings {
    ///     println!("{}: {} entries", listing.path.display(), listing.entries.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dirs_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        fields: crate::MetadataFields,
        options: crate::ReadDirOptions,
    ) -> Result<Vec<DirectoryListing>>;
    /// No-follow metadata with explicit fields; absent values remain None.
    ///
    /// This avoids requesting every optional attribute. Type/size can be
    /// requested internally even when not selected; missing optional attributes
    /// must still be handled via their `Option` accessors.
    ///
    /// ```no_run
    /// use vnfs::{Client, MetadataFields};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let metadata = fs.symlink_metadata_with_fields(
    ///     "/file-1", MetadataFields::MODE | MetadataFields::BLOCKS,
    /// )?;
    /// if let Some(blocks) = metadata.blocks() {
    ///     println!("allocated blocks: {blocks}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn symlink_metadata_with_fields(
        &self,
        path: impl AsRef<Path>,
        fields: crate::MetadataFields,
    ) -> Result<Metadata>;
    /// Strict no-follow metadata results in input order.
    ///
    /// This batches independent path queries. Like scalar symlink metadata,
    /// it does not follow the final symlink and does not provide a snapshot
    /// across files or protection against ancestor namespace changes.
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let paths = [Path::new("/file-1"), Path::new("/file-2")];
    /// let metadata = fs.symlink_metadatav(&paths)?;
    /// for (path, metadata) in paths.iter().zip(metadata) {
    ///     println!("{}: {} bytes", path.display(), metadata.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn symlink_metadatav(&self, paths: &[&Path]) -> Result<Vec<Metadata>>;
    /// Strict file-copy batches; a failed call can have copied earlier files.
    ///
    /// Pairs are `(source, destination)` in this client's namespace. Contents
    /// are copied, not a recursive tree or a metadata-preserving snapshot.
    /// Avoid aliasing sources/destinations; batching does not make overlapping
    /// copies transactional or safe to replay after ambiguous failure.
    ///
    /// ```no_run
    /// use vnfs::Client;
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.copy_files(&[
    ///     ("/input/file-1", "/output/file-1"),
    ///     ("/input/file-2", "/output/file-2"),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    fn copy_files<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()>;
    /// Ordered removal with explicit retry/error policy; no rollback promise.
    ///
    /// `recursive` allows removing nonempty directories. Backend support for
    /// custom policies varies; unsupported settings can fail explicitly.
    /// `continue_on_error` attempts later removals but still reports an error;
    /// retry settings are bounded handling of eligible statuses, not permission
    /// to replay every failed mutation. A successful call is not a transaction.
    ///
    /// ```no_run
    /// use vnfs::{Client, RemoveOptions};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// fs.remove_paths_with_options(
    ///     &["/owned-temp-1", "/owned-temp-2"], true, RemoveOptions::new(),
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    fn remove_paths_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        recursive: bool,
        options: crate::RemoveOptions,
    ) -> Result<()>;
    /// Collect a bounded tree, without following symlinks; no snapshot promise.
    ///
    /// Returns directory listings, not one flattened entry vector. Budgets
    /// apply across the walk; depth zero is the starting directory. Explicit
    /// options override client defaults. Use a visitor for incremental delivery
    /// or [`walk_events_with_options`](Self::walk_events_with_options) for pruning.
    ///
    /// ```no_run
    /// use vnfs::{Client, MetadataFields, WalkOptions};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let listings = fs.walk_with_options(
    ///     "/project", MetadataFields::MODE | MetadataFields::SIZE,
    ///     WalkOptions::new().max_entries(10_000).max_path_bytes(1024 * 1024)
    ///         .max_depth(8),
    /// )?;
    /// println!("{} directory listings", listings.len());
    /// # Ok(())
    /// # }
    /// ```
    fn walk_with_options(
        &self,
        path: impl AsRef<Path>,
        fields: crate::MetadataFields,
        options: crate::WalkOptions,
    ) -> Result<Vec<DirectoryListing>>;
    /// Visit bounded directory pages outside the backend lock. `Break(())`
    /// stops the entire walk, not one subtree. Order is backend-defined.
    ///
    /// This avoids materializing the whole tree but trades multi-directory
    /// batching for incremental delivery. Limits still apply to aggregate
    /// traversal work; backends without paging may buffer one bounded listing.
    /// Callbacks can use this client, and callback errors stop traversal.
    /// To prune just a subtree, use [`walk_events_with_options`](Self::walk_events_with_options).
    ///
    /// ```no_run
    /// use vnfs::{Client, ControlFlow, TraversalCompletion, WalkOptions};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let mut visited = 0;
    /// let completion = fs.visit_walk_with_options("/project", WalkOptions::new(), |entry| {
    ///     println!("{}", entry.path().display());
    ///     visited += 1;
    ///     Ok(if visited == 100 { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
    /// })?;
    /// if completion == TraversalCompletion::Stopped {
    ///     println!("stopped early; the rest of the tree was not visited");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn visit_walk_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::WalkOptions,
        callback: impl FnMut(&crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion>;
    /// Visit one directory; `Break(())` returns Stopped, exhaustion returns
    /// Complete, and callback errors propagate. The callback may reenter.
    ///
    /// Only this directory is visited, not its descendants. Budgets count all
    /// delivered entries/path bytes; paging bounds incremental delivery but
    /// does not mean unlimited traversal. Entry order is backend-defined.
    ///
    /// ```no_run
    /// use vnfs::{Client, ControlFlow, ReadDirOptions};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let completion = fs.visit_dir_with_options(
    ///     "/input", ReadDirOptions::new().max_entries(10_000), |entry| {
    ///         println!("{}", entry.path().display());
    ///         Ok(ControlFlow::Continue(()))
    ///     },
    /// )?;
    /// # let _ = completion;
    /// # Ok(())
    /// # }
    /// ```
    fn visit_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::ReadDirOptions,
        callback: impl FnMut(crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion>;
    /// Stream from offset zero outside the backend lock. `false` stops after
    /// the delivered chunk; completion reports the next offset. Not a snapshot.
    ///
    /// The callback borrows each chunk only for its invocation. Consume it
    /// without collecting chunks to keep memory bounded. `chunk_size` is a
    /// maximum; negotiated protocol limits and short reads can produce smaller
    /// chunks. The internally opened file is closed on completion, stopping,
    /// callback error, or read failure. Stopping successfully does not mean EOF.
    /// This simple stream does not promise RTT-hiding parallelism; direct NFS
    /// applications can use `NfsBuilder::connect_read_pool` for pipelined reads.
    ///
    /// ```no_run
    /// use vnfs::{Client, ReadStreamOptions, StreamCompletion};
    /// # fn example(fs: &impl Client) -> vnfs::Result<()> {
    /// let mut processed = 0_u64;
    /// let completion = fs.read_stream_with_options(
    ///     "/large.bin", ReadStreamOptions::new().chunk_size(1024 * 1024),
    ///     |_offset, chunk| {
    ///         processed += chunk.len() as u64; // Process the borrowed bytes here.
    ///         Ok(processed < 8 * 1024 * 1024) // Stop after a bounded sample.
    ///     },
    /// )?;
    /// match completion {
    ///     StreamCompletion::Complete => println!("reached EOF after {processed} bytes"),
    ///     StreamCompletion::Stopped { next_offset } => println!("stopped at {next_offset}"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
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
        fn create_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<()> {
            <$client>::create_dirs($receiver(self), paths)
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
