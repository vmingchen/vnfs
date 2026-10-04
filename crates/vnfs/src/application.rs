//! Protocol-independent application contracts. Backend implementer traits
//! remain in `backend`; generic application helpers need only these traits.

use crate::{DirectoryListing, Metadata, OpenRequest, ResourceLimits, Result, WriteResult};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Owned application file. Borrowed requests preserve the file's lifetime;
/// clients validate connection ownership before dispatching a vector.
///
/// External implementations must keep request storage valid for its borrowed
/// lifetime, validate client ownership before I/O, and report positional progress
/// without changing the cursor. Request constructors do no I/O. Associated
/// request types are implementer contracts, not application-facing builders.
/// A backend must retain ownership of live descriptors through cleanup failures.
pub trait FileHandle: Read + Write + Seek {
    /// Borrowed positional read request; constructing it performs no I/O.
    type ReadRequest<'a>
    where
        Self: 'a;
    /// Borrowed positional request into a caller-owned destination buffer.
    type ReadIntoRequest<'a>
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
    /// best-effort on a later operation or cleanup drain. Prefer `try_close` when
    /// cleanup failures need reconciliation.
    fn close(self) -> Result<()>
    where
        Self: Sized;
}

/// Vectorized filesystem operations for direct and routed application clients.
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
/// | Complete small files | [`vread`](Self::vread), [`write_files`](FsExt::write_files) |
/// | Repeated/range I/O on owned handles | [`vopen`](Self::vopen), [`vread`](Self::vread), [`vwrite`](Self::vwrite) |
/// | Large files without collecting them | [`vstream`](Self::vstream) |
/// | Directory pages with entry metadata | [`vlistdirs`](Self::vlistdirs) |
/// | Recursive directory pages | [`vlistdirs`](Self::vlistdirs) with [`VisitOptions::recursive`](crate::VisitOptions::recursive) |
///
/// Generic application code needs an `Fs` bound. Import [`FsExt`] for
/// convenience operations such as `read_files`, `write_files`, and scalar open.
/// [`FsExt::read_dirs_with_options`] collects directory pages into vectors;
/// [`FsExt::read_stream_with_options`] adapts streaming to a single path.
/// Use the core vector operations above to submit multiple targets together.
/// Extension helpers compose vectors; backend-specific execution stays
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
/// Callback cancellation returns a completed/stopped prefix for vector visitors;
/// directory-page waves may leave several started roots stopped.
/// A failed mutation can have partial effects. An error's index identifies an
/// input when known, **not** a committed-prefix count. Preserve the structured
/// [`crate::Error`] rather than blindly replaying a failed write.
///
/// Allocation/traversal defaults come from [`limits`](Self::limits). Explicit
/// per-call options override them; neither is a process-wide memory cap.
/// Calls are synchronous. Callback methods invoke user code outside the backend
/// lock and propagate callback errors, but do not provide a filesystem snapshot.
/// Writes are not automatically durable: use [`FileHandle::sync_data`] or
/// [`FileHandle::sync_all`] on an open handle when required. Drop queues handle cleanup
/// best-effort; explicit close methods let applications observe cleanup errors.
pub trait Fs {
    /// Query metadata in input order with selected fields and final-symlink behavior.
    /// Backend execution must preserve vector batching. Ancestor symlinks use
    /// ordinary namespace resolution; this is not a snapshot or confinement API.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, MetadataOptions, MetadataFields, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let entries = fs.vgetattrs(&["/file-1", "/link"],
    ///     MetadataOptions::new().fields(MetadataFields::MODE | MetadataFields::SIZE)
    ///         .follow_symlinks(false))?;
    /// # let _ = entries;
    /// # Ok(())
    /// # }
    /// ```
    fn vgetattrs<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::MetadataOptions,
    ) -> Result<Vec<Metadata>>;

    /// Owned handle; vectors must contain handles belonging to this client.
    type File: FileHandle + 'static;

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
    /// Singleton vectors must preserve scalar symlink resolution and independently
    /// opened handle state: FsExt::open_with is built from this primitive.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let files = fs.vopen(&[
    ///     OpenRequest::new("/file-1", OpenFlags::READ),
    ///     OpenRequest::new("/file-2", OpenFlags::READ),
    /// ])?;
    /// // files[0] corresponds to file-1; files[1] to file-2.
    /// fs.closev(files)?;
    /// # Ok(())
    /// # }
    /// ```
    fn vopen(&self, requests: &[OpenRequest]) -> Result<Vec<Self::File>>;
    /// Consume a read batch with an explicit aggregate logical-byte budget.
    /// Large files should usually be streamed instead of increasing the budget.
    /// Ordering, partial-progress and buffer-validity rules are the same as
    /// [`FsExt::readv`]; explicit options override client defaults.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, ReadOp, ReadOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let results = fs.vread(
    ///     [ReadOp::whole("/config")],
    ///     ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(1024 * 1024)),
    /// )?;
    /// println!("{} bytes", results[0].read());
    /// # Ok(())
    /// # }
    /// ```
    fn vread<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, Self::File>>,
        options: crate::ReadOptions,
    ) -> Result<Vec<crate::ReadResult>>;
    /// Write positional ranges with explicit short-write policy.
    /// With default options, report accepted byte counts just like `writev`.
    /// With `write_all(true)`, finish short writes after whole-batch local preflight.
    /// Backend failures can still follow mutations; aliasing paths are the
    /// caller's responsibility and do not imply transactional ordering.
    ///
    /// With completion enabled, success means every request's payload was written.
    /// Short writes are completed at their remaining offsets; this does not
    /// authorize replay after an ambiguous failure or guarantee durability.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let files = fs.vopen(&[
    ///     OpenRequest::new("/file-1", OpenFlags::WRITE),
    ///     OpenRequest::new("/file-2", OpenFlags::WRITE),
    /// ])?;
    /// let result = fs.vwrite(&[
    ///     WriteOp::at(&files[0], 0, b"hello"),
    ///     WriteOp::at(&files[1], 4096, b"world"),
    /// ], vnfs::WriteOptions::new().write_all(true));
    /// let close = fs.closev(files);
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    fn vwrite<'a>(
        &self,
        requests: &[crate::WriteOp<'a, Self::File>],
        options: crate::WriteOptions,
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
    /// let mut files = fs.vopen(&[OpenRequest::new("/file-1", OpenFlags::READ)])?;
    /// if let Err(error) = fs.vclose(&mut files) {
    ///     let retained = files.iter().filter(|file| !file.is_closed()).count();
    ///     eprintln!("{retained} handles retain cleanup ownership: {error}");
    ///     return Err(error); // Drop performs best-effort cleanup, not reconciliation.
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn vclose(&self, files: &mut [Self::File]) -> Result<()>;
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
    /// fs.vmkdir(&["/workspace/input", "/workspace/output"])?;
    /// # Ok(())
    /// # }
    /// ```
    fn vmkdir<P: AsRef<Path>>(&self, paths: &[P]) -> Result<()>;
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
    /// fs.vcopy(&[
    ///     ("/input/file-1", "/output/file-1"),
    ///     ("/input/file-2", "/output/file-2"),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()>;
    /// Remove entries, recursive trees, or directory contents with explicit policy.
    ///
    /// Contents mode retains each root and uses native anchored removal without
    /// following final symlinks. Failures may follow partial mutations; no mode
    /// provides rollback. Options control bounded retries and error handling.
    ///
    /// ```no_run
    /// use vnfs::{Fs, RemoveMode, RemoveOptions};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.vremove(&["/scratch-1", "/scratch-2"],
    ///     RemoveMode::Contents, RemoveOptions::new())?;
    /// # Ok(())
    /// # }
    /// ```
    fn vremove<P: AsRef<Path>>(
        &self,
        paths: &[P],
        mode: crate::RemoveMode,
        options: crate::RemoveOptions,
    ) -> Result<()>;
    /// Rename independent pairs in input order. Failures may leave a completed prefix.
    /// Source and destination belong to this filesystem's namespace.
    ///
    /// ```no_run
    /// use vnfs::Fs;
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.vrename(&[("/old-1", "/new-1"), ("/old-2", "/new-2")])?;
    /// # Ok(())
    /// # }
    /// ```
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()>;

    /// Visit shallow directories (default) or recursive trees using bounded pages.
    /// Entry/path-byte limits are shared across roots; recursive visits also
    /// charge retained directory path bytes. Root index accompanies every owned directory page, including empty directories.
    /// Callbacks run outside backend locks and may reenter. Entries for one directory may span several pages. Break stops the
    /// whole vector. The returned prefix covers every root whose pages reached
    /// the callback; unfinished roots are Stopped. Pages may interleave between roots.
    ///
    /// Metadata is selected on directory pages, not through per-entry stat
    /// calls. Entry symlinks are never followed during recursive descent.
    /// Each page owns its entries; collection can transfer them without cloning.
    /// Traversal order is backend-defined; this does not provide a snapshot.
    ///
    /// ```no_run
    /// use vnfs::{Fs, VisitOptions, ControlFlow};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.vlistdirs(&["/tree-1", "/tree-2"],
    ///     VisitOptions::new().recursive(true).max_depth(8),
    ///     |index, page| {
    ///         println!("{index}: {} ({} entries)", page.path.display(), page.entries.len());
    ///         Ok(ControlFlow::Continue(()))
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    fn vlistdirs<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::VisitOptions,
        callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::TraversalCompletion>>;

    /// Stream files through bounded chunks without collecting whole contents.
    /// The callback receives the input index, byte offset, and borrowed chunk.
    /// False stops the whole vector; results contain its completed/stopped prefix.
    /// Backends may process streams sequentially; this is not a parallelism promise.
    ///
    /// ```no_run
    /// use vnfs::{Fs, ReadStreamOptions};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.vstream(&["/large-1", "/large-2"],
    ///     ReadStreamOptions::new().chunk_size(1024 * 1024), |index, offset, data| {
    ///         println!("{index}: {} bytes at {offset}", data.len());
    ///         Ok(true)
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    fn vstream<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::ReadStreamOptions,
        callback: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
    ) -> Result<Vec<crate::StreamCompletion>>;
}

/// Scalar operations and convenience workflows alongside the [`Fs`] vectors.
///
/// Blanket-implemented for every filesystem implementing Fs. Import `FsExt` (or
/// [`crate::prelude`]) to use these helpers; generic code needs only an
/// `Fs` bound. Helpers preserve batching, resource limits, and non-atomic
/// failure semantics. All helpers compose vectorized Fs primitives; native execution belongs in Fs.
/// Single-target helpers use conventional names such as `open`, `metadata`,
/// and `read_dir`. Prefer vector APIs for independent work on many files or
/// directories so the backend can batch requests.
///
/// # Boundary
///
/// | Contract | Responsibility | Examples |
/// | --- | --- | --- |
/// | [`Fs`] | Native vector execution, paging, and policy inspection | `vopen`, `vread`, `vlistdirs`, `limits` |
/// | `FsExt` | Default-policy vectors, scalar adapters, and composed workflows | `readv`, `open`, `read_files`, `create_dir_all` |
///
/// Implement only `Fs`; this extension is blanket implemented. A helper belongs
/// here only when it can compose Fs primitives without losing native batching,
/// bounded paging, or recovery semantics. Singular convenience is not a promise
/// of one RPC, and vector execution is not a promise of atomicity.
pub trait FsExt: Fs {
    /// Collect shallow directories or recursive trees, grouped by input root.
    ///
    /// `results[i]` contains the listings for `paths[i]`. Shallow mode
    /// returns exactly one listing per root; recursive mode includes the root
    /// and descendants. Entries and path-byte budgets are shared across roots;
    /// recursive collection also charges retained directory paths.
    /// Collection builds on the batched directory-page visitor.
    /// Explicit limits override client defaults. Errors discard collected results;
    /// this is not a snapshot. Symlink entries are not recursively followed.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, MetadataFields, VisitOptions};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let trees = fs.read_dirs_with_options(&["/input", "/output"],
    ///     VisitOptions::new().recursive(true).fields(MetadataFields::MODE)
    ///         .max_entries(10_000).max_path_bytes(1024 * 1024))?;
    /// for tree in trees {
    ///     for listing in tree {
    ///         println!("{}: {} entries", listing.path.display(), listing.entries.len());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dirs_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::VisitOptions,
    ) -> Result<Vec<Vec<DirectoryListing>>> {
        let mut trees: Vec<Vec<DirectoryListing>> = (0..paths.len()).map(|_| Vec::new()).collect();
        let mut positions: Vec<std::collections::HashMap<std::path::PathBuf, usize>> = (0..paths
            .len())
            .map(|_| std::collections::HashMap::new())
            .collect();
        self.vlistdirs(paths, options, |index, page| {
            let tree = trees
                .get_mut(index)
                .ok_or_else(|| crate::Error::transport(None, "invalid visitor root index"))?;
            let positions = &mut positions[index];
            let slot = *positions.entry(page.path.clone()).or_insert_with(|| {
                tree.push(DirectoryListing {
                    path: page.path.clone(),
                    entries: Vec::new(),
                });
                tree.len() - 1
            });
            tree[slot].entries.extend(page.entries);
            Ok(std::ops::ControlFlow::Continue(()))
        })?;
        Ok(trees)
    }

    /// Visit individual entries using the directory-page primitive.
    /// Empty directories produce pages but no entry callbacks.
    fn visit_entries_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::VisitOptions,
        mut callback: impl FnMut(usize, &crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::TraversalCompletion>> {
        self.vlistdirs(paths, options, |index, page| {
            for entry in &page.entries {
                if callback(index, entry)?.is_break() {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
    }

    /// Remove a cohort with default retry/error policy. Prefer this vector
    /// operation over looping over single-target removal helpers.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, RemoveMode};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.removev(&["/scratch-1", "/scratch-2"], RemoveMode::Tree)?;
    /// # Ok(())
    /// # }
    /// ```
    fn removev<P: AsRef<Path>>(&self, paths: &[P], mode: crate::RemoveMode) -> Result<()> {
        self.vremove(paths, mode, crate::RemoveOptions::default())
    }
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
    /// println!("config: {:?}", results[0].data().unwrap());
    /// println!("buffer: {:?}", &buffer[..results[2].read()]);
    /// # Ok(())
    /// # }
    /// ```
    fn readv<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::ReadOp<'a, Self::File>>,
    ) -> Result<Vec<crate::ReadResult>> {
        self.vread(ops, crate::ReadOptions::default())
    }
    /// Possibly short writes in input order on success. Failure can follow
    /// partial mutations; neither a rollback nor an automatic retry is promised.
    ///
    /// Requests borrow payloads and preserve each handle's cursor. Inspect
    /// `written` rather than assuming the full payload was accepted. Use
    /// [`vwrite`](Fs::vwrite) with `write_all(true)`
    /// for complete writes across the cohort.
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
    fn writev<'a>(&self, requests: &[crate::WriteOp<'a, Self::File>]) -> Result<Vec<WriteResult>> {
        self.vwrite(requests, crate::WriteOptions::default())
    }

    /// Single-target convenience. For multiple requests, prefer [`Fs::vopen`] with per-file flags and modes.
    ///
    /// Open with an explicit access/create/truncate request; effects are eager.
    ///
    /// Creation/truncation occurs at open, not on the first write. `CREATE_NEW`
    /// rejects an existing path. This does not create missing parents.
    /// Delegates to singleton `Fs::vopen`. Backends must preserve scalar final-
    /// symlink resolution and independently opened handles for singleton vectors.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let file = fs.open_with(OpenRequest::new(
    ///     "/new-file", OpenFlags::WRITE | OpenFlags::CREATE_NEW,
    /// ).mode(0o600))?;
    /// file.close()?;
    /// # Ok(())
    /// # }
    /// ```
    fn open_with(&self, request: OpenRequest) -> Result<Self::File> {
        let mut files = self.vopen(&[request])?;
        if files.len() != 1 {
            return Err(crate::Error::transport(
                None,
                "vopen returned an invalid result count",
            ));
        }
        Ok(files.remove(0))
    }

    /// Single-target convenience. For multiple directory creations, prefer [`Fs::vmkdir`]; plan missing parents before their children.
    ///
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
    fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
        let mut current = std::path::PathBuf::new();
        for component in path.as_ref().components() {
            match component {
                std::path::Component::RootDir => {
                    current.push(Path::new("/"));
                    continue;
                }
                std::path::Component::Normal(part) => current.push(part),
                std::path::Component::CurDir => continue,
                std::path::Component::ParentDir => {
                    current.push("..");
                    continue;
                }
                std::path::Component::Prefix(_) => {
                    return Err(crate::Error::client(0, vfsi_core::ERR_INVAL));
                }
            }
            match self.vmkdir(&[&current]) {
                Ok(()) => {}
                Err(error) if error.err_no() == vfsi_core::ERR_EXIST => {
                    if !self.metadata(&current)?.is_dir() {
                        return Err(crate::Error::client(0, vfsi_core::ERR_NOTDIR)
                            .with_context("create_dir_all", &current));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Single-target convenience. For multiple paths, prefer [`Fs::vremove`]. That vector API accepts both files and directories.
    ///
    /// Remove one file or symlink, not the symlink target.
    ///
    /// Missing paths are errors. To remove a directory use
    /// [`remove_dir`](FsExt::remove_dir) or [`remove_dir_all`](FsExt::remove_dir_all).
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
    fn remove_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if self.symlink_metadata(path)?.is_dir() {
            return Err(
                crate::Error::client(0, vfsi_core::ERR_ISDIR).with_context("remove_file", path)
            );
        }
        self.vremove(
            &[path],
            crate::RemoveMode::Entry,
            crate::RemoveOptions::new(),
        )
    }

    /// Single-target convenience. For multiple paths, prefer [`Fs::vremove`]. That vector API does not enforce directory-only inputs.
    ///
    /// Remove one empty directory; not a recursive operation.
    ///
    /// Nonempty directories fail. Use [`remove_dir_contents`](FsExt::remove_dir_contents)
    /// to empty a directory while retaining it, or [`remove_dir_all`](FsExt::remove_dir_all)
    /// to remove its tree. Only operate on paths whose removal you intend.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.remove_dir("/empty-temporary-directory")?;
    /// # Ok(())
    /// # }
    /// ```
    fn remove_dir(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        require_directory(self, path, "remove_dir")?;
        self.vremove(
            &[path],
            crate::RemoveMode::Entry,
            crate::RemoveOptions::new(),
        )
    }

    /// Single-target convenience. For multiple trees, prefer [`Fs::vremove`] with `RemoveMode::Tree`.
    ///
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
    fn remove_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        require_directory(self, path, "remove_dir_all")?;
        self.vremove(
            &[path],
            crate::RemoveMode::Tree,
            crate::RemoveOptions::new(),
        )
    }

    /// Single-target convenience. For multiple directory roots, prefer [`Fs::vremove`].
    ///
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
    fn remove_dir_contents(&self, path: impl AsRef<Path>) -> Result<()> {
        self.vremove(
            &[path],
            crate::RemoveMode::Contents,
            crate::RemoveOptions::new(),
        )
    }

    /// Single-target convenience. For multiple source/destination pairs, prefer [`Fs::vrename`].
    ///
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
    fn rename(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
        self.vrename(&[(source, destination)])
    }

    /// Single-target convenience. For multiple roots, prefer [`FsExt::read_dirs_with_options`] with one aggregate budget.
    ///
    /// Collect a bounded tree, without following symlinks; no snapshot promise.
    ///
    /// Returns directory listings, not one flattened entry vector. Budgets
    /// apply across the walk; depth zero is the starting directory. Explicit
    /// options override client defaults. Use a visitor for incremental delivery
    /// or [`walk_events_with_options`](FsExt::walk_events_with_options) for pruning.
    ///
    /// ```no_run
    /// use vnfs::{VisitOptions, Fs, FsExt, MetadataFields, WalkOptions, WriteOp};
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
    ) -> Result<Vec<DirectoryListing>> {
        let mut trees = self
            .read_dirs_with_options(&[path], crate::VisitOptions::from(options).fields(fields))?;
        if trees.len() != 1 {
            return Err(crate::Error::transport(
                None,
                "walks returned an invalid result count",
            ));
        }
        Ok(trees.remove(0))
    }

    /// Single-target convenience. For multiple roots, prefer [`Fs::vlistdirs`].
    ///
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
    /// use vnfs::{VisitOptions, Fs, FsExt, ControlFlow, TraversalCompletion, WalkOptions, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let mut visited = 0;
    /// let completion = fs.visit_walk_with_options("/project", VisitOptions::new(), |entry| {
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
        options: crate::VisitOptions,
        callback: impl FnMut(&crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion> {
        let mut callback = callback;
        single_completion(
            self.visit_entries_with_options(&[path], options.recursive(true), |_, entry| {
                callback(entry)
            })?,
            "visit_walks",
        )
    }

    /// Single-target convenience. For multiple directories, prefer [`Fs::vlistdirs`].
    ///
    /// Visit one directory; `Break(())` returns Stopped, exhaustion returns
    /// Complete, and callback errors propagate. The callback may reenter.
    ///
    /// Only this directory is visited, not its descendants. Budgets count all
    /// delivered entries/path bytes; paging bounds incremental delivery but
    /// does not mean unlimited traversal. Entry order is backend-defined.
    ///
    /// ```no_run
    /// use vnfs::{VisitOptions, Fs, FsExt, ControlFlow, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let completion = fs.visit_dir_with_options(
    ///     "/input", VisitOptions::new().max_entries(10_000), |entry| {
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
        options: crate::VisitOptions,
        callback: impl FnMut(&crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion> {
        let mut callback = callback;
        single_completion(
            self.visit_entries_with_options(&[path], options.recursive(false), |_, entry| {
                callback(entry)
            })?,
            "visit_dirs",
        )
    }

    /// Single-target convenience. For multiple files, prefer [`Fs::vstream`]. Backends may process streams sequentially.
    ///
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
    ) -> Result<crate::StreamCompletion> {
        let mut callback = callback;
        single_completion(
            self.vstream(&[path], options, |_, offset, data| callback(offset, data))?,
            "read_streams",
        )
    }

    /// Single-target convenience. For multiple paths, prefer [`Fs::vgetattrs`].
    ///
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
        metadata(self, path, crate::MetadataOptions::new())
    }

    /// Single-target convenience. For multiple paths, prefer [`Fs::vgetattrs`] with selected fields and follow_symlinks(false).
    ///
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
        metadata(
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
    fn symlink_metadatav<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Metadata>> {
        self.vgetattrs(paths, crate::MetadataOptions::new().follow_symlinks(false))
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
        self.vgetattrs(paths, crate::MetadataOptions::new())
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
    ///     ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(1024 * 1024)))?;
    /// # let _ = files;
    /// # Ok(())
    /// # }
    /// ```
    fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::ReadOptions,
    ) -> Result<Vec<Vec<u8>>> {
        let results = self.vread(
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

    /// Single-target convenience. For multiple roots without enter/leave events or subtree pruning, prefer [`Fs::vlistdirs`]. Keep this helper when those event semantics are required.
    ///
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
    /// use vnfs::{VisitOptions, Fs, FsExt, MetadataFields, WalkControl, WalkEventKind, WalkOptions, WriteOp};
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
                let mut trees = self.read_dirs_with_options(
                    &[path],
                    crate::VisitOptions::from(limits).fields(fields),
                )?;
                let mut listings = single_tree(&mut trees)?;
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
    /// Single-target convenience. For multiple files, prefer [`Fs::vopen`] to expose batching opportunities.
    ///
    /// Open read-only. Paths are relative to this client's configured namespace.
    ///
    /// Use [`open_with`](FsExt::open_with) for write/create flags, or
    /// [`vopen`](Fs::vopen) to batch many opens. The returned handle implements
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

    /// Single-target convenience. For multiple creations, prefer [`Fs::vopen`] with CREATE/TRUNCATE flags.
    ///
    /// Create or truncate a file and open for writing; does not create parents.
    ///
    /// Existing contents are discarded immediately. For exclusive creation,
    /// use [`open_with`](FsExt::open_with) with `WRITE | CREATE_NEW` instead.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, FileHandle, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.vwrite(&[WriteOp::at(&file, 0, b"complete contents")], vnfs::WriteOptions::new().write_all(true));
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
    /// best-effort. Prefer `vclose` when close errors require reconciliation.
    ///
    /// This releases all local handles even on failure; it does not promise
    /// every remote CLOSE succeeded. Closing alone is not a durability barrier.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, OpenFlags, OpenRequest, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// let files = fs.vopen(&[OpenRequest::new("/config", OpenFlags::READ)])?;
    /// fs.closev(files)?; // Observe a close error instead of discarding it in Drop.
    /// # Ok(())
    /// # }
    /// ```
    fn closev(&self, mut files: Vec<Self::File>) -> Result<()> {
        self.vclose(&mut files)
    }

    /// Single-target convenience. For multiple complete files, prefer [`FsExt::write_files`]; for opened handles, prefer [`Fs::vwrite`].
    ///
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
        let result = self.vwrite(
            &[crate::WriteOp::at(&file, 0, data)],
            crate::WriteOptions::new().write_all(true),
        );
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
        let files = self.vopen(&requests)?;
        if files.len() != entries.len() {
            return Err(crate::Error::transport(
                None,
                "vopen returned an invalid result count",
            ));
        }
        let writes: Vec<_> = files
            .iter()
            .zip(entries)
            .map(|(file, (_, data))| crate::WriteOp::at(file, 0, data.as_ref()))
            .collect();
        let result = self.vwrite(&writes, crate::WriteOptions::new().write_all(true));
        drop(writes);
        let close_result = self.closev(files);
        result?;
        close_result
    }

    /// Single-target convenience. For multiple paths, prefer [`FsExt::symlink_metadatav`] or [`Fs::vgetattrs`] with follow_symlinks(false).
    ///
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

    /// Single-target convenience. For multiple independent directories, prefer [`Fs::vmkdir`].
    ///
    /// Create one directory; its parent must exist.
    ///
    /// An existing entry is an error, even if already a directory. Use
    /// [`create_dir_all`](FsExt::create_dir_all) for missing parents/idempotent
    /// directory setup, or [`vmkdir`](Fs::vmkdir) for independent siblings.
    ///
    /// ```no_run
    /// use vnfs::{Fs, FsExt, WriteOp};
    /// # fn example(fs: &impl Fs) -> vnfs::Result<()> {
    /// fs.create_dir("/fresh-directory")?;
    /// # Ok(())
    /// # }
    /// ```
    fn create_dir(&self, path: impl AsRef<Path>) -> Result<()> {
        self.vmkdir(&[path])
    }

    /// Single-target convenience. For multiple file pairs, prefer [`Fs::vcopy`].
    ///
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
        self.vcopy(&[(source, destination)])
    }

    /// Collect directory listings under one aggregate entry/path-byte policy.
    ///
    /// There is one listing per input directory, in input order; entry order is
    /// backend-defined. Entries include metadata, avoiding a separate scalar
    /// stat per child. The default policy comes from this client's limits.
    /// Use [`read_dirs_with_options`](FsExt::read_dirs_with_options) to select
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
        let trees = self.read_dirs_with_options(
            paths,
            crate::VisitOptions::new().fields(crate::MetadataFields::stat()),
        )?;
        if trees.len() != paths.len() || trees.iter().any(|tree| tree.len() != 1) {
            return Err(crate::Error::transport(
                None,
                "read_dirs returned an invalid result count",
            ));
        }
        Ok(trees.into_iter().flatten().collect())
    }
    /// Single-target convenience. For multiple files, prefer [`FsExt::readv`] with whole-file operations and decode each result as UTF-8.
    ///
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
        let path = path.as_ref();
        let mut files = self.read_files(&[path])?;
        String::from_utf8(files.remove(0)).map_err(|_| {
            crate::Error::client(0, vfsi_core::ERR_INVAL).with_context("read_to_string", path)
        })
    }

    /// Single-target convenience. For multiple files, prefer [`Fs::vread`] with an aggregate byte budget, then decode UTF-8.
    ///
    /// Read a complete UTF-8 file with an explicit, nonzero payload budget.
    /// A zero-byte override returns an invalid-input error.
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
        let max_bytes = std::num::NonZeroUsize::new(max_bytes).ok_or_else(|| {
            crate::Error::client(0, vfsi_core::ERR_INVAL).with_context("read_to_string", path)
        })?;
        let mut files = self.read_files_with_options(
            &[path],
            crate::ReadOptions::new().max_total_bytes(Some(max_bytes)),
        )?;
        String::from_utf8(files.remove(0)).map_err(|_| {
            crate::Error::client(0, vfsi_core::ERR_INVAL).with_context("read_to_string", path)
        })
    }

    /// Single-target convenience. For multiple directories, prefer [`FsExt::read_dirs_with_options`] or [`FsExt::read_dirs`].
    ///
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

    /// Single-target convenience. For multiple directories, prefer [`FsExt::read_dirs_with_options`] with one aggregate budget.
    ///
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
        let mut trees = self.read_dirs_with_options(
            &[path],
            crate::VisitOptions::from(options).fields(crate::MetadataFields::stat()),
        )?;
        let mut listings = single_tree(&mut trees)?;
        if listings.len() != 1 {
            return Err(crate::Error::transport(
                None,
                "read_dirs returned an invalid result count",
            ));
        }
        Ok(listings.remove(0).entries)
    }

    /// Single-target convenience. For multiple roots, prefer [`FsExt::read_dirs_with_options`].
    ///
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

    /// Single-target convenience. For multiple directories, prefer [`Fs::vlistdirs`].
    ///
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
        callback: impl FnMut(&crate::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::TraversalCompletion> {
        self.visit_dir_with_options(path, crate::VisitOptions::new(), callback)
    }

    /// Single-target convenience. For multiple roots, prefer [`Fs::vlistdirs`].
    ///
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
        self.visit_walk_with_options(root, crate::VisitOptions::new(), callback)
    }

    /// Single-target convenience. For multiple files, prefer [`Fs::vstream`]. Backends may process streams sequentially.
    ///
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

impl<C: Fs + ?Sized> FsExt for C {}

fn single_completion<T>(mut values: Vec<T>, operation: &'static str) -> Result<T> {
    if values.len() != 1 {
        return Err(crate::Error::transport(
            None,
            format!("{operation} returned an invalid result count"),
        ));
    }
    Ok(values.remove(0))
}
fn require_directory<C: Fs + ?Sized>(fs: &C, path: &Path, operation: &'static str) -> Result<()> {
    if !fs.symlink_metadata(path)?.is_dir() {
        return Err(crate::Error::client(0, vfsi_core::ERR_NOTDIR).with_context(operation, path));
    }
    Ok(())
}
fn vector_index(error: crate::Error, index: usize) -> crate::Error {
    if error.index().is_some() {
        error.with_index(index)
    } else {
        error
    }
}
struct VectorBudget {
    entries: usize,
    bytes: usize,
}
impl VectorBudget {
    fn new(entries: usize, bytes: usize) -> Self {
        Self { entries, bytes }
    }
    fn charge(&mut self, path: &Path) -> Result<()> {
        if self.entries == 0 {
            return Err(
                crate::Error::client(0, libc::EFBIG as u32).with_context("vector traversal", path)
            );
        }
        self.charge_path(path)?;
        self.entries -= 1;
        Ok(())
    }
    fn charge_path(&mut self, path: &Path) -> Result<()> {
        let bytes = path.as_os_str().len();
        if bytes > self.bytes {
            return Err(
                crate::Error::client(0, libc::EFBIG as u32).with_context("vector traversal", path)
            );
        }
        self.bytes -= bytes;
        Ok(())
    }
}
fn metadata<C: Fs + ?Sized>(
    client: &C,
    path: impl AsRef<Path>,
    options: crate::MetadataOptions,
) -> Result<Metadata> {
    let mut results = client.vgetattrs(&[path], options)?;
    if results.len() != 1 {
        return Err(crate::Error::transport(
            None,
            "metadatav returned an invalid result count",
        ));
    }
    Ok(results.remove(0))
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

/// Private optimized execution hooks. Public helpers compose Fs instead.
pub(crate) trait NativeHooks: Fs {
    fn page_capacity(&self, paths: &[&Path]) -> Result<usize>;
    fn open_native(&self, request: OpenRequest) -> Result<Self::File>;
    fn stream_native(
        &self,
        path: impl AsRef<Path>,
        options: crate::ReadStreamOptions,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::StreamCompletion>;
}

/// Shared bounded page traversal. Fallback backends advertise a cohort of one;
/// NFS retains at most one wire page per directory in a cohort.
fn visit_directory_pages<P: AsRef<Path>>(
    roots: &[P],
    policy: crate::VisitOptions,
    limits: ResourceLimits,
    mut capacity: impl FnMut(&[&Path]) -> Result<usize>,
    mut validate_root: impl FnMut(&Path) -> Result<()>,
    mut fetch: impl FnMut(
        &[&Path],
        Vec<Option<vfsi_sync::DirPageCursor>>,
        usize,
        usize,
    ) -> Result<Vec<vfsi_sync::DirectoryPage>>,
    mut callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
) -> Result<Vec<crate::TraversalCompletion>> {
    let directory = policy.directory_options(limits);
    let walk = policy.walk_options(limits);
    let mut budget = VectorBudget::new(directory.entry_limit(), directory.path_byte_limit());
    let mut completed = Vec::new();
    let mut pending = std::collections::VecDeque::new();
    for (index, root) in roots.iter().enumerate() {
        pending.push_back((index, root.as_ref().to_path_buf(), 0usize, None));
    }
    while !pending.is_empty() {
        let owner = pending.front().unwrap().0;
        if policy.is_recursive() && pending.front().unwrap().2 == 0 {
            validate_root(&pending.front().unwrap().1)
                .map_err(|error| vector_index(error, owner))?;
        }
        // Recursive traversal completes one input root before starting another.
        let candidate_paths: Vec<_> = pending
            .iter()
            .take_while(|state| !policy.is_recursive() || state.0 == owner)
            .take(32)
            .map(|state| state.1.as_path())
            .collect();
        let capacity = capacity(&candidate_paths)?.clamp(1, 32);
        let cohort_size = if policy.is_recursive() {
            pending
                .iter()
                .take(capacity)
                .take_while(|state| state.0 == owner)
                .count()
        } else {
            pending.len().min(capacity)
        };
        let mut cohort: Vec<_> = (0..cohort_size)
            .map(|_| pending.pop_front().unwrap())
            .collect();
        let cursors = cohort.iter_mut().map(|state| state.3.take()).collect();
        let paths: Vec<_> = cohort
            .iter()
            .map(|(_, path, _, _)| path.as_path())
            .collect();
        let requested = budget.entries.saturating_add(1);
        let mut deferred_error = None;
        let pages = match fetch(&paths, cursors, 1, requested) {
            Ok(pages) => pages,
            Err(error)
                if !policy.is_recursive()
                    && !error.is_transport()
                    && error.index().is_some_and(|i| i > 0 && i < cohort.len()) =>
            {
                // A speculative later root must not hide an earlier callback's
                // cancellation. Re-fetch only the known successful read-only
                // prefix; this exceptional path never replays a mutation.
                let failed = error.index().unwrap();
                deferred_error = Some(vector_index(error, cohort[failed].0));
                let prefix = fetch(
                    &paths[..failed],
                    (0..failed).map(|_| None).collect(),
                    1,
                    requested,
                )
                .map_err(|error| error.map_index(|i| cohort.get(i).map_or(i, |state| state.0)))?;
                cohort.truncate(failed);
                prefix
            }
            Err(error) => return Err(error.map_index(|i| cohort.get(i).map_or(i, |state| state.0))),
        };
        if pages.len() != cohort.len() {
            return Err(crate::Error::transport(
                None,
                "invalid directory page result count",
            ));
        }
        let mut children = Vec::new();
        let mut wave: Vec<_> = cohort
            .into_iter()
            .map(|(index, path, depth, _)| (index, path, depth, true))
            .zip(pages)
            .collect();
        while !wave.is_empty() {
            let mut continuations = Vec::new();
            for ((index, path, depth, first), (mut page, next, seeds)) in wave {
                if first && policy.is_recursive() {
                    budget
                        .charge_path(&path)
                        .map_err(|error| vector_index(error, index))?;
                }
                if page.path != path || (page.entries.is_empty() && next.is_some()) {
                    return Err(crate::Error::transport(
                        Some(index),
                        "invalid directory page parent or progress",
                    ));
                }
                let mut seeds: std::collections::HashMap<_, _> = seeds.into_iter().collect();
                let mut page_error = None;
                let mut accepted = 0;
                for entry in &page.entries {
                    if let Err(error) = budget.charge(entry.path()) {
                        page_error = Some(vector_index(error, index));
                        break;
                    }
                    accepted += 1;
                    if policy.is_recursive() && entry.metadata().is_dir() {
                        if depth >= walk.depth_limit() {
                            if !walk.truncates_at_depth_limit() {
                                page_error = Some(
                                    crate::Error::client(index, libc::EFBIG as u32)
                                        .with_context("visit_dirs", entry.path()),
                                );
                                break;
                            }
                        } else {
                            children.push((
                                index,
                                entry.path().to_path_buf(),
                                depth + 1,
                                seeds.remove(entry.path()),
                            ));
                        }
                    }
                }
                if let Some(error) = &page_error {
                    if accepted == 0 {
                        return Err(error.clone());
                    }
                    page.entries.truncate(accepted);
                }
                if !policy.is_recursive() && completed.len() <= index {
                    completed.resize(index + 1, crate::TraversalCompletion::Stopped);
                }
                if callback(index, page)
                    .map_err(|error| vector_index(error, index))?
                    .is_break()
                {
                    if policy.is_recursive() {
                        completed.truncate(index);
                        completed.push(crate::TraversalCompletion::Stopped);
                    } else {
                        completed[index] = crate::TraversalCompletion::Stopped;
                    }
                    return Ok(completed);
                }
                if let Some(error) = page_error {
                    return Err(error);
                }
                match next {
                    Some(cursor) => continuations.push(((index, path, depth, false), Some(cursor))),
                    None if !policy.is_recursive() => {
                        completed[index] = crate::TraversalCompletion::Complete
                    }
                    None => {}
                }
            }
            if continuations.is_empty() {
                break;
            }
            let cursors = continuations
                .iter_mut()
                .map(|state| state.1.take())
                .collect();
            let paths: Vec<_> = continuations
                .iter()
                .map(|state| state.0.1.as_path())
                .collect();
            let requested = budget.entries.saturating_add(1);
            let pages = fetch(&paths, cursors, requested.min(128), requested).map_err(|error| {
                error.map_index(|i| continuations.get(i).map_or(i, |state| state.0.0))
            })?;
            if pages.len() != continuations.len() {
                return Err(crate::Error::transport(
                    None,
                    "invalid directory continuation count",
                ));
            }
            wave = continuations
                .into_iter()
                .map(|state| state.0)
                .zip(pages)
                .collect();
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        // Do not hold fallback snapshots while descending; all pages above
        // were consumed and their cursors released before adding the frontier.
        for child in children.into_iter().rev() {
            pending.push_front(child);
        }
        if policy.is_recursive() && pending.front().is_none_or(|state| state.0 != owner) {
            completed.push(crate::TraversalCompletion::Complete);
        }
    }
    Ok(completed)
}

fn single_tree(trees: &mut Vec<Vec<DirectoryListing>>) -> Result<Vec<DirectoryListing>> {
    if trees.len() != 1 {
        return Err(crate::Error::transport(
            None,
            "read_dirs returned an invalid root count",
        ));
    }
    Ok(trees.remove(0))
}

macro_rules! client_methods {
    ($client:ty, $receiver:path) => {
        client_methods!($client, $receiver, <$client>::vread);
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
            <$client>::write_partial_native,
            <$client>::write_complete
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
            <$client>::vgetattrs
        );
    };
    ($client:ty, $receiver:path, $readv:expr, $read_receiver:path, $writev:expr, $write_allv:expr, $metadata:expr) => {
        client_methods!(
            $client,
            $receiver,
            $readv,
            $read_receiver,
            $writev,
            $write_allv,
            $metadata,
            $receiver
        );
    };
    ($client:ty, $receiver:path, $readv:expr, $read_receiver:path, $writev:expr, $write_allv:expr, $metadata:expr, $write_receiver:path) => {
        client_methods!(
            $client,
            $receiver,
            $readv,
            $read_receiver,
            $writev,
            $write_allv,
            $metadata,
            $write_receiver,
            vrename,
            vmkdir,
            vcopy,
            vclose,
            vopen
        );
    };
    // Application clients and backend clients use different native method names.
    ($client:ty, $receiver:path, $readv:expr, $read_receiver:path, $writev:expr, $write_allv:expr, $metadata:expr, $write_receiver:path, $rename:ident, $mkdir:ident, $copy:ident, $close:ident, $open_batch:ident) => {
        fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::$rename($receiver(self), pairs)
        }
        fn vlistdirs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: crate::VisitOptions,
            callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
        ) -> Result<Vec<crate::TraversalCompletion>> {
            visit_directory_pages(
                paths,
                options,
                self.limits(),
                |paths| <$client as NativeHooks>::page_capacity($receiver(self), paths),
                |path| {
                    let metadata = self.vgetattrs(
                        &[path],
                        crate::MetadataOptions::new()
                            .fields(crate::MetadataFields::MODE)
                            .follow_symlinks(false),
                    )?;
                    if metadata.len() != 1 {
                        return Err(crate::Error::transport(
                            None,
                            "invalid directory root metadata count",
                        ));
                    }
                    if !metadata[0].is_dir() {
                        return Err(crate::Error::client(0, libc::ENOTDIR as u32)
                            .with_context("visit_dirs", path));
                    }
                    Ok(())
                },
                |paths, cursors, page_size, max_entries| {
                    <$client>::read_dir_pages_with_fields(
                        $receiver(self),
                        paths,
                        options.metadata_fields(),
                        cursors,
                        page_size,
                        max_entries,
                    )
                },
                callback,
            )
        }
        fn vstream<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: crate::ReadStreamOptions,
            mut callback: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
        ) -> Result<Vec<crate::StreamCompletion>> {
            let mut output = Vec::new();
            for (index, path) in paths.iter().enumerate() {
                let completion = <$client as NativeHooks>::stream_native(
                    $receiver(self),
                    path,
                    options,
                    |offset, data| callback(index, offset, data),
                )
                .map_err(|error| vector_index(error, index))?;
                output.push(completion);
                if matches!(completion, crate::StreamCompletion::Stopped { .. }) {
                    break;
                }
            }
            Ok(output)
        }
        fn vgetattrs<P: AsRef<Path>>(
            &self,
            paths: &[P],
            options: crate::MetadataOptions,
        ) -> Result<Vec<Metadata>> {
            ($metadata)($receiver(self), paths, options)
        }
        fn limits(&self) -> ResourceLimits {
            <$client>::limits($receiver(self))
        }

        fn vopen(&self, requests: &[OpenRequest]) -> Result<Vec<Self::File>> {
            if requests.len() == 1 {
                // Preserve native symlink resolution and independent-handle state.
                return <$client as NativeHooks>::open_native($receiver(self), requests[0].clone())
                    .map(|file| vec![file]);
            }
            <$client>::$open_batch($receiver(self), requests)
        }
        fn vread<'a>(
            &self,
            ops: impl IntoIterator<Item = crate::ReadOp<'a, Self::File>>,
            options: crate::ReadOptions,
        ) -> Result<Vec<crate::ReadResult>> {
            ($readv)($read_receiver(self), ops, options)
        }
        fn vwrite<'a>(
            &self,
            requests: &[crate::WriteOp<'a, Self::File>],
            options: crate::WriteOptions,
        ) -> Result<Vec<WriteResult>> {
            let result = if options.writes_all() {
                ($write_allv)($write_receiver(self), requests)
            } else {
                ($writev)($write_receiver(self), requests)
            };
            result.map_err(crate::write::public_write_error)
        }
        fn vclose(&self, files: &mut [Self::File]) -> Result<()> {
            <$client>::$close($receiver(self), files)
        }
        fn vmkdir<P: AsRef<Path>>(&self, paths: &[P]) -> Result<()> {
            <$client>::$mkdir($receiver(self), paths)
        }

        fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
            <$client>::$copy($receiver(self), pairs)
        }
        fn vremove<P: AsRef<Path>>(
            &self,
            paths: &[P],
            mode: crate::RemoveMode,
            options: crate::RemoveOptions,
        ) -> Result<()> {
            match mode {
                crate::RemoveMode::Entry | crate::RemoveMode::Tree => {
                    <$client>::remove_paths_with_options(
                        $receiver(self),
                        paths,
                        mode == crate::RemoveMode::Tree,
                        options,
                    )
                }
                crate::RemoveMode::Contents => {
                    let mut first_error = None;
                    for (index, path) in paths.iter().enumerate() {
                        if let Err(error) = <$client>::remove_dir_contents_with_options(
                            $receiver(self),
                            path,
                            options,
                        ) {
                            let error = vector_index(error, index);
                            if error.is_transport() || !options.continues_on_error() {
                                return Err(error);
                            }
                            first_error.get_or_insert(error);
                        }
                    }
                    first_error.map_or(Ok(()), Err)
                }
            }
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
        crate::metadata::metadata_backend::<F, _>,
        std::convert::identity,
        renamev,
        mkdirv,
        copyv,
        try_closev,
        openv
    );
}

impl<F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static>
    NativeHooks for vfsi_sync::FsClient<F>
{
    fn open_native(&self, request: OpenRequest) -> Result<Self::File> {
        self.open_with(request)
    }
    fn page_capacity(&self, _paths: &[&Path]) -> Result<usize> {
        self.directory_page_batch_size()
    }
    fn stream_native(
        &self,
        path: impl AsRef<Path>,
        options: crate::ReadStreamOptions,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::StreamCompletion> {
        self.read_stream_with_options(path, options, callback)
    }
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

    #[test]
    fn page_faults_do_not_replay_transport_failures_and_reject_malformed_shapes() {
        use crate::{ControlFlow, ResourceLimits, VisitOptions};
        for malformed in 0..4 {
            let mut calls = 0;
            let result = super::visit_directory_pages(
                &["/a"],
                VisitOptions::new(),
                ResourceLimits::default(),
                |_| Ok(32),
                |_| Ok(()),
                |_, _, _, _| {
                    calls += 1;
                    match malformed {
                        0 => Err(crate::Error::transport(None, "lost reply")),
                        1 => Ok(Vec::new()),
                        2 => Ok(vec![(
                            DirectoryListing {
                                path: "/wrong".into(),
                                entries: Vec::new(),
                            },
                            None,
                            Vec::new(),
                        )]),
                        _ => Ok(vec![(
                            DirectoryListing {
                                path: "/a".into(),
                                entries: Vec::new(),
                            },
                            Some(vfsi_sync::DirPageCursor::new(0usize)),
                            Vec::new(),
                        )]),
                    }
                },
                |_, _| panic!("invalid pages must not be delivered"),
            );
            assert!(result.unwrap_err().is_transport());
            assert_eq!(calls, 1);
        }
        let mut calls = 0;
        let result = super::visit_directory_pages(
            &["/a", "/missing"],
            VisitOptions::new(),
            ResourceLimits::default(),
            |_| Ok(32),
            |_| Ok(()),
            |paths, _, _, _| {
                calls += 1;
                if paths.len() == 2 {
                    return Err(crate::Error::client(1, libc::ENOENT as u32));
                }
                Ok(vec![(
                    DirectoryListing {
                        path: "/a".into(),
                        entries: Vec::new(),
                    },
                    None,
                    Vec::new(),
                )])
            },
            |index, page| {
                assert_eq!(index, 0);
                assert!(page.entries.is_empty());
                Ok(ControlFlow::Break(()))
            },
        )
        .unwrap();
        assert_eq!(result, [crate::TraversalCompletion::Stopped]);
        assert_eq!(calls, 2, "only a known semantic prefix is re-fetched");
    }

    #[test]
    fn directory_continuations_run_in_vector_waves_and_cancellation_marks_unfinished_roots() {
        use crate::{ControlFlow, ResourceLimits, VisitOptions};
        let temp = tempfile::tempdir().unwrap();
        let mounted = crate::Mounted::new(temp.path()).unwrap();
        mounted.write("/f", b"x").unwrap();
        let metadata = mounted.metadata("/f").unwrap();
        for stop in [false, true] {
            let mut widths = Vec::new();
            let result = super::visit_directory_pages(
                &["/a", "/b"],
                VisitOptions::new(),
                ResourceLimits::default(),
                |_| Ok(32),
                |_| Ok(()),
                |paths, cursors, _, _| {
                    widths.push(paths.len());
                    paths
                        .iter()
                        .zip(cursors)
                        .map(|(path, cursor)| {
                            let step = match cursor {
                                Some(cursor) => cursor.into_state::<usize>()?,
                                None => 0,
                            };
                            Ok((
                                DirectoryListing {
                                    path: path.to_path_buf(),
                                    entries: vec![crate::DirEntry::new(
                                        path.join(format!("f{step}")),
                                        metadata.clone(),
                                    )],
                                },
                                (step < 2).then(|| vfsi_sync::DirPageCursor::new(step + 1)),
                                Vec::new(),
                            ))
                        })
                        .collect()
                },
                |index, page| {
                    Ok(
                        if stop && index == 0 && page.entries[0].path().ends_with("f1") {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        },
                    )
                },
            )
            .unwrap();
            if stop {
                assert_eq!(widths, [2, 2]);
                assert_eq!(result, [crate::TraversalCompletion::Stopped; 2]);
            } else {
                assert_eq!(widths, [2, 2, 2]);
                assert_eq!(result, [crate::TraversalCompletion::Complete; 2]);
            }
        }
    }

    #[test]
    fn unified_collection_keeps_grouping_limits_and_generic_metadata_paths() {
        use crate::{MetadataFields, VisitOptions};
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.create_dir_all("/a/sub").unwrap();
        fs.create_dir("/b").unwrap();
        fs.write("/a/sub/f", b"payload").unwrap();
        fs.write("/b/f", b"x").unwrap();
        let shallow = VisitOptions::new().fields(MetadataFields::SIZE);
        let listed = fs.read_dirs_with_options(&["/a", "/b"], shallow).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|tree| tree.len() == 1));
        assert_eq!(listed[0][0].path, Path::new("/a"));
        assert_eq!(listed[1][0].entries[0].metadata().len(), 1);
        let trees = fs
            .read_dirs_with_options(&["/a", "/b"], shallow.recursive(true))
            .unwrap();
        assert_eq!(trees[0].len(), 2);
        assert_eq!(trees[1].len(), 1);
        assert!(
            trees[0]
                .iter()
                .flat_map(|listing| &listing.entries)
                .any(|entry| entry.path() == Path::new("/a/sub/f") && entry.metadata().len() == 7)
        );
        assert_eq!(
            fs.read_dirs_with_options(&["/a", "/a"], shallow.max_entries(1))
                .unwrap_err()
                .index(),
            Some(1)
        );
        assert_eq!(
            fs.read_dirs_with_options(&["/a", "/missing"], shallow)
                .unwrap_err()
                .index(),
            Some(1)
        );
        assert!(
            fs.read_dirs_with_options::<&str>(&[], shallow)
                .unwrap()
                .is_empty()
        );
        let truncated = fs
            .read_dirs_with_options(
                &["/a"],
                shallow
                    .recursive(true)
                    .max_depth(0)
                    .truncate_at_max_depth(true),
            )
            .unwrap();
        assert_eq!(truncated[0].len(), 1);
        std::os::unix::fs::symlink(root.path().join("a"), root.path().join("link")).unwrap();
        let paths = vec![String::from("/link"), String::from("/b")];
        let metadata = fs.symlink_metadatav(&paths).unwrap();
        assert!(metadata[0].is_symlink());
        assert!(metadata[1].is_dir());
        assert!(
            fs.symlink_metadatav(&[std::path::PathBuf::from("/link")])
                .unwrap()[0]
                .is_symlink()
        );
        assert!(fs.symlink_metadatav(&["/link"]).unwrap()[0].is_symlink());
        assert!(fs.symlink_metadatav::<String>(&[]).unwrap().is_empty());
    }

    #[test]
    fn unified_visiting_preserves_depth_defaults_fields_and_failures() {
        use crate::{ControlFlow, MetadataFields, TraversalCompletion, VisitOptions};
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path())
                .unwrap()
                .with_limits(ResourceLimits {
                    max_directory_entries: 1,
                    ..Default::default()
                }),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        std::fs::create_dir_all(root.path().join("tree/sub/deep")).unwrap();
        std::fs::write(root.path().join("tree/sub/file"), b"payload").unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();
        std::os::unix::fs::symlink(root.path().join("tree/sub"), root.path().join("other/link"))
            .unwrap();
        let mut seen = Vec::new();
        fs.visit_entries_with_options(&["/tree"], VisitOptions::new(), |_, entry| {
            seen.push(entry.path().to_path_buf());
            // Callback may reenter the same filesystem.
            assert!(fs.metadata(entry.path()).is_ok());
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
        assert_eq!(seen, [std::path::PathBuf::from("/tree/sub")]);
        assert!(
            fs.visit_entries_with_options(
                &["/tree"],
                VisitOptions::new().recursive(true),
                |_, _| Ok(ControlFlow::Continue(()))
            )
            .is_err()
        ); // inherits client budget
        let recursive = VisitOptions::new().recursive(true).max_entries(10);
        assert!(
            fs.visit_entries_with_options(&["/tree"], recursive.max_depth(0), |_, _| Ok(
                ControlFlow::Continue(())
            ))
            .is_err()
        );
        for (depth, count) in [(0, 1), (1, 3), (2, 3)] {
            let mut seen = Vec::new();
            fs.visit_entries_with_options(
                &["/tree"],
                recursive
                    .max_depth(depth)
                    .truncate_at_max_depth(true)
                    .fields(MetadataFields::SIZE),
                |_, entry| {
                    if entry.path().ends_with("file") {
                        assert_eq!(entry.metadata().len(), 7);
                    }
                    seen.push(entry.path().to_path_buf());
                    Ok(ControlFlow::Continue(()))
                },
            )
            .unwrap();
            assert_eq!(seen.len(), count);
        }
        // Symlink entries are delivered but never traversed.
        let mut seen = Vec::new();
        fs.visit_entries_with_options(&["/other"], recursive, |_, entry| {
            seen.push(entry.path().to_path_buf());
            Ok(ControlFlow::Continue(()))
        })
        .unwrap();
        assert_eq!(seen, [std::path::PathBuf::from("/other/link")]);
        assert_eq!(
            fs.visit_entries_with_options(&["/tree", "/missing"], recursive, |_, _| Ok(
                ControlFlow::Break(())
            ))
            .unwrap(),
            [TraversalCompletion::Stopped]
        );
        let error = fs
            .visit_entries_with_options(&["/other", "/tree"], recursive, |index, _| {
                if index == 1 {
                    Err(crate::Error::client(0, libc::EIO as u32))
                } else {
                    Ok(ControlFlow::Continue(()))
                }
            })
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(
            fs.visit_entries_with_options::<&str>(&[], recursive, |_, _| panic!("empty vector"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn removal_modes_preserve_roots_symlinks_and_vector_indices() {
        use crate::{RemoveMode, RemoveOptions};
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        // Probe has no inherent removal methods: these calls exercise Fs/FsExt.
        std::fs::create_dir_all(root.path().join("tree/nested")).unwrap();
        std::fs::write(root.path().join("tree/nested/file"), b"keep").unwrap();
        assert!(fs.removev(&["/tree"], RemoveMode::Entry).is_err());
        assert!(root.path().join("tree/nested/file").exists());
        fs.removev(&["/tree"], RemoveMode::Contents).unwrap();
        assert!(root.path().join("tree").is_dir());
        assert!(
            std::fs::read_dir(root.path().join("tree"))
                .unwrap()
                .next()
                .is_none()
        );

        std::fs::create_dir(root.path().join("outside")).unwrap();
        std::fs::write(root.path().join("outside/file"), b"safe").unwrap();
        symlink(root.path().join("outside"), root.path().join("link")).unwrap();
        let error = fs
            .removev(&["/tree", "/link"], RemoveMode::Contents)
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(root.path().join("tree").is_dir());
        assert!(root.path().join("outside/file").exists());
        fs.removev(&["/link"], RemoveMode::Tree).unwrap();
        assert!(root.path().join("outside/file").exists());
        fs.removev(&["/outside"], RemoveMode::Tree).unwrap();
        assert!(!root.path().join("outside").exists());
        fs.removev(&["/tree"], RemoveMode::Entry).unwrap();

        for mode in [RemoveMode::Entry, RemoveMode::Tree, RemoveMode::Contents] {
            fs.vremove::<&str>(&[], mode, RemoveOptions::new()).unwrap();
        }
        std::fs::create_dir(root.path().join("policy")).unwrap();
        std::fs::write(root.path().join("policy/file"), b"unchanged").unwrap();
        // The mounted generic remover rejects unsupported custom policy. Contents
        // must forward it rather than silently falling back to default options.
        assert!(
            fs.vremove(
                &["/policy"],
                RemoveMode::Contents,
                RemoveOptions::new().retries(0)
            )
            .is_err()
        );
        assert!(root.path().join("policy/file").exists());
    }

    // No inherent conveniences or explicit FsExt implementation.
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
            let mut results = self.mounted.vread(ops, options)?;
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
                crate::ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(8))
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
        fs.vclose(std::slice::from_mut(&mut file)).unwrap();
        assert!(file.is_closed());
        fs.closev(vec![file]).unwrap();
    }

    // Dispatch spy: model a backend accepting at most two bytes per request,
    // while delegating complete writes to the existing native implementation.
    // Native short-write waves and zero-progress checks have separate coverage
    // in vfsi-sync/tests/native_client.rs.
    struct WritePolicyProbe {
        mounted: crate::Mounted,
        partial_calls: Cell<usize>,
        complete_calls: Cell<usize>,
        lose_reply: Cell<bool>,
    }
    impl WritePolicyProbe {
        fn inner(&self) -> &crate::Mounted {
            &self.mounted
        }
        fn partial(
            &self,
            ops: &[crate::WriteOp<'_, crate::MountedFile>],
        ) -> Result<Vec<WriteResult>> {
            self.partial_calls.set(self.partial_calls.get() + 1);
            let short: Vec<_> = ops
                .iter()
                .map(|op| {
                    crate::WriteOp::at(op.file(), op.offset(), &op.data()[..op.data().len().min(2)])
                })
                .collect();
            self.mounted.writev(&short)
        }
        fn complete(
            &self,
            ops: &[crate::WriteOp<'_, crate::MountedFile>],
        ) -> Result<Vec<WriteResult>> {
            self.complete_calls.set(self.complete_calls.get() + 1);
            if self.lose_reply.get() {
                self.partial(ops)?; // The server mutated data before the reply was lost.
                return Err(crate::Error::transport(None, "injected lost write reply"));
            }
            self.mounted
                .vwrite(ops, crate::WriteOptions::new().write_all(true))
        }
    }
    impl Fs for WritePolicyProbe {
        type File = crate::MountedFile;
        client_methods!(
            crate::Mounted,
            WritePolicyProbe::inner,
            crate::Mounted::vread,
            WritePolicyProbe::inner,
            WritePolicyProbe::partial,
            WritePolicyProbe::complete,
            crate::Mounted::vgetattrs,
            std::convert::identity
        );
    }

    #[test]
    fn write_options_select_completion_without_replaying_ambiguous_errors() {
        let root = tempfile::tempdir().unwrap();
        let fs = WritePolicyProbe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            partial_calls: Cell::new(0),
            complete_calls: Cell::new(0),
            lose_reply: Cell::new(false),
        };
        let file = fs.create("/file").unwrap();
        let ops = [crate::WriteOp::at(&file, 0, b"abcdef")];
        assert!(!crate::WriteOptions::default().writes_all());
        assert_eq!(fs.writev(&ops).unwrap()[0].written, 2);
        for options in [
            crate::WriteOptions::new(),
            crate::WriteOptions::new().write_all(true).write_all(false),
        ] {
            assert_eq!(fs.vwrite(&ops, options).unwrap()[0].written, 2);
        }
        assert_eq!(fs.partial_calls.get(), 3);
        assert_eq!(fs.complete_calls.get(), 0);
        assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"ab".to_vec()]);
        let complete = crate::WriteOptions::new().write_all(true);
        assert_eq!(fs.vwrite(&ops, complete).unwrap()[0].written, 6);
        assert_eq!(fs.partial_calls.get(), 3);
        assert_eq!(fs.complete_calls.get(), 1);
        assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"abcdef".to_vec()]);

        fs.lose_reply.set(true);
        let error = fs
            .vwrite(&[crate::WriteOp::at(&file, 0, b"UVWXYZ")], complete)
            .unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.index(), None);
        assert_eq!(fs.partial_calls.get(), 4);
        assert_eq!(
            fs.complete_calls.get(),
            2,
            "never replay an ambiguous failure"
        );
        assert_eq!(fs.read_files(&["/file"]).unwrap(), [b"UVcdef".to_vec()]);
        file.close().unwrap();
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

    #[test]
    fn vector_walk_budgets_count_entries_not_listing_containers() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.vmkdir(&["/a", "/b"]).unwrap();
        fs.write_files(&[("/a/f", b"x"), ("/b/f", b"y")]).unwrap();
        let options = crate::WalkOptions::new().max_entries(2);
        let trees = fs
            .read_dirs_with_options(
                &["/a", "/b"],
                crate::VisitOptions::from(options).fields(crate::MetadataFields::MODE),
            )
            .unwrap();
        assert_eq!(
            trees
                .iter()
                .flat_map(|tree| tree.iter())
                .map(|listing| listing.entries.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(
            fs.read_dirs_with_options(
                &["/a", "/b"],
                crate::VisitOptions::from(options.max_entries(1))
                    .fields(crate::MetadataFields::MODE)
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
        fs.remove_file("/a/f").unwrap();
        fs.remove_file("/b/f").unwrap();
        assert!(
            fs.read_dirs_with_options(
                &["/a", "/b"],
                crate::VisitOptions::from(options.max_entries(0))
                    .fields(crate::MetadataFields::MODE)
            )
            .is_ok()
        );
        assert_eq!(
            fs.read_dirs_with_options(
                &["/a", "/b"],
                crate::VisitOptions::from(options.max_path_bytes(3))
                    .fields(crate::MetadataFields::MODE)
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
    }

    #[test]
    fn vector_walk_visitors_charge_root_paths_once_and_honor_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.vmkdir(&["/a", "/b"]).unwrap();
        let options = crate::WalkOptions::new().max_entries(0).max_path_bytes(4);
        assert_eq!(
            fs.visit_entries_with_options(&["/a", "/b"], options.into(), |_, _| panic!(
                "empty roots"
            ))
            .unwrap(),
            [crate::TraversalCompletion::Complete; 2]
        );
        assert_eq!(
            fs.visit_entries_with_options(
                &["/a", "/b"],
                options.max_path_bytes(2).into(),
                |_, _| panic!("empty roots")
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
        fs.write_files(&[("/a/f", b"x"), ("/b/f", b"y")]).unwrap();
        let options = options.max_entries(2).max_path_bytes(12);
        let mut seen = Vec::new();
        fs.visit_entries_with_options(&["/a", "/b"], options.into(), |index, entry| {
            seen.push((index, entry.path().to_path_buf()));
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                (0, std::path::PathBuf::from("/a/f")),
                (1, std::path::PathBuf::from("/b/f"))
            ]
        );
        assert_eq!(
            fs.visit_entries_with_options(
                &["/a", "/b"],
                options.max_path_bytes(11).into(),
                |_, _| Ok(std::ops::ControlFlow::Continue(()))
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
        assert_eq!(
            fs.visit_entries_with_options(
                &["/a", "/missing"],
                options.max_path_bytes(6).into(),
                |_, _| Ok(std::ops::ControlFlow::Break(()))
            )
            .unwrap(),
            [crate::TraversalCompletion::Stopped]
        );
    }

    #[test]
    fn singleton_walk_reports_the_root_index_not_an_entry_index() {
        let root = tempfile::tempdir().unwrap();
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fs.create_dir_all("/a/sub").unwrap();
        fs.write("/a/f", b"x").unwrap();
        let options = crate::WalkOptions::new().max_depth(0);
        // Concrete method syntax must use the same extension as generic code,
        // not leak the backend's per-entry index.
        assert_eq!(
            fs.mounted
                .walk_with_options("/a", crate::MetadataFields::MODE, options)
                .unwrap_err()
                .index(),
            Some(0)
        );
        assert_eq!(
            fs.read_dirs_with_options(
                &["/a"],
                crate::VisitOptions::from(options).fields(crate::MetadataFields::MODE)
            )
            .unwrap_err()
            .index(),
            Some(0)
        );
    }

    #[test]
    fn blanket_scalar_helpers_and_new_vectors_preserve_limits_stop_and_indices() {
        let root = tempfile::tempdir().unwrap();
        // Probe implements only Fs; no explicit FsExt implementation is possible.
        let fs = Probe {
            mounted: crate::Mounted::new(root.path()).unwrap(),
            calls: Cell::new(0),
            shape: Cell::new(0),
        };
        fn scalar<C: Fs>(fs: &C) {
            fs.create_dir_all("/one/nested").unwrap();
            fs.create_dir_all("/two").unwrap();
            fs.write("/one/a", b"abc").unwrap();
            fs.write("/two/b", b"def").unwrap();
            fs.rename("/one/a", "/one/renamed").unwrap();
        }
        scalar(&fs);
        fs.vrename(&[("/one/renamed", "/one/a"), ("/two/b", "/two/c")])
            .unwrap();
        let error = fs
            .vrename(&[("/one/a", "/one/moved"), ("/absent", "/two/moved")])
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(root.path().join("one/moved").exists());

        let roots = ["/one", "/two"];
        let trees = fs
            .read_dirs_with_options(
                &roots,
                crate::VisitOptions::from(crate::WalkOptions::new())
                    .fields(crate::MetadataFields::MODE),
            )
            .unwrap();
        assert_eq!(trees.len(), 2);
        assert!(trees.iter().all(|tree| !tree.is_empty()));
        for limit in 0..=4 {
            let options = crate::WalkOptions::new().max_entries(limit);
            assert_eq!(
                fs.walk_with_options("/two", crate::MetadataFields::MODE, options)
                    .is_ok(),
                fs.mounted
                    .walk_with_options("/two", crate::MetadataFields::MODE, options)
                    .is_ok()
            );
        }
        let mut seen = Vec::new();
        let completion = fs
            .visit_entries_with_options(&roots, crate::VisitOptions::new(), |index, entry| {
                seen.push(index);
                assert!(fs.metadata(entry.path()).is_ok()); // callbacks can reenter
                Ok(std::ops::ControlFlow::Break(()))
            })
            .unwrap();
        assert_eq!(completion, [crate::TraversalCompletion::Stopped]);
        assert_eq!(seen, [0]);

        let mut seen = Vec::new();
        let completion = fs
            .visit_entries_with_options(
                &roots,
                crate::VisitOptions::new().recursive(true),
                |index, _| {
                    seen.push(index);
                    Ok(std::ops::ControlFlow::Break(()))
                },
            )
            .unwrap();
        assert_eq!(completion, [crate::TraversalCompletion::Stopped]);
        assert_eq!(seen, [0]);

        let error = fs
            .visit_entries_with_options(
                &["/two", "/two"],
                crate::VisitOptions::new().max_entries(1),
                |_, _| Ok(std::ops::ControlFlow::Continue(())),
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        let error = fs
            .visit_entries_with_options(
                &["/two", "/two"],
                crate::VisitOptions::new().max_path_bytes("/two/c".len()),
                |_, _| Ok(std::ops::ControlFlow::Continue(())),
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(
            fs.read_dirs_with_options(
                &["/two", "/two"],
                crate::VisitOptions::from(crate::WalkOptions::new().max_entries(1))
                    .fields(crate::MetadataFields::MODE)
            )
            .is_err()
        );

        let mut seen = Vec::new();
        let completion = fs
            .vstream(
                &["/one/moved", "/absent"],
                crate::ReadStreamOptions::new().chunk_size(2),
                |index, offset, data| {
                    assert_eq!((index, offset), (0, 0));
                    assert_eq!(data, b"ab");
                    seen.push(index);
                    Ok(false)
                },
            )
            .unwrap();
        assert_eq!(
            completion,
            [crate::StreamCompletion::Stopped { next_offset: 2 }]
        );
        assert_eq!(seen, [0]);
        let error = fs
            .vstream(
                &["/one/moved", "/absent"],
                crate::ReadStreamOptions::new(),
                |_, _, _| Ok(true),
            )
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert!(fs.remove_file("/one").is_err());
        assert!(fs.remove_dir("/one/moved").is_err());
        fs.vremove(
            &roots,
            crate::RemoveMode::Contents,
            crate::RemoveOptions::new(),
        )
        .unwrap();
        assert!(fs.read_dir("/one").unwrap().is_empty());
        assert!(fs.read_dir("/two").unwrap().is_empty());
        fs.remove_dir("/two").unwrap();
        fs.remove_dir_all("/one").unwrap();
        assert!(fs.read_dir("/").unwrap().is_empty());
    }
}
