use crate::api::{
    Attrs, CopyOption, DirectoryListing, MkDirOp, OpenOp, ResourceLimits, Result, SetAttrsOp,
    WriteResult,
};
use std::path::Path;

use super::{DirHandle, FileHandle};

/// Vectorized filesystem operations for direct and routed application clients.
/// Construction, builders and protocol-specific diagnostics stay concrete.
/// Vectors are strict, ordered results on success, not transactions: errors
/// can follow partially completed requests and never imply rollback.
/// Generic vector and callback methods require static generic dispatch, not
/// `dyn Vfsi`. It adds no boxing, data copies or serial-loop fallbacks.
///
/// # Choosing an operation
///
/// | Task | Start with |
/// | --- | --- |
/// | Complete small files | [`vread`](Self::vread), [`write_files`](super::VfsiExt::write_files) |
/// | Repeated/range I/O on owned handles | [`vopen`](Self::vopen), [`vread`](Self::vread), [`vwrite`](Self::vwrite) |
/// | Large files without collecting them | [`vstream`](Self::vstream) |
/// | Directory pages with entry metadata | [`vlistdirs`](Self::vlistdirs) |
/// | Recursive directory pages | [`vlistdirs`](Self::vlistdirs) with [`ListDirOptions::recursive`](crate::api::ListDirOptions::recursive) |
///
/// Generic application code needs a `Vfsi` bound. Import [`VfsiExt`](super::VfsiExt) for
/// convenience operations such as `read_files`, `write_files`, and scalar open.
/// [`VfsiExt::read_dirs_with_options`](super::VfsiExt::read_dirs_with_options) collects directory pages into vectors;
/// [`VfsiExt::read_stream_with_options`](super::VfsiExt::read_stream_with_options) adapts streaming to a single path.
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
/// Writes are not automatically durable: use [`Vfsi::vfsync`] on open handles
/// when required. Drop queues handle cleanup
/// best-effort; explicit close methods let applications observe cleanup errors.
pub trait Vfsi {
    /// Query filesystems for paths (following symlinks) and retained open handles.
    /// Results preserve input order. Unsupported fields are `None`.
    fn vstatfs<P: crate::AsTarget<Self::File>>(
        &self,
        targets: &[P],
    ) -> Result<Vec<crate::FilesystemStats>>;

    /// Query paths and retained handles in input order with selected fields.
    /// Final-symlink behavior applies to paths; handles retain their opened
    /// identity after rename/unlink. Preflight every handle's ownership and live
    /// state before dispatching any query. Empty vectors perform no I/O.
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
    fn vgetattrs<P: crate::AsTarget<Self::File>>(
        &self,
        paths: &[P],
        options: crate::api::AttrsOptions,
    ) -> Result<Vec<Attrs>>;

    /// Synchronize retained handles in input order. All handles must belong to
    /// this client and be live; validate the entire vector before dispatch.
    /// Errors may follow completed synchronization of earlier handles. Empty
    /// vectors succeed without I/O; ambiguous failures must not be replayed.
    fn vfsync(&self, files: &[&Self::File], mode: crate::api::SyncMode) -> Result<()>;

    /// Update selected attributes for paths or open handles using native batching.
    /// Every handle is validated before dispatch, including ownership and closure.
    /// Unspecified fields are unchanged. Each [`SetAttrsOp`] controls whether
    /// its final path symlink is followed; this does not change open-handle identity.
    /// Ancestor symlinks retain ordinary backend resolution.
    /// Equal-policy runs remain batched; mixed policies retain input order.
    /// Validate every input before any run mutates the filesystem.
    /// Failure can follow partial mutations, including within one request;
    /// an error index identifies an input, not a committed-prefix count.
    /// Empty vectors succeed without I/O. Do not replay ambiguous failures.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, Permissions, SetAttrsOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vsetattrs(&[
    ///     SetAttrsOp::new("/file-1").permissions(Permissions::from_mode(0o640)).len(1024),
    ///     SetAttrsOp::new("/file-2").len(0),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    /// Handle targets and paths can share a batch:
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, Target, SetAttrsOp};
    /// # fn example<F: Vfsi>(fs: &F, file: &F::File) -> vfsi_core::api::Result<()> {
    /// fs.vsetattrs(&[
    ///     SetAttrsOp::file(file).len(1024),
    ///     SetAttrsOp::new(Target::Path(std::path::Path::new("/other"))).len(0)
    ///         .follow_symlinks(false),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    fn vsetattrs<P: crate::api::AsTarget<Self::File>>(
        &self,
        updates: &[SetAttrsOp<P>],
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

    /// Owned directory handle for this client's retained namespace objects.
    type Dir: DirHandle + 'static;

    /// Open genuine directory handles in input order without following final
    /// symlinks. Path-only backends must report unsupported. On error, opened
    /// handles are cleaned up; an empty vector performs no I/O.
    fn vopen_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Self::Dir>> {
        if paths.is_empty() {
            Ok(Vec::new())
        } else {
            Err(crate::api::Error::unsupported(0))
        }
    }

    /// Remove contents anchored to retained directories, preserving the roots.
    /// Validate every handle's ownership and live state before any mutation.
    /// Never reopen diagnostic paths. Errors can follow partial mutation; an
    /// index identifies an input, not a committed prefix. Do not replay failures.
    fn vremove_dir_contents(
        &self,
        dirs: &[&Self::Dir],
        options: crate::RemoveOptions,
    ) -> Result<()> {
        let _ = options;
        if dirs.is_empty() {
            Ok(())
        } else {
            Err(crate::api::Error::unsupported(0))
        }
    }

    /// Owned handle; vectors must contain handles belonging to this client.
    type File: FileHandle + 'static;

    /// Collection/batch defaults, not a process memory cap or file-reader cap.
    ///
    /// This only inspects policy; it performs no filesystem I/O. Configure a
    /// concrete client's builder or `with_limits` to change its defaults.
    ///
    /// ```no_run
    /// use vfsi_core::api::Vfsi;
    /// # fn example(fs: &impl Vfsi) {
    /// let limits = fs.limits();
    /// println!("owned-read budget: {} bytes", limits.read_byte_limit());
    /// println!("walk depth: {}", limits.walk_depth_limit());
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
    /// The default aggregate budget comes from `limits().read_byte_limit()`.
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
    /// Handles opened for append select EOF regardless of the requested offset.
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
    /// Unlike [`close_files`](super::VfsiExt::close_files), this borrows the handles. After an error,
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
    /// use vfsi_core::api::{Vfsi, VfsiExt, MkDirOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.create_dir_all("/workspace")?;
    /// fs.vmkdir(&[MkDirOp::new("/workspace/input", 0o750), MkDirOp::new("/workspace/output", 0o700)])?;
    /// # Ok(())
    /// # }
    /// ```
    /// Each [`MkDirOp`] supplies a path and Unix permission bits; backends apply the
    /// requested mode rather than relying on the process umask. Unsupported
    /// permission semantics are reported by the backend.
    fn vmkdir<P: AsRef<Path>>(&self, directories: &[MkDirOp<P>]) -> Result<()>;
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
    /// `ControlFlow::Break(())` stops the whole vector; results contain its completed/stopped prefix.
    /// Backends may process streams sequentially; this is not a parallelism promise.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, StreamOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.vstream(&["/large-1", "/large-2"],
    ///     StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(1024 * 1024).unwrap()), |index, offset, data| {
    ///         println!("{index}: {} bytes at {offset}", data.len());
    ///         Ok(std::ops::ControlFlow::Continue(()))
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    fn vstream<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::StreamOptions,
        callback: impl FnMut(usize, u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::api::StreamCompletion>>;
}
