use crate::api::{
    Attrs, CopyOptions, DirectoryListing, MkDirOp, OpenOp, ResourceLimits, Result, SetAttrsOp,
    WriteResult,
};
use std::path::Path;

use super::{DirHandle, FileHandle};

/// Vectorized filesystem operations for direct and routed application clients.
/// Construction, builders and protocol-specific diagnostics stay concrete.
/// Metadata, I/O, lifecycle, namespace and directory vectors return per-input
/// outcomes, including file streaming. Result position follows input order,
/// not execution order. Errors can follow partial effects and imply no rollback.
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
/// | Recursive directory pages | [`vlistdirs`](Self::vlistdirs) with [`ListDirOptions::with_recursive`](crate::api::ListDirOptions::with_recursive) |
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
/// A failed mutation can have partial effects. The result slot identifies its
/// input; an `OpError` records completion certainty and acknowledged I/O progress.
/// Neither implies a committed prefix. Preserve that evidence rather than
/// blindly replaying a failed write. Strict `VfsiExt` conveniences deliberately
/// discard sibling outcomes when selecting one diagnostic [`crate::api::Error`].
///
/// Allocation/traversal defaults come from [`limits`](Self::limits). Explicit
/// per-call options override them; neither is a process-wide memory cap.
/// Calls are synchronous. Callback methods invoke user code outside the backend
/// lock and propagate callback errors, but do not provide a filesystem snapshot.
/// Writes are not automatically durable: use [`Vfsi::vfsync`] on open handles
/// when required. Drop queues handle cleanup
/// best-effort; explicit close methods let applications observe cleanup errors.
pub trait Vfsi {
    /// Whether object IDs from these paths share a backend/server namespace.
    /// Single-backend clients use the default. Routers must override it;
    /// equal numeric IDs from unrelated servers do not identify one object.
    /// This is route discovery, not an object-equality or existence check.
    fn same_object_namespace(&self, _left: &Path, _right: &Path) -> bool {
        true
    }

    /// Query filesystems for paths (following symlinks) and retained open handles.
    /// Exactly one outcome per input, in input order; execution may be unordered.
    /// Outer errors reject the call before dispatch. Item failures do not suppress
    /// independent siblings. Unsupported fields are `None`.
    /// Use VfsiExt::statfs_all for lossy all-success collection.
    fn vstatfs<P: crate::AsTarget<Self::File>>(
        &self,
        targets: &[P],
    ) -> crate::VecResult<crate::FilesystemStats>;

    /// Query paths and retained handles in input order with selected fields.
    /// Final-symlink behavior applies to paths; handles retain their opened
    /// identity after rename/unlink. Invalid handles receive individual errors
    /// before backend dispatch; independent valid inputs still execute.
    /// Return one outcome per input in input order, without prescribing execution
    /// order. Outer errors reject the whole call before dispatch; failures after
    /// dispatch stay in the input slots. Empty vectors perform no I/O.
    /// Backend execution must preserve vector batching. Ancestor symlinks use
    /// ordinary namespace resolution; this is not a snapshot or confinement API.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, AttrsOptions, Attributes, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let entries = fs.getattrs_all(&["/file-1", "/link"],
    ///     AttrsOptions::new().with_fields(Attributes::MODE | Attributes::SIZE)
    ///         .with_follow_final_symlink(false))?;
    /// # let _ = entries;
    /// # Ok(())
    /// # }
    /// ```
    fn vgetattrs<P: crate::AsTarget<Self::File>>(
        &self,
        paths: &[P],
        options: crate::api::AttrsOptions,
    ) -> crate::VecResult<Attrs>;

    /// Synchronize independent retained handles, preserving one outcome per input.
    /// Closed or foreign handles are NotAttempted without suppressing valid
    /// siblings. Outcome position identifies the input, not execution order.
    /// Empty vectors perform no I/O; ambiguous failures are not automatically replayed.
    fn vfsync(&self, files: &[&Self::File], mode: crate::api::SyncMode) -> crate::VecResult<()>;

    /// Update selected attributes for paths or open handles using native batching.
    /// Every handle is validated before dispatch, including ownership and closure.
    /// Unspecified fields are unchanged. Each [`SetAttrsOp`] controls whether
    /// its final path symlink is followed; this does not change open-handle identity.
    /// Ancestor symlinks retain ordinary backend resolution.
    /// Equal-policy runs remain batched. Invalid, closed, or foreign targets
    /// are NotAttempted without suppressing valid siblings. Outcome position
    /// identifies the input, not completion order or a committed prefix.
    /// A failed operation may have changed some attributes; Unknown means its
    /// effects cannot be confirmed. No rollback or automatic replay is promised.
    /// Empty vectors succeed without I/O. Use VfsiExt::setattrs_all for strict
    /// first-error selection after the entire vector has executed.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, Permissions, SetAttrsOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.setattrs_all(&[
    ///     SetAttrsOp::new("/file-1").permissions(Permissions::from_mode(0o640)).len(1024),
    ///     SetAttrsOp::new("/file-2").len(0),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    /// Handle targets and paths can share a batch:
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, Target, SetAttrsOp};
    /// # fn example<F: Vfsi>(fs: &F, file: &F::File) -> vfsi_core::api::Result<()> {
    /// fs.setattrs_all(&[
    ///     SetAttrsOp::file(file).len(1024),
    ///     SetAttrsOp::new(Target::Path(std::path::Path::new("/other"))).len(0)
    ///         .with_follow_final_symlink(false),
    /// ])?;
    /// # Ok(())
    /// # }
    /// ```
    fn vsetattrs<P: crate::api::AsTarget<Self::File>>(
        &self,
        updates: &[SetAttrsOp<P>],
    ) -> crate::VecResult<()>;

    /// Capabilities supported by this client. Routed clients report capabilities
    /// common to their routes; support does not imply authorization for a path.
    fn capabilities(&self) -> Result<crate::api::Capabilities>;

    /// Create symbolic links from `(target text, link path)` pairs.
    /// Targets are stored verbatim, including relative or dangling targets.
    /// Parents must exist. Each independent request retains its own outcome; no rollback.
    fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> crate::VecResult<()>;

    /// Read symbolic link targets in input order without following final links.
    /// Returns their original path bytes, including non-UTF-8 names on Unix.
    /// Each input retains its own result; a failed input does not discard siblings.
    fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> crate::VecResult<std::path::PathBuf>;

    /// Create hard links from `(existing source, new link path)` pairs.
    /// Sources are not followed when the final component is a symlink.
    /// Cross-filesystem links can fail independently; successful siblings remain completed.
    fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> crate::VecResult<()>;

    /// Owned directory handle for this client's retained namespace objects.
    type Dir: DirHandle + 'static;

    /// Open genuine directory handles in input order without following final
    /// symlinks. Path-only backends report unsupported. Successful owned handles
    /// remain available across sibling failures. Empty vectors perform no I/O.
    /// Use VfsiExt::open_dirs for strict collection with sibling cleanup.
    fn vopen_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> crate::VecResult<Self::Dir> {
        if paths.is_empty() {
            Ok(Vec::new())
        } else {
            Err(crate::api::Error::unsupported(None))
        }
    }

    /// Remove contents anchored to retained directories, preserving the roots.
    /// Validate each handle before use; invalid handles do not suppress valid
    /// independent siblings. Never reopen diagnostic paths. Each root retains
    /// its own outcome and borrowed cleanup owner. Errors can follow partial
    /// mutation; neither failures nor unknown effects permit replay.
    fn vremove_dir_contents(
        &self,
        dirs: &[&Self::Dir],
        options: crate::RemoveOptions,
    ) -> crate::VecResult<()> {
        let _ = options;
        if dirs.is_empty() {
            Ok(Vec::new())
        } else {
            Err(crate::api::Error::unsupported(None))
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
    /// println!("owned-read budget: {} bytes", limits.max_read_bytes());
    /// println!("walk depth: {}", limits.max_walk_depth().get());
    /// # }
    /// ```
    fn limits(&self) -> ResourceLimits;
    /// Return one independently owned handle or error per input, in input order.
    /// Successful siblings remain usable when another input fails; the caller
    /// owns their cleanup. Failed opens may have created or truncated a file
    /// before a later step failed. No rollback or execution order is promised.
    /// Singleton requests use the same engine and preserve symlink semantics.
    /// Use VfsiExt::open_batch when all handles must be returned together.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, FileHandle, OpenFlags, OpenOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let outcomes = fs.vopen(&[
    ///     OpenOp::new("/file-1", OpenFlags::READ),
    ///     OpenOp::new("/file-2", OpenFlags::READ),
    /// ])?;
    /// for outcome in outcomes {
    ///     match outcome {
    ///         Ok(file) => file.close()?,
    ///         Err(error) => eprintln!("open failed: {:?}", error.state()),
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn vopen(&self, requests: &[OpenOp]) -> crate::VecResult<Self::File>;
    /// Read independent whole-file, allocating-range and caller-buffer inputs.
    ///
    /// An outer error rejects the batch before input dispatch. Otherwise one
    /// outcome corresponds to each input, in input order. A failed input does
    /// not suppress independent siblings; result order does not promise execution
    /// order. Implementations may group read forms into batched phases.
    ///
    /// Successful whole-file reads reach EOF; ranges may return short progress.
    /// Read failures can retain a confirmed prefix in [`crate::IoProgress::Read`].
    /// For caller storage, only that prefix or a successful result's byte count
    /// identifies valid bytes. Failure state distinguishes definite failure,
    /// uncertainty and inputs that were not attempted. Outcome position supplies
    /// identity; an inner cause has no legacy input index.
    ///
    /// All requested range/buffer lengths must fit the aggregate logical-byte
    /// budget before dispatch. Whole files share the remaining returned-data
    /// budget and fail rather than truncate. Options override
    /// [`limits`](Self::limits); large files should usually be streamed.
    /// No snapshot or atomicity is promised. Results never borrow caller storage.
    ///
    /// Use [`VfsiExt::read_batch`](super::VfsiExt::read_batch) for an explicitly
    /// lossy all-success collection after the vector has finished.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, ReadOp, ReadOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let outcomes = fs.vread(
    ///     [ReadOp::whole("/config"), ReadOp::whole("/optional")],
    ///     ReadOptions::new(),
    /// )?;
    /// for (index, outcome) in outcomes.iter().enumerate() {
    ///     match outcome {
    ///         Ok(read) => println!("input {index}: {} bytes", read.bytes_read()),
    ///         Err(error) => eprintln!("input {index}: {error}"),
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, ReadOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let file = fs.open("/large")?;
    /// let mut buffer = [0_u8; 4096];
    /// let result = fs.vread([
    ///     ReadOp::whole("/config"),
    ///     ReadOp::range(&file, 1024, 4096),
    ///     ReadOp::into(&file, 8192, &mut buffer),
    /// ], Default::default());
    /// let close = file.close();
    /// let outcomes = result?;
    /// close?;
    /// if let Ok(progress) = &outcomes[2] {
    ///     println!("buffer: {:?}", &buffer[..progress.bytes_read()]);
    /// }
    /// for (index, outcome) in outcomes.iter().enumerate() {
    ///     if let Err(error) = outcome {
    ///         eprintln!("input {index}: {error}");
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn vread<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::api::ReadOp<'a, Self::File>>,
        options: crate::api::ReadOptions,
    ) -> crate::VecResult<crate::api::ReadResult>;
    /// Write positional ranges with explicit short-write policy.
    /// With default options, report accepted byte counts without completing short writes.
    /// With `write_all(true)`, complete each eligible input through short-write waves.
    /// Return one outcome per input, retaining healthy siblings and confirmed
    /// payload prefixes when another input fails. Input-local rejection does not
    /// suppress independent writes. An outer error rejects before dispatch.
    /// Completion orders backend-known aliases within each route cohort;
    /// uncertain predecessors leave dependent writes NotAttempted while
    /// independent inputs remain eligible. Cross-route alias ordering requires
    /// separate calls.
    /// This is not transactional or a vector-wide execution-order guarantee.
    ///
    /// Handles opened for append select EOF regardless of the requested offset.
    /// With completion enabled, success means every request's payload was written.
    /// Short writes are completed at their remaining offsets; this does not
    /// authorize replay after an ambiguous failure or guarantee durability.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, OpenFlags, OpenOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let files = fs.open_batch(&[
    ///     OpenOp::new("/file-1", OpenFlags::WRITE),
    ///     OpenOp::new("/file-2", OpenFlags::WRITE),
    /// ])?;
    /// let result = fs.write_batch(&[
    ///     WriteOp::at(&files[0], 0, b"hello"),
    ///     WriteOp::at(&files[1], 4096, b"world"),
    /// ], vfsi_core::api::WriteOptions::new().with_write_all(true));
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
    ) -> crate::VecResult<WriteResult>;
    /// Close independent handles and return one outcome per input. Confirmed
    /// successes are disarmed, including when a sibling fails. Already closed
    /// handles succeed without dispatch. A lost reply leaves remote state
    /// ambiguous, not safely open; failed handles retain cleanup ownership.
    ///
    /// Unlike [`close_files`](super::VfsiExt::close_files), this borrows the handles. After an error,
    /// `is_closed()` reports local cleanup ownership only; reconcile/close the
    /// retained handles rather than resuming ordinary I/O as if nothing happened.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, OpenFlags, OpenOp, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let mut files = fs.open_batch(&[OpenOp::new("/file-1", OpenFlags::READ)])?;
    /// if let Err(error) = fs.close_batch(&mut files) {
    ///     let retained = files.iter().filter(|file| !file.is_closed()).count();
    ///     eprintln!("{retained} handles retain cleanup ownership: {error}");
    ///     return Err(error); // Drop performs best-effort cleanup, not reconciliation.
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn vclose(&self, files: &mut [Self::File]) -> crate::VecResult<()>;
    /// Create independent directories with one outcome per input.
    /// Parents must already exist; a failed input does not suppress siblings or imply rollback.
    ///
    /// Submit independent siblings together. Do not rely on creating a parent
    /// and its child in the same vector; create parents first or use
    /// a dependency-aware tree builder for dependency-aware fresh-tree creation.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, MkDirOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.create_dir_all("/workspace")?;
    /// fs.mkdirs_all(&[MkDirOp::new("/workspace/input", 0o750), MkDirOp::new("/workspace/output", 0o700)])?;
    /// # Ok(())
    /// # }
    /// ```
    /// Each [`MkDirOp`] supplies a path and Unix permission bits; backends apply the
    /// requested mode rather than relying on the process umask. Unsupported
    /// permission semantics are reported by the backend.
    fn vmkdir<P: AsRef<Path>>(&self, directories: &[MkDirOp<P>]) -> crate::VecResult<()>;
    /// Copy independent file pairs, preserving one outcome per input.
    /// Successful siblings remain copied after an input fails. Vector position
    /// identifies the input, not completion order or a successful prefix.
    ///
    /// Pairs are `(source, destination)` in this client's namespace. Contents
    /// are copied, not a recursive tree or a metadata-preserving snapshot.
    /// `options` controls whether final source symlinks are followed; ancestor
    /// symlinks retain ordinary path-resolution behavior.
    /// Avoid aliasing sources/destinations; batching does not make overlapping
    /// copies transactional or safe to replay after ambiguous failure.
    ///
    /// ```no_run
    /// use vfsi_core::api::{CopyOptions, Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.copies_all(&[
    ///     ("/input/file-1", "/output/file-1"),
    ///     ("/input/file-2", "/output/file-2"),
    /// ], CopyOptions::default())?;
    /// # Ok(())
    /// # }
    /// ```
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: CopyOptions,
    ) -> crate::VecResult<()>;
    /// Remove entries, recursive trees, or directory contents with explicit policy.
    ///
    /// Contents mode retains each root and uses native anchored removal without
    /// following final symlinks. Failures may follow partial mutations; no mode
    /// provides rollback. Each root retains its own outcome. Ordinary failures
    /// do not suppress independent roots. Options control bounded retries and
    /// continuation within each root; fatal errors may leave later roots untouched.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, RemoveMode, RemoveOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.removes_all(&["/scratch-1", "/scratch-2"],
    ///     RemoveMode::Contents, RemoveOptions::new())?;
    /// # Ok(())
    /// # }
    /// ```
    fn vremove<P: AsRef<Path>>(
        &self,
        paths: &[P],
        mode: crate::api::RemoveMode,
        options: crate::api::RemoveOptions,
    ) -> crate::VecResult<()>;
    /// Rename independent pairs using the requested atomic destination behavior.
    /// Each pair retains its own outcome; vector position identifies an input,
    /// not completion order or a committed prefix. Successful siblings remain
    /// renamed after another input fails. Unknown effects are never replayed.
    /// Both paths use this filesystem's namespace. Unsupported atomic guarantees
    /// are rejected before mutation, never emulated with check-then-rename.
    ///
    /// ```no_run
    /// use vfsi_core::api::{RenameMode, Vfsi, VfsiExt};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.renames_all(
    ///     &[("/old-1", "/new-1"), ("/old-2", "/new-2")],
    ///     Default::default(), // ordinary replacement semantics
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: crate::api::RenameMode,
    ) -> crate::VecResult<()>;

    /// Visit shallow directories (default) or recursive trees using bounded pages.
    /// Entry/path-byte limits are shared across roots; recursive visits also
    /// charge retained directory path bytes. Root index accompanies every owned directory page, including empty directories.
    /// Callbacks run outside backend locks and may reenter. Entries for one directory may span several pages. Break stops the
    /// whole vector. Callbacks may also return [`ListDirControl`](crate::api::ListDirControl)
    /// to prune selected child directories before any descent I/O. Pruned paths
    /// must be directory entries in the current page. Existing `ControlFlow<()>`
    /// callbacks continue to work. Return exactly one outcome per root.
    /// Ordinary filesystem/callback failures affect their root while independent
    /// roots remain eligible. Failed roots may already have emitted pages; callbacks
    /// are never replayed. A callback Stop ends further delivery: unfinished and
    /// unvisited roots are Stopped, while known successes and failures are retained.
    /// Pages may interleave between roots. Outcome position is input identity,
    /// not execution order or a completed-prefix count.
    ///
    /// Sorting and enter/leave options are rejected before I/O; use VfsiExt::listdir
    /// for those policies.
    /// Metadata is selected on directory pages, not through per-entry stat
    /// calls. Entry symlinks are never followed during recursive descent.
    /// Descendants reopened by pathname reject symlinks in every component;
    /// backends unable to guarantee this return Unsupported. Input roots retain
    /// the supplied path-resolution policy.
    /// Each page owns its entries; collection can transfer them without cloning.
    /// Traversal order is backend-defined; this does not provide a snapshot.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, ListDirOptions, ControlFlow};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.listdirs_all(&["/tree-1", "/tree-2"],
    ///     ListDirOptions::new().with_recursive(true).with_max_depth(vfsi_core::api::DepthLimit::new(8).expect("finite depth")),
    ///     |index, page| {
    ///         println!("{index}: {} ({} entries)", page.path.display(), page.entries.len());
    ///         Ok(ControlFlow::Continue(()))
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    fn vlistdirs<P: AsRef<Path>, C: Into<crate::api::ListDirControl>>(
        &self,
        paths: &[P],
        options: crate::api::ListDirOptions,
        callback: impl FnMut(usize, DirectoryListing) -> Result<C>,
    ) -> crate::VecResult<crate::api::TraversalCompletion>;

    /// Read owned, shallow pages for an application-controlled directory frontier.
    ///
    /// A cohort has at most 32 paths and is split by native negotiated capacity.
    /// Pass one optional continuation/child cursor per path. Every cursor must
    /// originate from this client and name that same original path. Inputs move
    /// into the call and are released on failure. Invalid/foreign cursors are
    /// NotAttempted without blocking valid independent siblings. Ownership is
    /// checked before use. Each input retains its own page/error; successful
    /// pages keep their cursors across sibling failures. Zero-entry pages have
    /// no continuation. Structural rejection occurs before input dispatch.
    /// Options set aggregate entry/path budgets and metadata fields; recursive
    /// options are rejected. Page size is a hint, capped at 128 for native clients.
    ///
    /// Native implementations retain anchored identity across pages and child
    /// descent. The default supports path-only implementations by collecting a
    /// complete bounded listing with no cursors; it may exceed the page-size hint.
    /// Descendant path-only opens should disable symlink components. These pages
    /// do not provide snapshots or kernel/direct-client cache coherence.
    fn vlistdir_pages<P: AsRef<Path>>(
        &self,
        paths: &[P],
        cursors: Vec<Option<crate::api::DirectoryCursor>>,
        options: crate::api::ListDirOptions,
        _page_size: std::num::NonZeroUsize,
    ) -> crate::VecResult<crate::api::DirectoryPage> {
        super::paging::fallback_pages(self, paths, cursors, options)
    }

    /// Stream files through bounded chunks without collecting whole contents.
    /// The callback receives the input index, byte offset, and borrowed chunk.
    /// Return one outcome per input. Filesystem/callback errors affect their file;
    /// independent siblings remain eligible. `ControlFlow::Break(())` ends delivery
    /// for the whole vector. Unvisited files are Stopped at offset zero; known
    /// failures from submitted work remain errors. No callback is replayed.
    /// Error progress records the prefix accepted by successful callbacks in
    /// `IoProgress::Stream`; a failing callback may have additional effects.
    /// Backends may process streams sequentially; this is not a parallelism promise.
    /// Native clients preserve callback input order while prefetching at most one
    /// chunk per sibling in a bounded OPEN/READ/CLOSE cohort. Cohorts hold at most
    /// 32 files and fit the client's aggregate read budget. Oversized chunk hints
    /// are capped by that budget; a zero budget rejects nonempty calls before
    /// dispatch. Reads do not provide a snapshot.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, StreamOptions};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.streams_all(&["/large-1", "/large-2"],
    ///     StreamOptions::new().with_chunk_size(std::num::NonZeroUsize::new(1024 * 1024).unwrap()), |index, offset, data| {
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
    ) -> crate::VecResult<crate::api::StreamCompletion>;
}
