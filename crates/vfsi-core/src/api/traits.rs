//! Protocol-independent application contracts. Backend implementer traits
//! live in implementation crates; generic applications need only these traits.

use crate::api::{
    Attrs, CopyOption, DirectoryListing, OpenOp, ResourceLimits, Result, WriteResult,
};
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
    fn attrs(&self) -> Result<Attrs>;
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
    /// Change permission bits on the opened object, not by re-resolving its path.
    /// The default reports Unsupported; callers must not silently claim preservation.
    fn set_permissions(&self, _permissions: crate::api::Permissions) -> Result<()> {
        Err(crate::api::Error::client(0, crate::VF_ERR_UNSUPPORTED))
    }
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
/// `dyn Vfsi`. It adds no boxing, data copies or serial-loop fallbacks.
///
/// # Choosing an operation
///
/// | Task | Start with |
/// | --- | --- |
/// | Complete small files | [`vread`](Self::vread), [`write_files`](VfsiExt::write_files) |
/// | Repeated/range I/O on owned handles | [`vopen`](Self::vopen), [`vread`](Self::vread), [`vwrite`](Self::vwrite) |
/// | Large files without collecting them | [`vstream`](Self::vstream) |
/// | Directory pages with entry metadata | [`vlistdirs`](Self::vlistdirs) |
/// | Recursive directory pages | [`vlistdirs`](Self::vlistdirs) with [`ListDirOptions::recursive`](crate::api::ListDirOptions::recursive) |
///
/// Generic application code needs a `Vfsi` bound. Import [`VfsiExt`] for
/// convenience operations such as `read_files`, `write_files`, and scalar open.
/// [`VfsiExt::read_dirs_with_options`] collects directory pages into vectors;
/// [`VfsiExt::read_stream_with_options`] adapts streaming to a single path.
/// Use the core vector operations above to submit multiple targets together.
/// Extension helpers compose vectors; backend-specific execution stays
/// here so batching, paging, and recovery do not become scalar-loop fallbacks:
///
/// ```no_run
/// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
/// fn load_parts(fs: &impl Vfsi) -> vfsi_core::api::Result<Vec<Vec<u8>>> {
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
/// [`crate::api::Error`] rather than blindly replaying a failed write.
///
/// Allocation/traversal defaults come from [`limits`](Self::limits). Explicit
/// per-call options override them; neither is a process-wide memory cap.
/// Calls are synchronous. Callback methods invoke user code outside the backend
/// lock and propagate callback errors, but do not provide a filesystem snapshot.
/// Writes are not automatically durable: use [`FileHandle::sync_data`] or
/// [`FileHandle::sync_all`] on an open handle when required. Drop queues handle cleanup
/// best-effort; explicit close methods let applications observe cleanup errors.
pub trait Vfsi {
    /// Query filesystems for paths (following symlinks) and retained open handles.
    /// Results preserve input order. Unsupported fields are `None`.
    fn vstatfs<P: crate::MetadataOperand<Self::File>>(
        &self,
        targets: &[P],
    ) -> Result<Vec<crate::FilesystemStats>>;

    /// Query metadata in input order with selected fields and final-symlink behavior.
    /// Backend execution must preserve vector batching. Ancestor symlinks use
    /// ordinary namespace resolution; this is not a snapshot or confinement API.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, AttrsOptions, Attributes, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let entries = fs.vgetattrs(&["/file-1", "/link"],
    ///     AttrsOptions::new().fields(Attributes::MODE | Attributes::SIZE)
    ///         .follow_symlinks(false))?;
    /// # let _ = entries;
    /// # Ok(())
    /// # }
    /// ```
    fn vgetattrs<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::AttrsOptions,
    ) -> Result<Vec<Attrs>>;

    /// Update selected attributes for paths or open handles using native batching.
    /// Every handle is validated before dispatch, including ownership and closure.
    /// Unspecified fields are unchanged. `follow_symlinks` controls the final
    /// component of path targets; it does not change open-handle identity.
    /// Ancestor symlinks retain ordinary backend resolution.
    /// Failure can follow partial mutations, including within one request;
    /// an error index identifies an input, not a committed-prefix count.
    /// Empty vectors succeed without I/O. Do not replay ambiguous failures.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, MetadataUpdate, Permissions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vsetattrs(&[
    ///     ("/file-1", MetadataUpdate::new().permissions(Permissions::from_mode(0o640)).len(1024)),
    ///     ("/file-2", MetadataUpdate::new().len(0)),
    /// ], true)?;
    /// # Ok(())
    /// # }
    /// ```
    /// Handle targets and paths can share a batch:
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, MetadataTarget, MetadataUpdate};
    /// # fn example<F: Vfsi>(fs: &F, file: &F::File) -> vfsi_core::api::Result<()> {
    /// fs.vsetattrs(&[
    ///     (MetadataTarget::File(file), MetadataUpdate::new().len(1024)),
    ///     (MetadataTarget::Path(std::path::Path::new("/other")), MetadataUpdate::new().len(0)),
    /// ], true)?;
    /// # Ok(())
    /// # }
    /// ```
    fn vsetattrs<P: crate::api::MetadataOperand<Self::File>>(
        &self,
        updates: &[(P, crate::api::MetadataUpdate)],
        follow_symlinks: bool,
    ) -> Result<()>;

    /// Capabilities supported by this client. Routed clients report capabilities
    /// common to their routes; support does not imply authorization for a path.
    fn capabilities(&self) -> Result<crate::api::Capabilities>;

    /// Create symbolic links from `(target text, link path)` pairs.
    /// Targets are stored verbatim, including relative or dangling targets.
    /// Parents must exist. Errors may follow partial creation; no rollback.
    fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()>;

    /// Read symbolic link targets in input order without following final links.
    /// Returns their original path bytes, including non-UTF-8 names on Unix.
    /// An error identifies the input when known and discards collected results.
    fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<std::path::PathBuf>>;

    /// Create hard links from `(existing source, new link path)` pairs.
    /// Sources are not followed when the final component is a symlink.
    /// Cross-filesystem links can fail. Errors may follow partial creation.
    fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()>;

    /// Owned handle; vectors must contain handles belonging to this client.
    type File: FileHandle + 'static;

    /// Collection/batch defaults, not a process memory cap or file-reader cap.
    ///
    /// This only inspects policy; it performs no filesystem I/O. Configure a
    /// concrete client's builder or `with_limits` to change its defaults.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) {
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
    /// opened handle state: VfsiExt::open_with is built from this primitive.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, OpenFlags, OpenOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let files = fs.vopen(&[
    ///     OpenOp::new("/file-1", OpenFlags::READ),
    ///     OpenOp::new("/file-2", OpenFlags::READ),
    /// ])?;
    /// // files[0] corresponds to file-1; files[1] to file-2.
    /// fs.close_files(files)?;
    /// # Ok(())
    /// # }
    /// ```
    fn vopen(&self, requests: &[OpenOp]) -> Result<Vec<Self::File>>;
    /// Consume a read batch with an explicit aggregate logical-byte budget.
    /// Large files should usually be streamed instead of increasing the budget.
    /// Explicit options override client defaults.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, ReadOp, ReadOptions, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let results = fs.vread(
    ///     [ReadOp::whole("/config")],
    ///     ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(1024 * 1024)),
    /// )?;
    /// println!("{} bytes", results[0].read());
    /// # Ok(())
    /// # }
    /// ```
    ///
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
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, ReadOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let file = fs.open("/large")?;
    /// let mut buffer = [0_u8; 4096];
    /// let result = fs.vread([
    ///     ReadOp::whole("/config"),
    ///     ReadOp::range(&file, 1024, 4096),
    ///     ReadOp::into(&file, 8192, &mut buffer),
    /// ], Default::default());
    /// let close = file.close();
    /// let results = result?;
    /// close?;
    /// println!("config: {:?}", results[0].data().unwrap());
    /// println!("buffer: {:?}", &buffer[..results[2].read()]);
    /// # Ok(())
    /// # }
    /// ```
    fn vread<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::api::ReadOp<'a, Self::File>>,
        options: crate::api::ReadOptions,
    ) -> Result<Vec<crate::api::ReadResult>>;
    /// Write positional ranges with explicit short-write policy.
    /// With default options, report accepted byte counts without completing short writes.
    /// With `write_all(true)`, finish short writes after whole-batch local preflight.
    /// Backend failures can still follow mutations; aliasing paths are the
    /// caller's responsibility and do not imply transactional ordering.
    ///
    /// With completion enabled, success means every request's payload was written.
    /// Short writes are completed at their remaining offsets; this does not
    /// authorize replay after an ambiguous failure or guarantee durability.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, OpenFlags, OpenOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let files = fs.vopen(&[
    ///     OpenOp::new("/file-1", OpenFlags::WRITE),
    ///     OpenOp::new("/file-2", OpenFlags::WRITE),
    /// ])?;
    /// let result = fs.vwrite(&[
    ///     WriteOp::at(&files[0], 0, b"hello"),
    ///     WriteOp::at(&files[1], 4096, b"world"),
    /// ], vfsi_core::api::WriteOptions::new().write_all(true));
    /// let close = fs.close_files(files);
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    fn vwrite<'a>(
        &self,
        requests: &[crate::api::WriteOp<'a, Self::File>],
        options: crate::api::WriteOptions,
    ) -> Result<Vec<WriteResult>>;
    /// Retain handles for failed cleanup; completed groups can already be
    /// closed. A lost reply leaves remote state ambiguous, not safely open.
    ///
    /// Unlike [`close_files`](VfsiExt::close_files), this borrows the handles. After an error,
    /// `is_closed()` reports local cleanup ownership only; reconcile/close the
    /// retained handles rather than resuming ordinary I/O as if nothing happened.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, OpenFlags, OpenOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let mut files = fs.vopen(&[OpenOp::new("/file-1", OpenFlags::READ)])?;
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
    /// a dependency-aware tree builder for dependency-aware fresh-tree creation.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.create_dir_all("/workspace")?;
    /// fs.vmkdir(&[("/workspace/input", 0o750), ("/workspace/output", 0o700)])?;
    /// # Ok(())
    /// # }
    /// ```
    /// Each `(path, mode)` supplies Unix permission bits; backends apply the
    /// requested mode rather than relying on the process umask. Unsupported
    /// permission semantics are reported by the backend.
    fn vmkdir<P: AsRef<Path>>(&self, directories: &[(P, u32)]) -> Result<()>;
    /// Strict file-copy batches; a failed call can have copied earlier files.
    ///
    /// Pairs are `(source, destination)` in this client's namespace. Contents
    /// are copied, not a recursive tree or a metadata-preserving snapshot.
    /// `options` controls whether final source symlinks are followed; ancestor
    /// symlinks retain ordinary path-resolution behavior.
    /// Avoid aliasing sources/destinations; batching does not make overlapping
    /// copies transactional or safe to replay after ambiguous failure.
    ///
    /// ```no_run
    /// use vfsi_core::api::{CopyOption, Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vcopy(&[
    ///     ("/input/file-1", "/output/file-1"),
    ///     ("/input/file-2", "/output/file-2"),
    /// ], CopyOption::default())?;
    /// # Ok(())
    /// # }
    /// ```
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: CopyOption,
    ) -> Result<()>;
    /// Remove entries, recursive trees, or directory contents with explicit policy.
    ///
    /// Contents mode retains each root and uses native anchored removal without
    /// following final symlinks. Failures may follow partial mutations; no mode
    /// provides rollback. Options control bounded retries and error handling.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, RemoveMode, RemoveOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vremove(&["/scratch-1", "/scratch-2"],
    ///     RemoveMode::Contents, RemoveOptions::new())?;
    /// # Ok(())
    /// # }
    /// ```
    fn vremove<P: AsRef<Path>>(
        &self,
        paths: &[P],
        mode: crate::api::RemoveMode,
        options: crate::api::RemoveOptions,
    ) -> Result<()>;
    /// Rename independent pairs in input order using the requested atomic
    /// destination behavior. Each pair is atomic where the backend supports
    /// the option; the vector is not a transaction, so failures may leave a
    /// completed prefix. Source and destination are resolved in this filesystem
    /// namespace. Unsupported guarantees return `Unsupported` rather than
    /// being emulated with check-then-rename.
    ///
    /// ```no_run
    /// use vfsi_core::api::{RenameOptions, Vfsi};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vrename(
    ///     &[("/old-1", "/new-1"), ("/old-2", "/new-2")],
    ///     Default::default(), // ordinary replacement semantics
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: crate::api::RenameOptions,
    ) -> Result<()>;

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
    /// use vfsi_core::api::{Vfsi, ListDirOptions, ControlFlow};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vlistdirs(&["/tree-1", "/tree-2"],
    ///     ListDirOptions::new().recursive(true).max_depth(8),
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
        options: crate::api::ListDirOptions,
        callback: impl FnMut(usize, DirectoryListing) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::api::TraversalCompletion>>;

    /// Stream files through bounded chunks without collecting whole contents.
    /// The callback receives the input index, byte offset, and borrowed chunk.
    /// False stops the whole vector; results contain its completed/stopped prefix.
    /// Backends may process streams sequentially; this is not a parallelism promise.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, StreamOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vstream(&["/large-1", "/large-2"],
    ///     StreamOptions::new().chunk_size(1024 * 1024), |index, offset, data| {
    ///         println!("{index}: {} bytes at {offset}", data.len());
    ///         Ok(true)
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    fn vstream<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::StreamOptions,
        callback: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
    ) -> Result<Vec<crate::api::StreamCompletion>>;
}

/// Scalar operations and convenience workflows alongside the [`Vfsi`] vectors.
///
/// Blanket-implemented for every filesystem implementing Vfsi. Import `VfsiExt` (or
/// [`crate::api::prelude`]) to use these helpers; generic code needs only an
/// `Vfsi` bound. Helpers preserve batching, resource limits, and non-atomic
/// failure semantics. All helpers compose vectorized Vfsi primitives; native execution belongs in Vfsi.
/// Single-target helpers use conventional names such as `open`, `attrs`,
/// and `read_dir`. Prefer vector APIs for independent work on many files or
/// directories so the backend can batch requests.
///
/// # Method groups
///
/// - Open and close: [`open`](Self::open), [`create`](Self::create).
/// - Read files: [`read_files`](Self::read_files), [`read_files_with_options`](Self::read_files_with_options).
/// - Write files: [`write`](Self::write), [`write_files`](Self::write_files).
/// - Attrs: [`attrs`](Self::attrs), [`attrs_with_options`](Self::attrs_with_options).
/// - Collect directories: [`read_dir`](Self::read_dir), [`read_dir_with_options`](Self::read_dir_with_options).
/// - Visit directories: [`visit_dir`](Self::visit_dir), [`visit_walk`](Self::visit_walk).
/// - Create, move, copy, and remove: [`create_dir`](Self::create_dir), [`create_dir_all`](Self::create_dir_all).
///
/// # Boundary
///
/// | Contract | Responsibility | Examples |
/// | --- | --- | --- |
/// | [`Vfsi`] | Native vector execution, paging, and policy inspection | `vopen`, `vread`, `vlistdirs`, `limits` |
/// | `VfsiExt` | Scalar adapters and composed workflows | `open`, `read_files`, `create_dir_all` |
///
/// Implement only `Vfsi`; this extension is blanket implemented. A helper belongs
/// here only when it can compose Vfsi primitives without losing native batching,
/// bounded paging, or recovery semantics. Singular convenience is not a promise
/// of one RPC, and vector execution is not a promise of atomicity.
pub trait VfsiExt: Vfsi {
    /// Query one target through the vector filesystem-statistics engine.
    fn statfs<P: crate::MetadataOperand<Self::File>>(
        &self,
        target: P,
    ) -> Result<crate::FilesystemStats> {
        let mut results = self.vstatfs(&[target])?;
        if results.len() != 1 {
            return Err(crate::VfError::transport(
                None,
                "statfs backend returned an invalid result count",
            ));
        }
        Ok(results.remove(0))
    }

    // Open and close
    /// Single-target convenience. For multiple files, prefer [`Vfsi::vopen`] to expose batching opportunities.
    ///
    /// Open read-only. Paths are relative to this client's configured namespace.
    ///
    /// Use [`open_with`](VfsiExt::open_with) for write/create flags, or
    /// [`vopen`](Vfsi::vopen) to batch many opens. The returned handle implements
    /// `std::io::Read`/`Write`/`Seek`; use native methods to retain structured errors.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
        self.open_with(OpenOp::new(path.as_ref(), crate::api::OpenFlags::READ))
    }

    /// Single-target convenience. For multiple creations, prefer [`Vfsi::vopen`] with CREATE/TRUNCATE flags.
    ///
    /// Create or truncate a file and open for writing; does not create parents.
    ///
    /// Existing contents are discarded immediately. For exclusive creation,
    /// use [`open_with`](VfsiExt::open_with) with `WRITE | CREATE_NEW` instead.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let file = fs.create("/output")?;
    /// let result = fs.vwrite(&[WriteOp::at(&file, 0, b"complete contents")], vfsi_core::api::WriteOptions::new().write_all(true));
    /// let close = file.close();
    /// result?;
    /// close?;
    /// # Ok(())
    /// # }
    /// ```
    fn create(&self, path: impl AsRef<Path>) -> Result<Self::File> {
        self.open_with(OpenOp::new(
            path.as_ref(),
            crate::api::OpenFlags::WRITE
                | crate::api::OpenFlags::CREATE
                | crate::api::OpenFlags::TRUNCATE,
        ))
    }

    /// Single-target convenience. For multiple requests, prefer [`Vfsi::vopen`] with per-file flags and modes.
    ///
    /// Open with an explicit access/create/truncate request; effects are eager.
    ///
    /// Creation/truncation occurs at open, not on the first write. `CREATE_NEW`
    /// rejects an existing path. This does not create missing parents.
    /// Delegates to singleton `Vfsi::vopen`. Backends must preserve scalar final-
    /// symlink resolution and independently opened handles for singleton vectors.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, OpenFlags, OpenOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let file = fs.open_with(OpenOp::new(
    ///     "/new-file", OpenFlags::WRITE | OpenFlags::CREATE_NEW,
    /// ).mode(0o600))?;
    /// file.close()?;
    /// # Ok(())
    /// # }
    /// ```
    fn open_with(&self, request: OpenOp) -> Result<Self::File> {
        let mut files = self.vopen(&[request])?;
        if files.len() != 1 {
            return Err(crate::api::Error::transport(
                None,
                "vopen returned an invalid result count",
            ));
        }
        Ok(files.remove(0))
    }

    /// Consume all handles. Errors cannot return cleanup ownership; Drop is
    /// best-effort. Prefer `vclose` when close errors require reconciliation.
    ///
    /// This releases all local handles even on failure; it does not promise
    /// every remote CLOSE succeeded. Closing alone is not a durability barrier.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, OpenFlags, OpenOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let files = fs.vopen(&[OpenOp::new("/config", OpenFlags::READ)])?;
    /// fs.close_files(files)?; // Observe a close error instead of discarding it in Drop.
    /// # Ok(())
    /// # }
    /// ```
    fn close_files(&self, mut files: Vec<Self::File>) -> Result<()> {
        self.vclose(&mut files)
    }

    // Read files
    /// Read one complete file within the client's aggregate read budget.
    ///
    /// For multiple files, prefer [`Self::read_files`]. This is a convenience
    /// over the vector read operation and retains its allocation limits.
    fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        self.read_with_options(path, crate::api::ReadOptions::default())
    }

    /// Read one complete file with an explicit read policy.
    fn read_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::ReadOptions,
    ) -> Result<Vec<u8>> {
        let path = path.as_ref();
        let files = self
            .read_files_with_options(&[path], options)
            .map_err(|error| error.with_context("read", path))?;
        single_completion(files, "read")
    }
    /// Read complete files in input order using vectorized whole-file reads.
    ///
    /// The aggregate payload is bounded by `limits().max_read_bytes`.
    /// Use [`Self::read_files_with_options`] to override it, or stream large files.
    /// A failure returns an error, not a successful partial collection.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let contents = fs.read_files(&["/file-1", "/file-2"])?;
    /// for bytes in contents { println!("{} bytes", bytes.len()); }
    /// # Ok(())
    /// # }
    /// ```
    fn read_files<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Vec<u8>>> {
        self.read_files_with_options(paths, crate::api::ReadOptions::default())
    }

    /// Whole-file convenience reads with an explicit aggregate payload budget.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, ReadOptions, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let files = fs.read_files_with_options(&["/config"],
    ///     ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(1024 * 1024)))?;
    /// # let _ = files;
    /// # Ok(())
    /// # }
    /// ```
    fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::ReadOptions,
    ) -> Result<Vec<Vec<u8>>> {
        let results = self.vread(
            paths
                .iter()
                .map(|path| crate::api::ReadOp::whole(path.as_ref())),
            options,
        )?;
        if results.len() != paths.len() {
            return Err(crate::api::Error::transport(
                None,
                "vread_native returned an invalid result count",
            ));
        }
        results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                let data = result.data.ok_or_else(|| {
                    crate::api::Error::transport(
                        Some(index),
                        "whole-file vread_native omitted owned data",
                    )
                })?;
                if data.len() != result.read || result.offset != 0 || !result.eof {
                    return Err(crate::api::Error::transport(
                        Some(index),
                        "whole-file vread_native returned incomplete or invalid data",
                    ));
                }
                Ok(data)
            })
            .collect()
    }

    /// Single-target convenience. For multiple files, prefer [`Vfsi::vread`] with whole-file operations and decode each result as UTF-8.
    ///
    /// Read a complete UTF-8 file within this client's aggregate read budget.
    ///
    /// Invalid UTF-8 is an invalid-input error; use `read_files` for arbitrary bytes.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let text = fs.read_to_string("/config")?;
    /// println!("{text}");
    /// # Ok(())
    /// # }
    /// ```
    fn read_to_string(&self, path: impl AsRef<Path>) -> Result<String> {
        self.read_to_string_with_options(path, crate::api::ReadOptions::default())
    }

    /// Single-target convenience. For multiple files, prefer [`Vfsi::vread`] with an aggregate byte budget, then decode UTF-8.
    ///
    /// Read a complete UTF-8 file with the same budget policy as `Vfsi::vread`.
    /// `ReadOptions::default()` inherits the client limit; explicit overrides are nonzero.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let text = fs.read_to_string_with_options("/config", vfsi_core::api::ReadOptions::new()
    ///     .max_total_bytes(std::num::NonZeroUsize::new(4096)))?;
    /// println!("{text}");
    /// # Ok(())
    /// # }
    /// ```
    fn read_to_string_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::ReadOptions,
    ) -> Result<String> {
        let path = path.as_ref();
        let mut files = self.read_files_with_options(&[path], options)?;
        String::from_utf8(files.remove(0)).map_err(|_| {
            crate::api::Error::client(0, crate::ERR_INVAL).with_context("read_to_string", path)
        })
    }

    /// Single-target convenience. For multiple files, prefer [`Vfsi::vstream`]. Backends may process streams sequentially.
    ///
    /// Stream a file using the client's bounded chunk size instead of collecting it.
    /// Return `Ok(false)` to stop successfully; the callback runs outside the lock.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
    ) -> Result<crate::api::StreamCompletion> {
        self.read_stream_with_options(
            path,
            crate::api::StreamOptions::new().chunk_size(self.limits().stream_chunk_bytes),
            callback,
        )
    }

    /// Single-target convenience. For multiple files, prefer [`Vfsi::vstream`]. Backends may process streams sequentially.
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
    /// use vfsi_core::api::{Vfsi, VfsiExt, StreamOptions, StreamCompletion, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let mut processed = 0_u64;
    /// let completion = fs.read_stream_with_options(
    ///     "/large.bin", StreamOptions::new().chunk_size(1024 * 1024),
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
        options: crate::api::StreamOptions,
        callback: impl FnMut(u64, &[u8]) -> Result<bool>,
    ) -> Result<crate::api::StreamCompletion> {
        let mut callback = callback;
        single_completion(
            self.vstream(&[path], options, |_, offset, data| callback(offset, data))?,
            "read_streams",
        )
    }

    // Write files
    /// Single-target convenience. For multiple complete files, prefer [`VfsiExt::write_files`]; for opened handles, prefer [`Vfsi::vwrite`].
    ///
    /// Replace a file completely, creating/truncating eagerly; not atomic replace.
    ///
    /// The parent must exist. Success writes all bytes and closes the internal
    /// handle; failure may leave a created, truncated, or partially written file.
    /// For exclusive creation or durability control use an explicit open handle.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.write("/output", b"replacement contents")?;
    /// # Ok(())
    /// # }
    /// ```
    fn write(&self, path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
        // Scalar open preserves backend-specific final-symlink resolution.
        // Always attempt close, but retain the write error if both fail.
        let file = self.create(path)?;
        let result = self.vwrite(
            &[crate::api::WriteOp::at(&file, 0, data)],
            crate::api::WriteOptions::new().write_all(true),
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
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
                return Err(crate::api::Error::client(index, crate::ERR_INVAL)
                    .with_context("write_files", path.as_ref()));
            }
        }
        let requests: Vec<_> = entries
            .iter()
            .map(|(path, _)| {
                OpenOp::new(
                    path.as_ref(),
                    crate::api::OpenFlags::WRITE
                        | crate::api::OpenFlags::CREATE
                        | crate::api::OpenFlags::TRUNCATE,
                )
            })
            .collect();
        let files = self.vopen(&requests)?;
        if files.len() != entries.len() {
            return Err(crate::api::Error::transport(
                None,
                "vopen returned an invalid result count",
            ));
        }
        let writes: Vec<_> = files
            .iter()
            .zip(entries)
            .map(|(file, (_, data))| crate::api::WriteOp::at(file, 0, data.as_ref()))
            .collect();
        let result = self.vwrite(&writes, crate::api::WriteOptions::new().write_all(true));
        drop(writes);
        let close_result = self.close_files(files);
        result?;
        close_result
    }

    // Attrs
    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vgetattrs`].
    ///
    /// Query a path following its final symlink; unavailable fields remain None.
    ///
    /// To inspect the symlink itself use [`symlink_attrs`](VfsiExt::symlink_attrs).
    /// To identify an already opened object after rename, use [`FileHandle::attrs`].
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let attrs = fs.attrs("/file-1")?;
    /// println!("{} bytes; directory={}", attrs.len(), attrs.is_dir());
    /// # Ok(())
    /// # }
    /// ```
    fn attrs(&self, path: impl AsRef<Path>) -> Result<Attrs> {
        attrs_query(self, path, crate::api::AttrsOptions::new(), "attrs")
    }

    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vgetattrs`] with selected fields and follow_symlinks(false).
    ///
    /// Query one path with selected fields and final-symlink behavior; absent fields remain None.
    ///
    /// This avoids requesting every optional attribute. Type/size can be
    /// requested internally even when not selected; missing optional attributes
    /// must still be handled via their `Option` accessors.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, Attributes, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let attrs = fs.attrs_with_options(
    ///     "/file-1", vfsi_core::api::AttrsOptions::new()
    ///         .fields(Attributes::MODE | Attributes::BLOCKS).follow_symlinks(false),
    /// )?;
    /// if let Some(blocks) = attrs.blocks() {
    ///     println!("allocated blocks: {blocks}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn attrs_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::AttrsOptions,
    ) -> Result<Attrs> {
        attrs_query(self, path, options, "attrs_with_options")
    }

    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vgetattrs`] with follow_symlinks(false).
    ///
    /// Query the final symlink itself instead of following it.
    ///
    /// This does not prevent following symlinks in ancestor components and is
    /// not a race-free namespace confinement primitive.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let attrs = fs.symlink_attrs("/link-or-file")?;
    /// println!("symlink={}", attrs.is_symlink());
    /// # Ok(())
    /// # }
    /// ```
    fn symlink_attrs(&self, path: impl AsRef<Path>) -> Result<Attrs> {
        attrs_query(
            self,
            path,
            crate::api::AttrsOptions::new().follow_symlinks(false),
            "symlink_attrs",
        )
    }

    // Collect directories
    /// Single-target convenience. For multiple directories, prefer [`VfsiExt::read_dirs_with_options`] or [`VfsiExt::read_dirs`].
    ///
    /// Collect one directory using the client's entry and path-byte limits.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// for entry in fs.read_dir("/input")? {
    ///     println!("{}", entry.path().display());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dir(&self, path: impl AsRef<Path>) -> Result<Vec<crate::api::DirEntry>> {
        self.read_dir_with_options(
            path,
            crate::api::ListDirOptions::new().fields(crate::api::Attributes::stat()),
        )
    }

    /// Single-target convenience. For multiple directories, prefer [`VfsiExt::read_dirs_with_options`] with one aggregate budget.
    ///
    /// Collect one shallow directory with selected metadata and entry/path-byte limits.
    /// This helper forces `recursive(false)`; use `walk_with_options` for a tree.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let entries = fs.read_dir_with_options("/input",
    ///     vfsi_core::api::ListDirOptions::new().max_entries(100))?;
    /// println!("{} entries", entries.len());
    /// # Ok(())
    /// # }
    /// ```
    fn read_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::ListDirOptions,
    ) -> Result<Vec<crate::api::DirEntry>> {
        let mut trees = self.read_dirs_with_options(&[path], options.recursive(false))?;
        let mut listings = single_tree(&mut trees)?;
        if listings.len() != 1 {
            return Err(crate::api::Error::transport(
                None,
                "read_dirs returned an invalid result count",
            ));
        }
        Ok(listings.remove(0).entries)
    }

    /// Collect directory listings under one aggregate entry/path-byte policy.
    ///
    /// There is one listing per input directory, in input order; entry order is
    /// backend-defined. Entries include metadata, avoiding a separate scalar
    /// stat per child. The default policy comes from this client's limits.
    /// Use [`read_dirs_with_options`](VfsiExt::read_dirs_with_options) to select
    /// attributes, or a visitor instead of collecting large listings.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// for listing in fs.read_dirs(&["/input", "/output"])? {
    ///     for entry in listing.entries {
    ///         println!("{}: {} bytes", entry.path().display(), entry.attrs().len());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>> {
        let trees = self.read_dirs_with_options(
            paths,
            crate::api::ListDirOptions::new().fields(crate::api::Attributes::stat()),
        )?;
        if trees.len() != paths.len() || trees.iter().any(|tree| tree.len() != 1) {
            return Err(crate::api::Error::transport(
                None,
                "read_dirs returned an invalid result count",
            ));
        }
        Ok(trees.into_iter().flatten().collect())
    }

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
    /// use vfsi_core::api::{Vfsi, VfsiExt, Attributes, ListDirOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let trees = fs.read_dirs_with_options(&["/input", "/output"],
    ///     ListDirOptions::new().recursive(true).fields(Attributes::MODE)
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
        options: crate::api::ListDirOptions,
    ) -> Result<Vec<Vec<DirectoryListing>>> {
        let mut trees: Vec<Vec<DirectoryListing>> = (0..paths.len()).map(|_| Vec::new()).collect();
        let mut positions: Vec<std::collections::HashMap<std::path::PathBuf, usize>> = (0..paths
            .len())
            .map(|_| std::collections::HashMap::new())
            .collect();
        self.vlistdirs(paths, options, |index, page| {
            let tree = trees
                .get_mut(index)
                .ok_or_else(|| crate::api::Error::transport(None, "invalid visitor root index"))?;
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

    /// Single-target convenience. For multiple roots, prefer [`VfsiExt::read_dirs_with_options`].
    ///
    /// Collect a no-follow tree using the client's entry, byte, and depth limits.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// for listing in fs.walk("/project")? {
    ///     println!("{}", listing.path.display());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn walk(&self, root: impl AsRef<Path>) -> Result<Vec<DirectoryListing>> {
        self.walk_with_options(
            root,
            crate::api::ListDirOptions::new().fields(crate::api::Attributes::stat()),
        )
    }

    /// Single-target convenience. For multiple roots, prefer [`VfsiExt::read_dirs_with_options`] with one aggregate budget.
    ///
    /// Collect a bounded tree, without following symlinks; no snapshot promise.
    ///
    /// Returns directory listings, not one flattened entry vector. Budgets
    /// apply across the walk; depth zero is the starting directory. Explicit
    /// options override client defaults. This helper forces `recursive(true)`.
    /// Use a visitor for incremental delivery
    /// or [`walk_events_with_options`](VfsiExt::walk_events_with_options) for pruning.
    ///
    /// ```no_run
    /// use vfsi_core::api::{ListDirOptions, Vfsi, VfsiExt, Attributes, WalkOptions, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let listings = fs.walk_with_options(
    ///     "/project", ListDirOptions::new().fields(Attributes::MODE | Attributes::SIZE)
    ///         .max_entries(10_000).max_path_bytes(1024 * 1024).max_depth(8),
    /// )?;
    /// println!("{} directory listings", listings.len());
    /// # Ok(())
    /// # }
    /// ```
    fn walk_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::ListDirOptions,
    ) -> Result<Vec<DirectoryListing>> {
        let mut trees = self.read_dirs_with_options(&[path], options.recursive(true))?;
        if trees.len() != 1 {
            return Err(crate::api::Error::transport(
                None,
                "walks returned an invalid result count",
            ));
        }
        Ok(trees.remove(0))
    }

    // Visit directories
    /// Single-target convenience. For multiple directories, prefer [`Vfsi::vlistdirs`].
    ///
    /// Visit a directory incrementally using the client's allocation limits.
    /// The callback runs outside the backend lock.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
        callback: impl FnMut(&crate::api::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::api::TraversalCompletion> {
        self.visit_dir_with_options(path, crate::api::ListDirOptions::new(), callback)
    }

    /// Single-target convenience. For multiple roots, prefer [`Vfsi::vlistdirs`].
    ///
    /// Visit a no-follow tree incrementally using the client's traversal limits.
    /// The callback runs outside the backend lock.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
        callback: impl FnMut(&crate::api::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::api::TraversalCompletion> {
        self.visit_dir_with_options(
            root,
            crate::api::ListDirOptions::new().recursive(true),
            callback,
        )
    }

    /// Single-target convenience. For multiple directories, prefer [`Vfsi::vlistdirs`].
    ///
    /// Visit one directory; `Break(())` returns Stopped, exhaustion returns
    /// Complete, and callback errors propagate. The callback may reenter.
    ///
    /// Defaults to immediate children. Set `recursive(true)` to visit descendants
    /// without following entry symlinks. Options are forwarded unchanged, including
    /// entry, path-byte, and depth limits. Paging bounds incremental delivery;
    /// backends without paging may buffer one bounded listing. Entry order is
    /// backend-defined. `Break(())` stops the entire traversal, not one subtree.
    /// To prune a subtree, use [`walk_events_with_options`](Self::walk_events_with_options).
    ///
    /// ```no_run
    /// use vfsi_core::api::{ListDirOptions, Vfsi, VfsiExt, ControlFlow, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let completion = fs.visit_dir_with_options(
    ///     "/input", ListDirOptions::new().max_entries(10_000), |entry| {
    ///         println!("{}", entry.path().display());
    ///         Ok(ControlFlow::Continue(()))
    ///     },
    /// )?;
    /// # let _ = completion;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Recursive traversal uses the same entry point:
    ///
    /// ```no_run
    /// use vfsi_core::api::{ControlFlow, VfsiExt, ListDirOptions};
    /// # fn example(fs: &impl vfsi_core::Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.visit_dir_with_options("/project", ListDirOptions::new().recursive(true), |entry| {
    ///     println!("{}", entry.path().display());
    ///     Ok(ControlFlow::Continue(()))
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    fn visit_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::ListDirOptions,
        callback: impl FnMut(&crate::api::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::api::TraversalCompletion> {
        let mut callback = callback;
        single_completion(
            self.visit_entries_with_options(&[path], options, |_, entry| callback(entry))?,
            "visit_dirs",
        )
    }

    /// Visit individual entries using the directory-page primitive.
    /// Empty directories produce pages but no entry callbacks.
    fn visit_entries_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::ListDirOptions,
        mut callback: impl FnMut(usize, &crate::api::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::api::TraversalCompletion>> {
        self.vlistdirs(paths, options, |index, page| {
            for entry in &page.entries {
                if callback(index, entry)?.is_break() {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
    }

    /// Single-target convenience. For multiple roots without enter/leave events or subtree pruning, prefer [`Vfsi::vlistdirs`]. Keep this helper when those event semantics are required.
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
    /// use vfsi_core::api::{ListDirOptions, Vfsi, VfsiExt, Attributes, WalkControl, WalkEventKind, WalkOptions, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let completion = fs.walk_events_with_options(
    ///     "/project", Attributes::MODE, WalkOptions::new(), true,
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
        fields: crate::api::Attributes,
        options: crate::api::WalkOptions,
        sort_by_name: bool,
        callback: impl FnMut(&crate::api::WalkEvent) -> Result<crate::api::WalkControl>,
    ) -> Result<crate::api::TraversalCompletion> {
        let root = root.as_ref();
        let fields = fields | crate::api::Attributes::MODE;
        let metadata = self.attrs_with_options(
            root,
            crate::api::AttrsOptions::new()
                .fields(fields)
                .follow_symlinks(false),
        )?;
        crate::api::walk_events(
            crate::api::DirEntry::new(root.to_path_buf(), metadata),
            options,
            sort_by_name,
            |path, limits| {
                let mut trees = self.read_dirs_with_options(
                    &[path],
                    crate::api::ListDirOptions::from(limits).fields(fields),
                )?;
                let mut listings = single_tree(&mut trees)?;
                if listings.len() != 1 {
                    return Err(crate::api::Error::transport(
                        None,
                        "invalid directory result count",
                    ));
                }
                Ok(listings.remove(0).entries)
            },
            callback,
        )
    }

    /// Visit complete shallow listings in depth-first order with application policy.
    ///
    /// `order` may reorder siblings (for example, locale-aware `ls` ordering).
    /// `descend` selects directories before their contents are read; it does not
    /// filter entries delivered in their parent's listing. Symlinks are never
    /// traversed. `visitor` runs outside backend locks and may stop immediately
    /// or skip all children of the current directory. Errors after delivered
    /// listings must not trigger replay through another backend.
    ///
    /// Storage is bounded by one complete listing and the pending directory
    /// frontier. Entry/path budgets charge all fetched children, including those
    /// rejected by `descend`. This ordered workflow deliberately does not
    /// speculatively list children before admission; use `vlistdirs` to batch
    /// already-approved independent directories.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Attributes, Result, Vfsi, VfsiExt, WalkControl};
    /// # fn example(fs: &impl Vfsi) -> Result<()> {
    /// fs.visit_dirs_ordered("/input", Attributes::MODE | Attributes::SIZE,
    ///     fs.limits().walk_options(),
    ///     |entries| entries.sort_by(|a, b| a.path().cmp(b.path())),
    ///     |entry| entry.file_name() != Some(std::ffi::OsStr::new(".git")),
    ///     |listing, depth| {
    ///         println!("{}: {} entries at depth {depth}", listing.path.display(), listing.entries.len());
    ///         Ok(WalkControl::Continue)
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    fn visit_dirs_ordered(
        &self,
        root: impl AsRef<Path>,
        fields: crate::api::Attributes,
        options: crate::api::WalkOptions,
        mut order: impl FnMut(&mut [crate::api::DirEntry]),
        mut descend: impl FnMut(&crate::api::DirEntry) -> bool,
        mut visitor: impl FnMut(DirectoryListing, usize) -> Result<crate::api::WalkControl>,
    ) -> Result<crate::api::TraversalCompletion> {
        let root = root.as_ref();
        let mut count = 1usize;
        let mut bytes = root.as_os_str().len();
        if count > options.entry_limit() || bytes > options.path_byte_limit() {
            return Err(crate::api::Error::client(0, libc::EFBIG as u32));
        }
        let mut pending = vec![(root.to_path_buf(), 0usize)];
        while let Some((path, depth)) = pending.pop() {
            if depth >= options.depth_limit() && options.truncates_at_depth_limit() {
                continue;
            }
            let mut entries = self.read_dir_with_options(
                &path,
                crate::api::ListDirOptions::new()
                    .fields(fields | crate::api::Attributes::MODE)
                    .max_entries(options.entry_limit().saturating_sub(count))
                    .max_path_bytes(options.path_byte_limit().saturating_sub(bytes)),
            )?;
            for entry in &entries {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| crate::api::Error::client(0, libc::EFBIG as u32))?;
                bytes = bytes
                    .checked_add(entry.path().as_os_str().len())
                    .ok_or_else(|| crate::api::Error::client(0, libc::EFBIG as u32))?;
                if count > options.entry_limit()
                    || bytes > options.path_byte_limit()
                    || depth >= options.depth_limit()
                {
                    return Err(crate::api::Error::client(0, libc::EFBIG as u32)
                        .with_context("visit_dirs_ordered", entry.path()));
                }
            }
            order(&mut entries);
            let listing = DirectoryListing { path, entries };
            let children: Vec<_> = listing
                .entries
                .iter()
                .filter(|entry| entry.attrs().is_dir() && descend(entry))
                .map(|entry| (entry.path().to_path_buf(), depth + 1))
                .collect();
            match visitor(listing, depth)? {
                crate::api::WalkControl::Stop => {
                    return Ok(crate::api::TraversalCompletion::Stopped);
                }
                crate::api::WalkControl::SkipSubtree => continue,
                crate::api::WalkControl::Continue => {}
            }
            pending.extend(children.into_iter().rev());
        }
        Ok(crate::api::TraversalCompletion::Complete)
    }

    // Create, move, copy, and remove
    /// Single-target convenience. For multiple independent directories, prefer [`Vfsi::vmkdir`].
    ///
    /// Create one directory; its parent must exist.
    ///
    /// An existing entry is an error, even if already a directory. Use
    /// [`create_dir_all`](VfsiExt::create_dir_all) for missing parents/idempotent
    /// directory setup, or [`vmkdir`](Vfsi::vmkdir) for independent siblings.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.create_dir("/fresh-directory")?;
    /// # Ok(())
    /// # }
    /// ```
    fn create_dir(&self, path: impl AsRef<Path>) -> Result<()> {
        self.vmkdir(&[(path, 0o777)])
    }

    /// Truncate or extend one path or opened object using [`Vfsi::vsetattrs`].
    fn truncate<T: crate::api::MetadataOperand<Self::File>>(
        &self,
        target: T,
        len: u64,
    ) -> Result<()> {
        self.vsetattrs(
            &[(target, crate::api::MetadataUpdate::new().len(len))],
            true,
        )
    }

    /// Change permissions on one path or opened object using [`Vfsi::vsetattrs`].
    fn chmod<T: crate::api::MetadataOperand<Self::File>>(
        &self,
        target: T,
        permissions: crate::api::Permissions,
    ) -> Result<()> {
        self.vsetattrs(
            &[(
                target,
                crate::api::MetadataUpdate::new().permissions(permissions),
            )],
            true,
        )
    }

    /// Change ownership of one path or opened object through [`Vfsi::vsetattrs`].
    /// `None` leaves the corresponding owner/group unchanged. Use the vector
    /// with `follow_symlinks = false` to change a symlink itself.
    fn chown<T: crate::api::MetadataOperand<Self::File>>(
        &self,
        target: T,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<()> {
        let mut update = crate::api::MetadataUpdate::new();
        update.uid = uid;
        update.gid = gid;
        self.vsetattrs(&[(target, update)], true)
    }

    /// Create one directory with explicit Unix permission bits.
    fn create_dir_with_mode(&self, path: impl AsRef<Path>, mode: u32) -> Result<()> {
        self.vmkdir(&[(path, mode)])
    }

    /// Create one symbolic link; submit multiple pairs with [`Vfsi::vsymlink`].
    fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
        self.vsymlink(&[(target, link)])
    }

    /// Read one symlink target; submit multiple paths with [`Vfsi::vreadlink`].
    fn read_link(&self, path: impl AsRef<Path>) -> Result<std::path::PathBuf> {
        let mut targets = self.vreadlink(&[path])?;
        if targets.len() != 1 {
            return Err(crate::api::Error::transport(
                None,
                "invalid readlink result count",
            ));
        }
        Ok(targets.pop().expect("validated readlink count"))
    }

    /// Create one hard link; submit multiple pairs with [`Vfsi::vhardlink`].
    fn hard_link(&self, source: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
        self.vhardlink(&[(source, link)])
    }

    /// Single-target convenience. For multiple directory creations, prefer [`Vfsi::vmkdir`]; plan missing parents before their children.
    ///
    /// Create missing parents; an error can leave some directories created.
    ///
    /// Existing directories are accepted; an existing non-directory component
    /// is an error. This is not atomic and is not protected against concurrent
    /// changes to ancestor paths.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
                    return Err(crate::api::Error::client(0, crate::ERR_INVAL));
                }
            }
            match self.vmkdir(&[(&current, 0o777)]) {
                Ok(()) => {}
                Err(error) if error.err_no() == crate::ERR_EXIST => {
                    if !self.attrs(&current)?.is_dir() {
                        return Err(crate::api::Error::client(0, crate::ERR_NOTDIR)
                            .with_context("create_dir_all", &current));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Single-target convenience. For multiple source/destination pairs, prefer [`Vfsi::vrename`].
    ///
    /// Rename within supported namespaces; cross-filesystem moves can fail.
    ///
    /// Both paths use this client's namespace. Replacement semantics follow
    /// the backend/filesystem; this is not a cross-file vector transaction or
    /// a guarantee of durable directory updates. No copy fallback is implied.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.rename("/old-name", "/new-name")?;
    /// # Ok(())
    /// # }
    /// ```
    fn rename(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
        self.vrename(&[(source, destination)], crate::api::RenameOptions::Replace)
    }

    /// Single-target convenience. For multiple file pairs, prefer [`Vfsi::vcopy`].
    ///
    /// Copy a file's contents; this is not recursive tree copying.
    ///
    /// Destination creation/replacement and failures follow backend semantics.
    /// A failure can leave a partial destination. Do not assume metadata,
    /// sparse layout, or durability is preserved. Server-side copy is used
    /// only where supported; callers need not build protocol COPY requests.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.copy("/source", "/destination")?;
    /// # Ok(())
    /// # }
    /// ```
    fn copy(&self, source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
        self.copy_with_options(source, destination, CopyOption::default())
    }

    /// Single-target copy with explicit operation semantics; prefer [`Vfsi::vcopy`]
    /// when submitting multiple pairs.
    fn copy_with_options(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        options: CopyOption,
    ) -> Result<()> {
        self.vcopy(&[(source, destination)], options)
    }

    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vremove`]. That vector API accepts both files and directories.
    ///
    /// Remove one file or symlink, not the symlink target.
    ///
    /// Missing paths are errors. To remove a directory use
    /// [`remove_dir`](VfsiExt::remove_dir) or [`remove_dir_all`](VfsiExt::remove_dir_all).
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, ErrorKind, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
        if self.symlink_attrs(path)?.is_dir() {
            return Err(
                crate::api::Error::client(0, crate::ERR_ISDIR).with_context("remove_file", path)
            );
        }
        self.vremove(
            &[path],
            crate::api::RemoveMode::Entry,
            crate::api::RemoveOptions::new(),
        )
    }

    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vremove`]. That vector API does not enforce directory-only inputs.
    ///
    /// Remove one empty directory; not a recursive operation.
    ///
    /// Nonempty directories fail. Use [`remove_dir_contents`](VfsiExt::remove_dir_contents)
    /// to empty a directory while retaining it, or [`remove_dir_all`](VfsiExt::remove_dir_all)
    /// to remove its tree. Only operate on paths whose removal you intend.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.remove_dir("/empty-temporary-directory")?;
    /// # Ok(())
    /// # }
    /// ```
    fn remove_dir(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        require_directory(self, path, "remove_dir")?;
        self.vremove(
            &[path],
            crate::api::RemoveMode::Entry,
            crate::api::RemoveOptions::new(),
        )
    }

    /// Single-target convenience. For multiple trees, prefer [`Vfsi::vremove`] with `RemoveMode::Tree`.
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
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
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
            crate::api::RemoveMode::Tree,
            crate::api::RemoveOptions::new(),
        )
    }

    /// Recursively remove one directory using explicit batching and failure
    /// policy. The directory is validated before mutation; symlink roots are
    /// not followed. Prefer [`Vfsi::vremove`] when removing several trees.
    fn remove_dir_all_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::RemoveOptions,
    ) -> Result<()> {
        let path = path.as_ref();
        require_directory(self, path, "remove_dir_all")?;
        self.vremove(&[path], crate::api::RemoveMode::Tree, options)
    }

    /// Single-target convenience. For multiple directory roots, prefer [`Vfsi::vremove`].
    ///
    /// Recursively empty a directory while keeping its root.
    ///
    /// The root must be a directory, not a symlink. Child symlinks are removed
    /// without following their targets. Errors can leave partially removed
    /// contents; retaining the root does not make the operation transactional.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.remove_dir_contents("/owned-scratch-directory")?;
    /// // The scratch directory remains available for subsequent work.
    /// # Ok(())
    /// # }
    /// ```
    fn remove_dir_contents(&self, path: impl AsRef<Path>) -> Result<()> {
        self.remove_dir_contents_with_options(path, crate::api::RemoveOptions::new())
    }

    /// Recursively empty one directory while retaining its root, using
    /// explicit batching and failure policy. The root is validated without
    /// following a symlink. Prefer [`Vfsi::vremove`] for several roots.
    fn remove_dir_contents_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::RemoveOptions,
    ) -> Result<()> {
        let path = path.as_ref();
        require_directory(self, path, "remove_dir_contents")?;
        self.vremove(&[path], crate::api::RemoveMode::Contents, options)
    }
}

impl<C: Vfsi + ?Sized> VfsiExt for C {}

fn single_completion<T>(mut values: Vec<T>, operation: &'static str) -> Result<T> {
    if values.len() != 1 {
        return Err(crate::api::Error::transport(
            None,
            format!("{operation} returned an invalid result count"),
        ));
    }
    Ok(values.remove(0))
}
fn require_directory<C: Vfsi + ?Sized>(fs: &C, path: &Path, operation: &'static str) -> Result<()> {
    if !fs.symlink_attrs(path)?.is_dir() {
        return Err(crate::api::Error::client(0, crate::ERR_NOTDIR).with_context(operation, path));
    }
    Ok(())
}
fn attrs_query<C: Vfsi + ?Sized>(
    client: &C,
    path: impl AsRef<Path>,
    options: crate::api::AttrsOptions,
    operation: &'static str,
) -> Result<Attrs> {
    let path = path.as_ref();
    let mut results = client
        .vgetattrs(&[path], options)
        .map_err(|error| error.with_context(operation, path))?;
    if results.len() != 1 {
        return Err(crate::api::Error::transport(
            None,
            "vgetattrs returned an invalid result count",
        )
        .with_context(operation, path));
    }
    Ok(results.remove(0))
}

fn single_tree(trees: &mut Vec<Vec<DirectoryListing>>) -> Result<Vec<DirectoryListing>> {
    if trees.len() != 1 {
        return Err(crate::api::Error::transport(
            None,
            "read_dirs returned an invalid root count",
        ));
    }
    Ok(trees.remove(0))
}
