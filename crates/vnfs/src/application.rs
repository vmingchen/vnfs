//! Protocol-independent application contracts. Backend implementer traits
//! remain in `backend`; generic application helpers need only these traits.

use crate::{DirectoryListing, Metadata, OpenRequest, ResourceLimits, Result, WriteResult};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Owned application file. Borrowed requests preserve the file's lifetime;
/// clients validate connection ownership before dispatching a vector.
pub trait FileHandle: Read + Write + Seek {
    #[doc(hidden)]
    /// Borrowed positional read request; constructing it performs no I/O.
    type ReadRequest<'a>
    where
        Self: 'a;
    #[doc(hidden)]
    /// Borrowed positional request into a caller-owned destination buffer.
    type ReadIntoRequest<'a>
    where
        Self: 'a;
    /// Diagnostic name captured at open; not current path or object identity.
    fn path(&self) -> &Path;
    /// Query the opened object, even if its original pathname was renamed.
    fn metadata(&self) -> Result<Metadata>;
    #[doc(hidden)]
    /// Prepare a non-cursor-changing read for this handle's owning client.
    fn read_request_at(&self, offset: u64, length: usize) -> Self::ReadRequest<'_>;
    #[doc(hidden)]
    /// Borrow both the handle and destination until the vector call completes.
    fn read_request_at_into<'a>(
        &'a self,
        offset: u64,
        buffer: &'a mut [u8],
    ) -> Self::ReadIntoRequest<'a>;
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

/// Backend execution primitives for direct and routed application clients.
/// Construction, builders and protocol-specific diagnostics stay concrete.
/// Vectors are strict, ordered results on success, not transactions: errors
/// can follow partially completed requests and never imply rollback.
/// This uses borrowed request GATs, so it is for static generic dispatch, not
/// `dyn Fs`. It adds no boxing, data copies or serial-loop fallbacks.
///
/// # Choosing an operation
///
/// | Task | Start with |
/// | --- | --- |
/// | Complete small files | [`readv`](Self::readv), [`write_files`](FsExt::write_files) |
/// | Repeated/range I/O on owned handles | [`openv`](Self::openv), [`readv`](Self::readv), [`write_allv`](Self::write_allv) |
/// | A large file without collecting it | [`read_stream_with_options`](Self::read_stream_with_options) |
/// | Metadata for many directories | [`read_dirs_with_options`](Self::read_dirs_with_options) |
/// | Incremental traversal or pruning | [`visit_walk_with_options`](Self::visit_walk_with_options), [`walk_events_with_options`](FsExt::walk_events_with_options) |
///
/// Generic application code needs a `Fs` bound. Import [`FsExt`] for
/// convenience operations such as `read_files`, `write_files`, and scalar open.
/// Extension helpers are blanket-implemented; backend-specific execution stays
/// here so batching, paging, and recovery do not become scalar-loop fallbacks:
///
/// ```no_run
/// use vnfs::{Fs, FsExt, WriteOp};
/// fn load_parts(fs: &impl Fs) -> vnfs::Result<Vec<Vec<u8>>> {
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
pub trait Fs {
    /// Query metadata in input order with selected fields and final-symlink behavior.
    /// Backend execution must preserve vector batching. Ancestor symlinks use
    /// ordinary namespace resolution; this is not a snapshot or confinement API.
    ///
    /// ```no_run
    /// use vnfs::{Fs, MetadataOptions, MetadataFields, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let entries = fs.metadatav_with_options(&["/file-1", "/link"],
    ///     MetadataOptions::new().fields(MetadataFields::MODE | MetadataFields::SIZE)
    ///         .follow_symlinks(false))?;
    /// # let _ = entries;
    /// # Ok(())
    /// # }
    /// ```
    fn metadatav_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::MetadataOptions,
    ) -> Result<Vec<Metadata>>;

    /// Owned handle; vectors must contain handles belonging to this client.
    type File: FileHandle + 'static;

    /// Open with an explicit access/create/truncate request; effects are eager.
    ///
    /// Creation/truncation occurs at open, not on the first write. `CREATE_NEW`
    /// rejects an existing path. This does not create missing parents.
    /// This is a backend primitive: independently opened handles must remain
    /// usable when another handle for the same object is closed. Do not derive
    /// it from a one-element vector unless the backend preserves that contract.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FileHandle, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let file = fs.open_with(OpenRequest::new(
    ///     "/new-file", OpenFlags::WRITE | OpenFlags::CREATE_NEW,
    /// ).mode(0o600))?;
    /// file.close()?;
    /// # Ok(())
    /// # }
    /// ```
    fn open_with(&self, request: OpenRequest) -> Result<Self::File>;

    /// Collection/batch defaults, not a process memory cap or file-reader cap.
    ///
    /// This only inspects policy; it performs no filesystem I/O. Configure a
    /// concrete client's builder or `with_limits` to change its defaults.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) {
    /// let limits = fs.limits();
    /// println!("owned-read budget: {} bytes", limits.max_read_bytes);
    /// println!("walk depth: {}", limits.max_walk_depth);
    /// # }
    /// ```
    fn limits(&self) -> ResourceLimits;
    /// Strict ordered OPEN results. Failure releases returned handles but cannot
    /// undo files created/truncated earlier; `index()` is not a progress count.
    ///
    /// Different requests can use different flags/modes. All resulting handles
    /// belong to this client. Prefer this to a scalar loop when opening a cohort.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// Consume whole-file, allocating-range, and caller-buffer operations.
    ///
    /// Construction does no I/O. Results retain input order and never borrow
    /// caller storage. Whole files complete or fail; ranges may return short
    /// progress. Only `buffer[..result.read]` is valid after a buffered read.
    /// The default aggregate budget comes from `limits().max_read_bytes`.
    /// All range and buffer lengths must fit before dispatch. Buffer reads run
    /// first, then allocating ranges, then whole paths, in batched phases.
    /// Errors retain original indices; buffers may already contain partial
    /// progress on failure. No snapshot or atomicity is promised.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, ReadOp, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let file = fs.open("/large")?;
    /// let mut buffer = [0_u8; 4096];
    /// let result = fs.readv([
    ///     ReadOp::whole("/config"),
    ///     ReadOp::range(&file, 1024, 4096),
    ///     ReadOp::into(&file, 8192, &mut buffer),
    /// ]);
    /// let close = file.close();
    /// let results = result?;
    /// close?;
    /// println!("config: {:?}", results[0].data.as_deref().unwrap());
    /// println!("buffer: {:?}", &buffer[..results[2].read]);
    /// # Ok(())
    /// # }
    /// ```
    fn readv<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, Self::File>>,
    ) -> Result<Vec<crate::ReadResult>> {
        self.readv_with_options(ops, crate::ReadOptions::default())
    }
    /// Consume a read batch with an explicit aggregate logical-byte budget.
    /// Large files should usually be streamed instead of increasing the budget.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, ReadOp, ReadOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let results = fs.readv_with_options(
    ///     [ReadOp::whole("/config")],
    ///     ReadOptions::new().max_total_bytes(1024 * 1024),
    /// )?;
    /// println!("{} bytes", results[0].read);
    /// # Ok(())
    /// # }
    /// ```
    fn readv_with_options<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, Self::File>>,
        options: crate::ReadOptions,
    ) -> Result<Vec<crate::ReadResult>>;
    /// Possibly short writes in input order on success. Failure can follow
    /// partial mutations; neither a rollback nor an automatic retry is promised.
    ///
    /// Requests borrow payloads and preserve each handle's cursor. Inspect
    /// `written` rather than assuming the full payload was accepted. Use
    /// [`write_allv`](Self::write_allv) for complete writes across the cohort.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.writev(&[WriteOp::at(&file, 0, b"payload")]);
    /// let close = file.close();
    /// let results = result?;
    /// close?;
    /// println!("accepted {} of 7 bytes", results[0].written);
    /// # Ok(())
    /// # }
    /// ```
    fn writev<'a>(&self, requests: &[crate::WriteOp<'a, Self::File>]) -> Result<Vec<WriteResult>>;
    /// Finish short positional writes after whole-batch local preflight.
    /// Backend failures can still follow mutations; aliasing paths are the
    /// caller's responsibility and do not imply transactional ordering.
    ///
    /// On success each request's complete payload has been written. Successful
    /// short writes are completed at their remaining offsets; this does not
    /// authorize replay after an ambiguous failure or guarantee durability.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let files = fs.openv(&[
    ///     OpenRequest::new("/file-1", OpenFlags::WRITE),
    ///     OpenRequest::new("/file-2", OpenFlags::WRITE),
    /// ])?;
    /// let result = fs.write_allv(&[
    ///     WriteOp::at(&files[0], 0, b"hello"),
    ///     WriteOp::at(&files[1], 4096, b"world"),
    /// ]);
    /// let close = fs.closev(files);
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    fn write_allv<'a>(
        &self,
        requests: &[crate::WriteOp<'a, Self::File>],
    ) -> Result<Vec<WriteResult>>;
    /// Retain handles for failed cleanup; completed groups can already be
    /// closed. A lost reply leaves remote state ambiguous, not safely open.
    ///
    /// Unlike [`closev`](FsExt::closev), this borrows the handles. After an error,
    /// `is_closed()` reports local cleanup ownership only; reconcile/close the
    /// retained handles rather than resuming ordinary I/O as if nothing happened.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// Strict vector directory creation. Parents must already exist; errors
    /// can follow completed mutations, and do not imply rollback.
    ///
    /// Submit independent siblings together. Do not rely on creating a parent
    /// and its child in the same vector; create parents first or use
    /// [`crate::helpers::TreeBuilder`] for dependency-aware fresh-tree creation.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, ErrorKind, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.rename("/old-name", "/new-name")?;
    /// # Ok(())
    /// # }
    /// ```
    fn rename(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()>;
    /// Select metadata fields and override aggregate listing bounds.
    ///
    /// Limits apply across the returned listings, not independently per
    /// directory. Request only the attributes the application needs. Optional
    /// metadata may still be unavailable; a collection error does not return
    /// a successful partial listing. Explicit options override client defaults.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, MetadataFields, ReadDirOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// Strict file-copy batches; a failed call can have copied earlier files.
    ///
    /// Pairs are `(source, destination)` in this client's namespace. Contents
    /// are copied, not a recursive tree or a metadata-preserving snapshot.
    /// Avoid aliasing sources/destinations; batching does not make overlapping
    /// copies transactional or safe to replay after ambiguous failure.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, RemoveOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// or [`walk_events_with_options`](FsExt::walk_events_with_options) for pruning.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, MetadataFields, WalkOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// To prune just a subtree, use [`walk_events_with_options`](FsExt::walk_events_with_options).
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, ControlFlow, TraversalCompletion, WalkOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, ControlFlow, ReadDirOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// use vnfs::{Fs, FsExt, ReadStreamOptions, StreamCompletion, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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

/// Convenience operations built from the [`Fs`] primitives.
///
/// Automatically implemented for every client. Import `FsExt` (or
/// [`crate::prelude`]) to use these helpers; generic code still only needs a
/// `Fs` bound. Helpers preserve batching, resource limits, and non-atomic
/// failure semantics. They do not add a backend implementer obligation.
pub trait FsExt: Fs {
    /// Query a path following its final symlink; unavailable fields remain None.
    ///
    /// To inspect the symlink itself use [`symlink_metadata`](FsExt::symlink_metadata).
    /// To identify an already opened object after rename, use [`FileHandle::metadata`].
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let metadata = fs.metadata("/file-1")?;
    /// println!("{} bytes; directory={}", metadata.len(), metadata.is_dir());
    /// # Ok(())
    /// # }
    /// ```
    fn metadata(&self, path: impl AsRef<Path>) -> Result<Metadata> {
        metadata_one(self, path, crate::MetadataOptions::new())
    }

    /// No-follow metadata with explicit fields; absent values remain None.
    ///
    /// This avoids requesting every optional attribute. Type/size can be
    /// requested internally even when not selected; missing optional attributes
    /// must still be handled via their `Option` accessors.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, MetadataFields, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    ) -> Result<Metadata> {
        metadata_one(
            self,
            path,
            crate::MetadataOptions::new()
                .fields(fields)
                .follow_symlinks(false),
        )
    }

    /// Strict no-follow metadata results in input order.
    ///
    /// This batches independent path queries. Like scalar symlink metadata,
    /// it does not follow the final symlink and does not provide a snapshot
    /// across files or protection against ancestor namespace changes.
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let paths = [Path::new("/file-1"), Path::new("/file-2")];
    /// let metadata = fs.symlink_metadatav(&paths)?;
    /// for (path, metadata) in paths.iter().zip(metadata) {
    ///     println!("{}: {} bytes", path.display(), metadata.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn symlink_metadatav(&self, paths: &[&Path]) -> Result<Vec<Metadata>> {
        self.metadatav_with_options(paths, crate::MetadataOptions::new().follow_symlinks(false))
    }

    /// Fetch standard metadata for a vector of paths, following final symlinks.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let metadata = fs.metadatav(&["/file-1", "/file-2"])?;
    /// # let _ = metadata;
    /// # Ok(())
    /// # }
    /// ```
    fn metadatav<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Metadata>> {
        self.metadatav_with_options(paths, crate::MetadataOptions::new())
    }

    /// Read complete files in input order using vectorized whole-file reads.
    ///
    /// The aggregate payload is bounded by `limits().max_read_bytes`.
    /// Use [`Self::read_files_with_options`] to override it, or stream large files.
    /// A failure returns an error, not a successful partial collection.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let contents = fs.read_files(&["/file-1", "/file-2"])?;
    /// for bytes in contents { println!("{} bytes", bytes.len()); }
    /// # Ok(())
    /// # }
    /// ```
    fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Vec<u8>>> {
        self.read_files_with_options(paths, crate::ReadOptions::default())
    }

    /// Whole-file convenience reads with an explicit aggregate payload budget.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, ReadOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let files = fs.read_files_with_options(&["/config"],
    ///     ReadOptions::new().max_total_bytes(1024 * 1024))?;
    /// # let _ = files;
    /// # Ok(())
    /// # }
    /// ```
    fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::ReadOptions,
    ) -> Result<Vec<Vec<u8>>> {
        let results = self.readv_with_options(
            paths.iter().map(|path| crate::ReadOp::whole(path.as_ref())),
            options,
        )?;
        if results.len() != paths.len() {
            return Err(crate::Error::transport(
                None,
                "readv returned an invalid result count",
            ));
        }
        results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                let data = result.data.ok_or_else(|| {
                    crate::Error::transport(Some(index), "whole-file readv omitted owned data")
                })?;
                if data.len() != result.read || result.offset != 0 || !result.eof {
                    return Err(crate::Error::transport(
                        Some(index),
                        "whole-file readv returned incomplete or invalid data",
                    ));
                }
                Ok(data)
            })
            .collect()
    }

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
    /// use vnfs::{Fs, FsExt, MetadataFields, WalkControl, WalkEventKind, WalkOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    /// Open read-only. Paths are relative to this client's configured namespace.
    ///
    /// Use [`open_with`](Fs::open_with) for write/create flags, or
    /// [`openv`](Fs::openv) to batch many opens. The returned handle implements
    /// `std::io::Read`/`Write`/`Seek`; use native methods to retain structured errors.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
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
    fn open(&self, path: impl AsRef<Path>) -> Result<Self::File> {
        self.open_with(OpenRequest::new(path.as_ref(), crate::OpenFlags::READ))
    }

    /// Create or truncate a file and open for writing; does not create parents.
    ///
    /// Existing contents are discarded immediately. For exclusive creation,
    /// use [`open_with`](Fs::open_with) with `WRITE | CREATE_NEW` instead.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.write_allv(&[WriteOp::at(&file, 0, b"complete contents")]);
    /// let close = file.close();
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    fn create(&self, path: impl AsRef<Path>) -> Result<Self::File> {
        self.open_with(OpenRequest::new(
            path.as_ref(),
            crate::OpenFlags::WRITE | crate::OpenFlags::CREATE | crate::OpenFlags::TRUNCATE,
        ))
    }

    /// Consume all handles. Errors cannot return cleanup ownership; Drop is
    /// best-effort. Prefer `try_closev` when close errors require reconciliation.
    ///
    /// This releases all local handles even on failure; it does not promise
    /// every remote CLOSE succeeded. Closing alone is not a durability barrier.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let files = fs.openv(&[OpenRequest::new("/config", OpenFlags::READ)])?;
    /// fs.closev(files)?; // Observe a close error instead of discarding it in Drop.
    /// # Ok(())
    /// # }
    /// ```
    fn closev(&self, mut files: Vec<Self::File>) -> Result<()> {
        self.try_closev(&mut files)
    }

    /// Replace a file completely, creating/truncating eagerly; not atomic replace.
    ///
    /// The parent must exist. Success writes all bytes and closes the internal
    /// handle; failure may leave a created, truncated, or partially written file.
    /// For exclusive creation or durability control use an explicit open handle.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.write("/output", b"replacement contents")?;
    /// # Ok(())
    /// # }
    /// ```
    fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        // Scalar open preserves backend-specific final-symlink resolution.
        // Always attempt close, but retain the write error if both fail.
        let file = self.create(path)?;
        let result = self.write_allv(&[crate::WriteOp::at(&file, 0, data)]);
        let close_result = file.close();
        result?;
        close_result
    }

    /// Replace files in vector phases. Success completes payloads; errors may
    /// follow create/truncate/write effects and never authorize blind replay.
    ///
    /// Parents must exist. Payloads are completed via vector open/write/close
    /// phases, not a scalar write loop or necessarily a single RPC. No atomic
    /// replacement, cross-file transaction, or durability guarantee is provided.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.write_files(&[
    ///     ("/file-1", b"hello".as_slice()),
    ///     ("/file-2", b"world".as_slice()),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    fn write_files<P: AsRef<Path>, B: AsRef<[u8]>>(&self, entries: &[(P, B)]) -> Result<()> {
        let mut seen = std::collections::HashSet::with_capacity(entries.len());
        for (index, (path, _)) in entries.iter().enumerate() {
            if !seen.insert(path.as_ref()) {
                return Err(crate::Error::client(index, vfsi_core::ERR_INVAL)
                    .with_context("write_files", path.as_ref()));
            }
        }
        let requests: Vec<_> = entries
            .iter()
            .map(|(path, _)| {
                OpenRequest::new(
                    path.as_ref(),
                    crate::OpenFlags::WRITE | crate::OpenFlags::CREATE | crate::OpenFlags::TRUNCATE,
                )
            })
            .collect();
        let files = self.openv(&requests)?;
        if files.len() != entries.len() {
            return Err(crate::Error::transport(
                None,
                "openv returned an invalid result count",
            ));
        }
        let writes: Vec<_> = files
            .iter()
            .zip(entries)
            .map(|(file, (_, data))| crate::WriteOp::at(file, 0, data.as_ref()))
            .collect();
        let result = self.write_allv(&writes);
        drop(writes);
        let close_result = self.closev(files);
        result?;
        close_result
    }

    /// Query the final symlink itself instead of following it.
    ///
    /// This does not prevent following symlinks in ancestor components and is
    /// not a race-free namespace confinement primitive.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let metadata = fs.symlink_metadata("/link-or-file")?;
    /// println!("symlink={}", metadata.is_symlink());
    /// # Ok(())
    /// # }
    /// ```
    fn symlink_metadata(&self, path: impl AsRef<Path>) -> Result<Metadata> {
        self.symlink_metadata_with_fields(path, crate::MetadataFields::stat())
    }

    /// Create one directory; its parent must exist.
    ///
    /// An existing entry is an error, even if already a directory. Use
    /// [`create_dir_all`](Fs::create_dir_all) for missing parents/idempotent
    /// directory setup, or [`create_dirs`](Fs::create_dirs) for independent siblings.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.create_dir("/fresh-directory")?;
    /// # Ok(())
    /// # }
    /// ```
    fn create_dir(&self, path: impl AsRef<Path>) -> Result<()> {
        self.create_dirs(&[path])
    }

    /// Copy a file's contents; this is not recursive tree copying.
    ///
    /// Destination creation/replacement and failures follow backend semantics.
    /// A failure can leave a partial destination. Do not assume metadata,
    /// sparse layout, or durability is preserved. Server-side copy is used
    /// only where supported; callers need not build protocol COPY requests.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.copy("/source", "/destination")?;
    /// # Ok(())
    /// # }
    /// ```
    fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
        self.copy_files(&[(source, destination)])
    }

    /// Collect directory listings under one aggregate entry/path-byte policy.
    ///
    /// There is one listing per input directory, in input order; entry order is
    /// backend-defined. Entries include metadata, avoiding a separate scalar
    /// stat per child. The default policy comes from this client's limits.
    /// Use [`read_dirs_with_options`](Fs::read_dirs_with_options) to select
    /// attributes, or a visitor instead of collecting large listings.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// for listing in fs.read_dirs(&["/input", "/output"])? {
    ///     for entry in listing.entries {
    ///         println!("{}: {} bytes", entry.path().display(), entry.metadata().len());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>> {
        self.read_dirs_with_options(
            paths,
            crate::MetadataFields::stat(),
            self.limits().directory_options(),
        )
    }
    /// Read a complete UTF-8 file within this client's aggregate read budget.
    ///
    /// Invalid UTF-8 is an invalid-input error; use `read_files` for arbitrary bytes.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let text = fs.read_to_string("/config")?;
    /// println!("{text}");
    /// # Ok(())
    /// # }
    /// ```
    fn read_to_string(&self, path: impl AsRef<Path>) -> Result<String> {
        self.read_to_string_with_limit(path, self.limits().max_read_bytes)
    }

    /// Read a complete UTF-8 file with an explicit payload budget.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let text = fs.read_to_string_with_limit("/config", 4096)?;
    /// println!("{text}");
    /// # Ok(())
    /// # }
    /// ```
    fn read_to_string_with_limit(
        &self,
        path: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<String> {
        let path = path.as_ref();
        let mut files = self.read_files_with_options(
            &[path],
            crate::ReadOptions::new().max_total_bytes(max_bytes),
        )?;
        String::from_utf8(files.remove(0)).map_err(|_| {
            crate::Error::client(0, vfsi_core::ERR_INVAL).with_context("read_to_string", path)
        })
    }

    /// Collect one directory using the client's entry and path-byte limits.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// for entry in fs.read_dir("/input")? {
    ///     println!("{}", entry.path().display());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dir(&self, path: impl AsRef<Path>) -> Result<Vec<crate::DirEntry>> {
        self.read_dir_with_options(path, self.limits().directory_options())
    }

    /// Collect one directory with explicit aggregate entry/path-byte limits.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let entries = fs.read_dir_with_options("/input",
    ///     vnfs::ReadDirOptions::new().max_entries(100))?;
    /// println!("{} entries", entries.len());
    /// # Ok(())
    /// # }
    /// ```
    fn read_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::ReadDirOptions,
    ) -> Result<Vec<crate::DirEntry>> {
        let mut listings =
            self.read_dirs_with_options(&[path], crate::MetadataFields::stat(), options)?;
        if listings.len() != 1 {
            return Err(crate::Error::transport(
                None,
                "read_dirs returned an invalid result count",
            ));
        }
        Ok(listings.remove(0).entries)
    }

    /// Collect a no-follow tree using the client's entry, byte, and depth limits.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// for listing in fs.walk("/project")? {
    ///     println!("{}", listing.path.display());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn walk(&self, root: impl AsRef<Path>) -> Result<Vec<DirectoryListing>> {
        self.walk_with_options(
            root,
            crate::MetadataFields::stat(),
            self.limits().walk_options(),
        )
    }

    /// Visit a directory incrementally using the client's allocation limits.
    /// The callback runs outside the backend lock.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.visit_dir("/input", |entry| {
    ///     println!("{}", entry.path().display());
    ///     Ok(std::ops::ControlFlow::Continue(()))
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    fn visit_dir(
        &self,
        path: impl AsRef<Path>,
        callback: impl FnMut(crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion> {
        self.visit_dir_with_options(path, self.limits().directory_options(), callback)
    }

    /// Visit a no-follow tree incrementally using the client's traversal limits.
    /// The callback runs outside the backend lock.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.visit_walk("/project", |entry| {
    ///     println!("{}", entry.path().display());
    ///     Ok(std::ops::ControlFlow::Continue(()))
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    fn visit_walk(
        &self,
        root: impl AsRef<Path>,
        callback: impl FnMut(&crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion> {
        self.visit_walk_with_options(root, self.limits().walk_options(), callback)
    }

    /// Stream a file using the client's bounded chunk size instead of collecting it.
    /// Return `Ok(false)` to stop successfully; the callback runs outside the lock.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.read_stream("/large", |offset, bytes| {
    ///     println!("{} bytes at {offset}", bytes.len());
    ///     Ok(true)
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    fn read_stream(
        &self,
        path: impl AsRef<Path>,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::StreamCompletion> {
        self.read_stream_with_options(
            path,
            crate::ReadStreamOptions::new().chunk_size(self.limits().stream_chunk_bytes),
            callback,
        )
    }
}

fn metadata_one<C: Fs + ?Sized>(
    client: &C,
    path: impl AsRef<Path>,
    options: crate::MetadataOptions,
) -> Result<Metadata> {
    let mut results = client.metadatav_with_options(&[path], options)?;
    if results.len() != 1 {
        return Err(crate::Error::transport(
            None,
            "metadatav returned an invalid result count",
        ));
    }
    Ok(results.remove(0))
}

impl<C: Fs + ?Sized> FsExt for C {}

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
    file_methods!(vfsi_sync::FsFile<F>);
}

macro_rules! client_methods {
    ($client:ty, $receiver:path) => {
        client_methods!($client, $receiver, <$client>::readv_with_options);
    };
    ($client:ty, $receiver:path, $readv:expr) => {
        client_methods!($client, $receiver, $readv, $receiver);
    };
    ($client:ty, $receiver:path, $readv:expr, $read_receiver:path) => {
        client_methods!(
            $client,
            $receiver,
            $readv,
            $read_receiver,
            <$client>::writev,
            <$client>::write_allv
        );
    };
    ($client:ty, $receiver:path, $readv:expr, $read_receiver:path, $writev:expr, $write_allv:expr) => {
        client_methods!(
            $client,
            $receiver,
            $readv,
            $read_receiver,
            $writev,
            $write_allv,
            <$client>::metadatav_with_options
        );
    };
    ($client:ty, $receiver:path, $readv:expr, $read_receiver:path, $writev:expr, $write_allv:expr, $metadata:expr) => {
        fn metadatav_with_options<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: crate::MetadataOptions,
        ) -> Result<Vec<Metadata>> {
            ($metadata)($receiver(self), paths, options)
        }
        fn limits(&self) -> ResourceLimits {
            <$client>::limits($receiver(self))
        }
        fn open_with(&self, request: OpenRequest) -> Result<Self::File> {
            <$client>::open_with($receiver(self), request)
        }
        fn openv(&self, requests: &[OpenRequest]) -> Result<Vec<Self::File>> {
            <$client>::openv($receiver(self), requests)
        }
        fn readv_with_options<'a>(
            &self,
            ops: impl IntoIterator<Item = crate::ReadOp<'a, Self::File>>,
            options: crate::ReadOptions,
        ) -> Result<Vec<crate::ReadResult>> {
            ($readv)($read_receiver(self), ops, options)
        }
        fn writev<'a>(
            &self,
            requests: &[crate::WriteOp<'a, Self::File>],
        ) -> Result<Vec<WriteResult>> {
            ($writev)($receiver(self), requests)
        }
        fn write_allv<'a>(
            &self,
            requests: &[crate::WriteOp<'a, Self::File>],
        ) -> Result<Vec<WriteResult>> {
            ($write_allv)($receiver(self), requests)
        }
        fn try_closev(&self, files: &mut [Self::File]) -> Result<()> {
            <$client>::try_closev($receiver(self), files)
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
        fn read_dirs_with_options<P: AsRef<Path>>(
            &self,
            paths: &[P],
            fields: crate::MetadataFields,
            options: crate::ReadDirOptions,
        ) -> Result<Vec<DirectoryListing>> {
            <$client>::read_dirs_with_options($receiver(self), paths, fields, options)
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

impl<F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static> Fs
    for vfsi_sync::FsClient<F>
{
    type File = vfsi_sync::FsFile<F>;
    client_methods!(
        vfsi_sync::FsClient<F>,
        std::convert::identity,
        crate::read::read_backend::<F>,
        std::convert::identity,
        crate::write::write_backend::<F>,
        crate::write::write_backend_all::<F>,
        crate::metadata::metadata_backend::<F, _>
    );
}

#[cfg(feature = "nfs")]
impl FileHandle for crate::NfsFile {
    type ReadRequest<'a> = crate::NfsRead<'a>;
    type ReadIntoRequest<'a> = crate::NfsReadInto<'a>;
    file_methods!(crate::NfsFile);
}
#[cfg(feature = "nfs")]
impl Fs for crate::NfsClient {
    type File = crate::NfsFile;
    client_methods!(crate::NfsClient, std::convert::identity);
}

#[cfg(all(feature = "auto", target_os = "linux"))]
mod routed {
    use super::*;
    impl FileHandle for crate::AutoFile {
        type ReadRequest<'a> = crate::AutoRead<'a>;
        type ReadIntoRequest<'a> = crate::AutoReadInto<'a>;
        file_methods!(crate::AutoFile);
    }
    impl Fs for crate::AutoClient {
        type File = crate::AutoFile;
        client_methods!(crate::AutoClient, std::convert::identity);
    }
    impl Fs for crate::Auto {
        type File = crate::AutoFile;
        client_methods!(crate::AutoClient, std::ops::Deref::deref);
    }
    impl Fs for crate::Mounted {
        type File = crate::MountedFile;
        client_methods!(crate::Mounted, std::convert::identity);
    }
    impl FileHandle for crate::MountedFile {
        type ReadRequest<'a> = crate::MountedRead<'a>;
        type ReadIntoRequest<'a> = crate::MountedReadInto<'a>;
        file_methods!(crate::MountedFile);
    }
}

#[cfg(all(test, feature = "auto", target_os = "linux"))]
mod extension_tests {
    use super::*;
    use std::cell::Cell;

    // Deliberately has no inherent convenience methods or FsExt impl.
    struct Probe {
        mounted: crate::Mounted,
        calls: Cell<usize>,
        shape: Cell<u8>,
    }
    impl Probe {
        fn inner(&self) -> &crate::Mounted {
            &self.mounted
        }
        fn read<'a>(
            &self,
            ops: impl IntoIterator<Item = crate::ReadOp<'a, crate::MountedFile>>,
            options: crate::ReadOptions,
        ) -> Result<Vec<crate::ReadResult>> {
            self.calls.set(self.calls.get() + 1);
            let ops: Vec<_> = ops.into_iter().collect();
            assert!(ops.iter().all(|op| op.whole_file_path().is_some()));
            if self.shape.get() == 6 {
                return Err(crate::Error::transport(None, "lost reply"));
            }
            let mut results = self.mounted.readv_with_options(ops, options)?;
            match self.shape.get() {
                1 => {
                    results.pop();
                }
                2 => {
                    results[0].data = None;
                }
                3 => {
                    results[0].read += 1;
                }
                4 => {
                    results[0].eof = false;
                }
                5 => {
                    results[0].offset = 1;
                }
                7 => {
                    results.push(crate::ReadResult {
                        offset: 0,
                        read: 0,
                        eof: true,
                        data: Some(Vec::new()),
                    });
                }
                _ => {}
            }
            Ok(results)
        }
    }
    impl Fs for Probe {
        type File = crate::MountedFile;
        client_methods!(
            crate::Mounted,
            Probe::inner,
            Probe::read,
            std::convert::identity
        );
    }

    #[test]
    fn blanket_helpers_preserve_order_empty_files_limits_and_error_indices() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path())
                .unwrap()
                .with_limits(ResourceLimits {
                    max_read_bytes: 5,
                    ..Default::default()
                }),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.write_files(&[("/a", b"abc".as_slice()), ("/b", b"de"), ("/empty", b"")])
            .unwrap();
        assert_eq!(
            fs.read_files(&["/b", "/empty", "/a"]).unwrap(),
            [b"de".to_vec(), vec![], b"abc".to_vec()]
        );
        assert_eq!(
            fs.calls.get(),
            1,
            "one vector dispatch, not a scalar read loop"
        );
        assert!(fs.read_files::<&str>(&[]).unwrap().is_empty());
        assert_eq!(
            fs.read_files(&["/a", "/b", "/a"]).unwrap_err().kind(),
            crate::ErrorKind::FileTooLarge
        );
        assert_eq!(
            fs.read_files_with_options(
                &["/a", "/b", "/a"],
                crate::ReadOptions::new().max_total_bytes(8)
            )
            .unwrap()
            .len(),
            3
        );
        assert_eq!(
            fs.read_files(&["/a", "/missing"]).unwrap_err().index(),
            Some(1)
        );
        fs.write("/a", b"z").unwrap();
        assert_eq!(fs.read_to_string("/a").unwrap(), "z");
        fs.write("/a", &[0xff]).unwrap();
        assert_eq!(
            fs.read_to_string("/a").unwrap_err().kind(),
            crate::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.write_files(&[("/a", b"bad"), ("/a", b"bad")])
                .unwrap_err()
                .index(),
            Some(1)
        );
        assert_eq!(
            fs.read_files(&["/a"]).unwrap(),
            [vec![0xff]],
            "duplicate writes rejected before truncate"
        );
        fs.copy("/b", "/copy").unwrap();
        assert_eq!(fs.read_files(&["/copy"]).unwrap(), [b"de".to_vec()]);
        let mut file = fs.create("/created").unwrap();
        assert!(!file.is_closed());
        fs.try_closev(std::slice::from_mut(&mut file)).unwrap();
        assert!(file.is_closed());
        fs.closev(vec![file]).unwrap();
    }

    #[test]
    fn read_files_rejects_malformed_replies_and_never_replays_transport_errors() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.write("/a", b"abc").unwrap();
        for shape in 1..=7 {
            fs.shape.set(shape);
            let before = fs.calls.get();
            let error = fs.read_files(&["/a"]).unwrap_err();
            assert!(error.is_transport(), "shape {shape}: {error}");
            assert_eq!(fs.calls.get(), before + 1, "shape {shape} must not replay");
            if matches!(shape, 2..=5) {
                assert_eq!(error.index(), Some(0));
            }
        }
    }

    #[test]
    fn default_listing_and_stream_helpers_respect_client_limits_and_reentry() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path())
                .unwrap()
                .with_limits(ResourceLimits {
                    max_directory_entries: 1,
                    stream_chunk_bytes: 2,
                    ..Default::default()
                }),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.create_dir("/dir").unwrap();
        fs.write_files(&[("/dir/a", b"abc"), ("/dir/b", b"def")])
            .unwrap();
        assert!(fs.read_dir("/dir").is_err());
        assert!(fs.walk("/dir").is_err());
        let mut payload = Vec::new();
        fs.read_stream("/dir/a", |offset, bytes| {
            assert!(bytes.len() <= 2);
            assert_eq!(offset as usize, payload.len());
            // Reenter the same client from its callback.
            assert_eq!(fs.metadata("/dir/a")?.len(), 3);
            payload.extend_from_slice(bytes);
            Ok(true)
        })
        .unwrap();
        assert_eq!(payload, b"abc");
        fs.visit_dir("/dir", |_| Ok(std::ops::ControlFlow::Break(())))
            .unwrap();
    }
}
