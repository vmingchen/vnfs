use crate::api::{
    Attrs, CopyOptions, DirectoryListing, Error, MkDirOp, OpenOp, Result, SetAttrsOp,
};
use std::path::Path;

use super::{FileHandle, Vfsi};

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
/// - Visit directories: [`listdir`](Self::listdir), [`visit_dirs_ordered`](Self::visit_dirs_ordered).
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
    /// Build reusable open options for this client.
    fn open_options(&self) -> super::OpenOptions<'_, Self> {
        super::OpenOptions::new(self)
    }

    /// Acquire all directory handles in one vector, selecting the first error
    /// in input order. On failure, successful sibling handles are dropped and
    /// their cleanup owners release or queue the native resources.
    fn open_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<Self::Dir>> {
        crate::internal::collect_vec_result(self.vopen_dirs(paths), paths.len())
    }

    /// Query one target through the vector filesystem-statistics engine.
    fn statfs<P: crate::AsTarget<Self::File>>(&self, target: P) -> Result<crate::FilesystemStats> {
        single_completion(self.statfs_all(&[target])?, "statfs backend")
    }

    /// Query all targets with one vector call, requiring every input to succeed.
    /// Selects the first error in input order after the call finishes. Other
    /// outcomes and completion knowledge are discarded; no retry is authorized.
    fn statfs_all<P: crate::AsTarget<Self::File>>(
        &self,
        targets: &[P],
    ) -> Result<Vec<crate::FilesystemStats>> {
        crate::internal::collect_vec_result(self.vstatfs(targets), targets.len())
    }

    // Open and close
    /// Single-target convenience. For multiple files, prefer [`Vfsi::vopen`] to expose batching opportunities.
    ///
    /// Open read-only. Paths are relative to this client's configured namespace.
    ///
    /// Use [`open_with`](VfsiExt::open_with) for write/create flags, or
    /// [`vopen`](Vfsi::vopen) to batch many opens. The returned handle owns
    /// lifecycle only; use vectors or [`std_io`](VfsiExt::std_io) for I/O.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, FileHandle, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let file = fs.open("/config")?;
    /// let mut header = [0_u8; 128];
    /// let result = fs.read_batch([vfsi_core::api::ReadOp::into(&file, 0, &mut header)], Default::default());
    /// let close = file.close();
    /// let bytes = result?[0].bytes_read();
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
    /// let result = fs.write_batch(&[WriteOp::at(&file, 0, b"complete contents")], vfsi_core::api::WriteOptions::new().with_write_all(true));
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
        let requests = [request];
        self.open_batch(&requests)
            .and_then(|files| single_completion(files, "vopen"))
            .map_err(|error| error.with_context("open", requests[0].path()))
    }

    /// Open all requested handles or select the first error in input order.
    /// The vector executes once. On failure, close every successful sibling in
    /// one vector and retain failed cleanup through handle Drop. This discards
    /// other errors and certainty; it does not roll back creation or truncation.
    fn open_batch(&self, requests: &[OpenOp]) -> Result<Vec<Self::File>> {
        let outcomes = self.vopen(requests).map_err(|error| {
            if error.index().is_some_and(|index| index >= requests.len()) {
                crate::VfError::invalid_reply(None, "invalid open rejection index")
            } else {
                error
            }
        })?;
        let bad_count = outcomes.len() != requests.len();
        if !bad_count && outcomes.iter().all(|result| result.is_ok()) {
            return Ok(outcomes
                .into_iter()
                .map(|result| result.expect("checked successful open"))
                .collect());
        }
        let mut files = Vec::new();
        let mut first = None;
        for (index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                Ok(file) => files.push(file),
                Err(error) if first.is_none() => first = Some(error.into_cause().with_index(index)),
                Err(_) => {}
            }
        }
        let primary = if bad_count {
            crate::VfError::invalid_reply(None, "invalid vector outcome count")
        } else {
            first.expect("failed open outcome")
        };
        if !files.is_empty() {
            let _ = self.vclose(&mut files);
        }
        Err(primary)
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
    /// let files = fs.open_batch(&[OpenOp::new("/config", OpenFlags::READ)])?;
    /// fs.close_files(files)?; // Observe a close error instead of discarding it in Drop.
    /// # Ok(())
    /// # }
    /// ```
    fn close_files(&self, mut files: Vec<Self::File>) -> Result<()> {
        self.close_batch(&mut files)
    }

    /// Close all handles, retaining cleanup ownership for unsuccessful inputs.
    /// Select the first error in input order after all outcomes are available;
    /// this deliberately discards sibling errors, not successful cleanup.
    fn close_batch(&self, files: &mut [Self::File]) -> Result<()> {
        crate::internal::collect_vec_result(self.vclose(files), files.len()).map(|_| ())
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
    /// The aggregate payload is bounded by `limits().max_read_bytes()`.
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

    /// Collect a read vector only if every input succeeds.
    ///
    /// Short reads remain short. The vector executes once; this helper then
    /// selects the first error in input order and discards sibling outcomes and
    /// confirmed error progress. It promises neither stop-on-error nor retry safety.
    fn read_batch<'a>(
        &self,
        ops: impl IntoIterator<Item = crate::api::ReadOp<'a, Self::File>>,
        options: crate::api::ReadOptions,
    ) -> Result<Vec<crate::api::ReadResult>> {
        let mut count = 0usize;
        let result = {
            let mut ops = ops.into_iter().inspect(|_| count += 1);
            let result = self.vread(&mut ops, options);
            if result.is_ok() {
                ops.for_each(drop);
            }
            result
        };
        crate::internal::collect_vec_result(result, count)
    }

    /// Whole-file convenience reads with an explicit aggregate payload budget.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, ReadOptions, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let files = fs.read_files_with_options(&["/config"],
    ///     ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(1024 * 1024).unwrap()))?;
    /// # let _ = files;
    /// # Ok(())
    /// # }
    /// ```
    fn read_files_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::ReadOptions,
    ) -> Result<Vec<Vec<u8>>> {
        let results = self.read_batch(
            paths
                .iter()
                .map(|path| crate::api::ReadOp::whole(path.as_ref())),
            options,
        )?;
        if results.len() != paths.len() {
            return Err(crate::api::Error::invalid_reply(
                None,
                "vread_native returned an invalid result count",
            ));
        }
        results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                let complete = result.offset() == 0 && result.eof();
                let data = result.into_data().ok_or_else(|| {
                    crate::api::Error::invalid_reply(
                        Some(index),
                        "whole-file vread_native omitted owned data",
                    )
                })?;
                if !complete {
                    return Err(crate::api::Error::invalid_reply(
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
    ///     .with_max_total_bytes(std::num::NonZeroUsize::new(4096).unwrap()))?;
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
            crate::api::Error::from_kind(0, std::io::ErrorKind::InvalidInput)
                .with_context("read_to_string", path)
        })
    }

    /// Single-target convenience. For multiple files, prefer [`Vfsi::vstream`].
    /// This discards per-operation completion/progress information and preserves
    /// a nested callback error's original diagnostic index. It never replays data.
    ///
    /// Stream a file using the client's bounded chunk size instead of collecting it.
    /// Return `Ok(ControlFlow::Break(()))` to stop successfully; the callback runs outside the lock.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.read_stream("/large", |offset, bytes| {
    ///     println!("{} bytes at {offset}", bytes.len());
    ///     Ok(std::ops::ControlFlow::Continue(()))
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    fn read_stream(
        &self,
        path: impl AsRef<Path>,
        callback: impl FnMut(u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::api::StreamCompletion> {
        self.read_stream_with_options(
            path,
            crate::api::StreamOptions::new().with_chunk_size(
                std::num::NonZeroUsize::new(self.limits().stream_chunk_size()).unwrap(),
            ),
            callback,
        )
    }

    /// Single-target convenience. For multiple files, prefer [`Vfsi::vstream`].
    /// This discards per-operation completion/progress information and preserves
    /// a nested callback error's original diagnostic index. It never replays data.
    ///
    /// Stream from offset zero outside the backend lock. `ControlFlow::Break(())` stops after
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
    ///     "/large.bin", StreamOptions::new().with_chunk_size(std::num::NonZeroUsize::new(1024 * 1024).unwrap()),
    ///     |_offset, chunk| {
    ///         processed += chunk.len() as u64; // Process the borrowed bytes here.
    ///         Ok(if processed < 8 * 1024 * 1024 {
    ///             std::ops::ControlFlow::Continue(())
    ///         } else {
    ///             std::ops::ControlFlow::Break(())
    ///         }) // Stop after a bounded sample.
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
        callback: impl FnMut(u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::api::StreamCompletion> {
        let mut callback = callback;
        let mut callback_error = None;
        let result = self.vstream(&[path], options, |_, offset, data| {
            let result = callback(offset, data);
            if let Err(error) = &result {
                callback_error = Some(error.clone());
            }
            result
        });
        single_completion(
            crate::internal::collect_vec_result(result, 1)
                .map_err(|error| callback_error.unwrap_or(error))?,
            "read_streams",
        )
    }

    /// Stream one vector and select the first failed input by position.
    /// All eligible files have executed when the call returns. This deliberately
    /// discards sibling outcomes and delivery progress, adds no retry/prefix
    /// guarantee, and never replays a callback. Explicit Stop ends delivery.
    fn streams_all<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::StreamOptions,
        callback: impl FnMut(usize, u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::api::StreamCompletion>> {
        crate::internal::collect_vec_result(self.vstream(paths, options, callback), paths.len())
    }

    /// Invoke one WRITE vector and require every input to succeed.
    ///
    /// Error selection follows input position, not execution order. This
    /// deliberately discards sibling results and byte progress; it never
    /// implies that later inputs were untouched or that retry is safe.
    fn write_batch<'a>(
        &self,
        requests: &[crate::api::WriteOp<'a, Self::File>],
        options: crate::api::WriteOptions,
    ) -> Result<Vec<crate::api::WriteResult>> {
        crate::internal::collect_vec_result(self.vwrite(requests, options), requests.len())
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
        let result = self.write_batch(
            &[crate::api::WriteOp::at(&file, 0, data)],
            crate::api::WriteOptions::new().with_write_all(true),
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
                return Err(
                    crate::api::Error::from_kind(index, std::io::ErrorKind::InvalidInput)
                        .with_context("write_files", path.as_ref()),
                );
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
        let files = self.open_batch(&requests)?;
        if files.len() != entries.len() {
            return Err(crate::api::Error::invalid_reply(
                None,
                "vopen returned an invalid result count",
            ));
        }
        let writes: Vec<_> = files
            .iter()
            .zip(entries)
            .map(|(file, (_, data))| crate::api::WriteOp::at(file, 0, data.as_ref()))
            .collect();
        let result = self.write_batch(
            &writes,
            crate::api::WriteOptions::new().with_write_all(true),
        );
        drop(writes);
        let close_result = self.close_files(files);
        result?;
        close_result
    }

    /// Borrow a standard-I/O adapter with an independent cursor, initially zero.
    /// Reads/writes use this client's vectors; collecting reads enforce its
    /// payload budget. The adapter does not own or close the file.
    /// A failed read with confirmed bytes is exposed as a short successful
    /// standard-I/O read; only those bytes advance the adapter's cursor.
    /// Use Vfsi::vread to retain the accompanying error and completion state.
    /// A write's acknowledged prefix is returned as a short success. Its
    /// accompanying failure is saved and reported before further I/O, on flush,
    /// or on seek, retaining OpError as the I/O error's inner source. Nonempty
    /// write_all also reports a failure following a fully acknowledged payload.
    /// The cursor is then unresolved: progress does not contain an actual append
    /// offset. After observing the error, an explicit absolute/end-based seek
    /// re-establishes it. Whole-call rejections retain VfError. Uncertain writes
    /// and errors after acknowledged bytes are never retryable Interrupted.
    fn std_io<'a>(
        &'a self,
        file: &'a Self::File,
    ) -> impl std::io::Read + std::io::Write + std::io::Seek + 'a {
        super::io::StdIo::new(self, file)
    }

    /// Execute one synchronization vector and require every input to succeed.
    /// Selects the first failed input after execution, discarding other outcomes;
    /// it implies neither ordered execution nor safe retry.
    fn sync_batch(&self, files: &[&Self::File], mode: crate::api::SyncMode) -> Result<()> {
        crate::internal::collect_vec_result(self.vfsync(files, mode), files.len()).map(|_| ())
    }

    /// Synchronize one retained handle through [`Vfsi::vfsync`].
    fn sync_data(&self, file: &Self::File) -> Result<()> {
        self.sync_batch(&[file], crate::api::SyncMode::Data)
    }

    /// Synchronize one retained handle's data and metadata through [`Vfsi::vfsync`].
    fn sync_all(&self, file: &Self::File) -> Result<()> {
        self.sync_batch(&[file], crate::api::SyncMode::All)
    }

    /// Query all inputs once, collecting values only if every input succeeds.
    /// Errors are selected in input order after the vector call finishes.
    /// Discards other outcomes and completion information; never authorizes replay.
    fn getattrs_all<P: crate::AsTarget<Self::File>>(
        &self,
        targets: &[P],
        options: crate::api::AttrsOptions,
    ) -> Result<Vec<Attrs>> {
        crate::internal::collect_vec_result(self.vgetattrs(targets, options), targets.len())
    }

    // Attrs
    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vgetattrs`].
    ///
    /// Query a path following its final symlink; unavailable fields remain None.
    ///
    /// To inspect the symlink itself use [`symlink_attrs`](VfsiExt::symlink_attrs).
    /// To query an opened object after rename, pass [`crate::api::Target::file`].
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let attrs = fs.attrs("/file-1")?;
    /// println!("{:?} bytes; directory={}", attrs.len(), attrs.is_dir());
    /// # Ok(())
    /// # }
    /// ```
    fn attrs<T: crate::AsTarget<Self::File>>(&self, path: T) -> Result<Attrs> {
        attrs_query(self, path, crate::api::AttrsOptions::new(), "attrs")
    }

    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vgetattrs`] with selected fields and follow_final_symlink(false).
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
    ///         .with_fields(Attributes::MODE | Attributes::BLOCKS).with_follow_final_symlink(false),
    /// )?;
    /// if let Some(blocks) = attrs.blocks() {
    ///     println!("allocated blocks: {blocks}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn attrs_with_options<T: crate::AsTarget<Self::File>>(
        &self,
        path: T,
        options: crate::api::AttrsOptions,
    ) -> Result<Attrs> {
        attrs_query(self, path, options, "attrs_with_options")
    }

    /// Single-target convenience. For multiple paths, prefer [`Vfsi::vgetattrs`] with follow_final_symlink(false).
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
            crate::api::AttrsOptions::new().with_follow_final_symlink(false),
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
        self.read_dir_with_options(path, crate::api::ListDirOptions::new())
    }

    /// Single-target convenience. For multiple directories, prefer [`VfsiExt::read_dirs_with_options`] with one aggregate budget.
    ///
    /// Collect one shallow directory with selected metadata and entry/path-byte limits.
    /// Recursive options are rejected; use `read_dirs_with_options` with
    /// `ListDirOptions::with_recursive(true)` to collect a tree.
    ///
    /// ```no_run
    /// use vfsi_core::api::{Vfsi, VfsiExt, WriteOp};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// let entries = fs.read_dir_with_options("/input",
    ///     vfsi_core::api::ListDirOptions::new().with_max_entries(100))?;
    /// println!("{} entries", entries.len());
    /// # Ok(())
    /// # }
    /// ```
    fn read_dir_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::ListDirOptions,
    ) -> Result<Vec<crate::api::DirEntry>> {
        if options.recursive() {
            return Err(
                crate::api::Error::from_kind(0, std::io::ErrorKind::InvalidInput).with_context(
                    "read_dir_with_options requires shallow options",
                    path.as_ref(),
                ),
            );
        }
        let mut trees = self.read_dirs_with_options(&[path], options)?;
        Ok(single_completion(single_tree(&mut trees)?, "read_dirs")?.entries)
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
    ///         println!("{}: {:?} bytes", entry.path().display(), entry.attrs().len());
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    fn read_dirs<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<DirectoryListing>> {
        let trees = self.read_dirs_with_options(paths, crate::api::ListDirOptions::new())?;
        if trees.len() != paths.len() || trees.iter().any(|tree| tree.len() != 1) {
            return Err(crate::api::Error::invalid_reply(
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
    ///     ListDirOptions::new().with_recursive(true).with_fields(Attributes::MODE)
    ///         .with_max_entries(10_000).with_max_path_bytes(1024 * 1024))?;
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
        self.listdirs_all(paths, options, |index, page| {
            let tree = trees.get_mut(index).ok_or_else(|| {
                crate::api::Error::invalid_reply(None, "invalid visitor root index")
            })?;
            let positions = &mut positions[index];
            if let Some(&slot) = positions.get(&page.path) {
                tree[slot].entries.extend(page.entries);
            } else {
                positions.insert(page.path.clone(), tree.len());
                // Keep the first page's path and entry storage; later pages
                // need only a borrowed lookup to find this directory.
                tree.push(page);
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })?;
        Ok(trees)
    }

    /// Visit one vector, retaining callback order and selecting the first
    /// failed input after execution. This discards other outcomes and adds no
    /// retry or completed-prefix guarantee.
    fn listdirs_all<P: AsRef<Path>, C: Into<crate::api::ListDirControl>>(
        &self,
        paths: &[P],
        options: crate::api::ListDirOptions,
        callback: impl FnMut(usize, DirectoryListing) -> Result<C>,
    ) -> Result<Vec<crate::api::TraversalCompletion>> {
        crate::internal::collect_vec_result(self.vlistdirs(paths, options, callback), paths.len())
    }

    /// Fetch one page vector and require every input to succeed. Successful
    /// sibling pages/cursors are dropped on failure; the call is never repeated.
    fn dir_pages_all<P: AsRef<Path>>(
        &self,
        paths: &[P],
        cursors: Vec<Option<crate::api::DirectoryCursor>>,
        options: crate::api::ListDirOptions,
        page_size: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::api::DirectoryPage>> {
        crate::internal::collect_vec_result(
            self.vlistdir_pages(paths, cursors, options, page_size),
            paths.len(),
        )
    }

    // Visit directories
    /// List one directory through callbacks, optionally recursively.
    /// For independent roots and owned pages, prefer [`Vfsi::vlistdirs`].
    ///
    /// By default, only child `Entry` events are delivered, with no extra root
    /// attribute request or full-listing allocation on paging backends. Set
    /// `recursive(true)` to descend without following entry symlinks. Entry-only
    /// traversal retains native anchored child cursors and sibling batching;
    /// its order is backend-defined.
    ///
    /// `enter_leave(true)` includes the root and emits depth-first `Enter` and
    /// `Leave` events for traversed directories, including pruned directories.
    /// In shallow lifecycle mode, children are `Entry` events. Symlinks are
    /// always `Entry` events. A non-directory lifecycle root produces one Entry.
    /// Lifecycle traversal retains bounded directory buffers and native cursors,
    /// preserving observed parent identity without reopening ancestor paths.
    /// `sort_by_name(true)` also buffers a bounded directory before sorting.
    /// Unsorted entry-only visiting remains paged. This is not a snapshot.
    /// Native recursive/lifecycle visiting retains anchored directory identity.
    /// Reopened paths reject symlinks, including ancestors. Backends unable to guarantee that
    /// return Unsupported instead of checking and then following the path.
    /// Ordinary shallow listing retains the backend's path resolution; use
    /// `allow_symlink_components(false)` to require no-follow directory opens explicitly.
    ///
    /// The root is depth zero and its children have depth one. Directory depth
    /// limits control descent; shallow mode ignores them. Lifecycle mode charges
    /// the root against the entry/path budgets; recursive entry-only mode charges
    /// retained directory paths. All unspecified budgets inherit client limits.
    ///
    /// `SkipSubtree` on Enter prevents reading that directory's children;
    /// it does nothing on files or Leave. Pruning a recursive directory Entry
    /// without lifecycle events is rejected: enable `enter_leave(true)`.
    /// `Stop` returns Stopped immediately,
    /// without synthesizing pending Leave events. Callback errors propagate
    /// without replay. Callbacks run outside backend locks and may reenter.
    ///
    /// ```no_run
    /// use vfsi_core::api::{ListDirOptions, Vfsi, VfsiExt, WalkControl};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.listdir("/input", ListDirOptions::new(), |event| {
    ///     println!("{}", event.entry.path().display());
    ///     Ok(WalkControl::Continue)
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Recursive lifecycle events allow pruning before listing children:
    ///
    /// ```no_run
    /// use vfsi_core::api::{ListDirOptions, Vfsi, VfsiExt, WalkControl, WalkEventKind};
    /// # fn example(fs: &impl Vfsi) -> vfsi_core::api::Result<()> {
    /// fs.listdir("/project",
    ///     vfsi_core::api::WalkOptions::new(ListDirOptions::new().with_recursive(true))
    ///         .with_enter_leave(true).with_sort_by_name(true),
    ///     |event| {
    ///         if event.kind == WalkEventKind::Enter
    ///             && event.entry.file_name() == Some(std::ffi::OsStr::new(".git")) {
    ///             return Ok(WalkControl::SkipSubtree);
    ///         }
    ///         println!("{:?}: {}", event.kind, event.entry.path().display());
    ///         Ok(WalkControl::Continue)
    ///     })?;
    /// # Ok(())
    /// # }
    /// ```
    fn listdir(
        &self,
        root: impl AsRef<Path>,
        options: impl Into<crate::api::WalkOptions>,
        visitor: impl FnMut(&crate::api::WalkEvent) -> Result<crate::api::WalkControl>,
    ) -> Result<crate::api::TraversalCompletion> {
        super::listdir::listdir(self, root.as_ref(), options.into(), visitor)
    }

    /// Traverse with explicit recovery for failed directory listings.
    ///
    /// The error callback receives the directory path, root-relative depth, and
    /// its error. Permission-denied, not-found and not-a-directory listing errors
    /// may be reported and skipped with Continue, or stop with Break. The caller
    /// records any desired failure exit status. Continued failures still produce
    /// the directory's Leave event in lifecycle mode. Events are never replayed.
    ///
    /// Root metadata failures, transport/invalid replies, quota failures, and
    /// visitor errors remain terminal. The default listdir method still fails on
    /// the first error. This workflow buffers bounded directories for recovery.
    fn listdir_with_errors(
        &self,
        root: impl AsRef<Path>,
        options: impl Into<crate::api::WalkOptions>,
        visitor: impl FnMut(&crate::api::WalkEvent) -> Result<crate::api::WalkControl>,
        on_error: impl FnMut(&Path, usize, Error) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<crate::api::TraversalCompletion> {
        super::listdir::listdir_with_errors(self, root.as_ref(), options.into(), visitor, on_error)
    }

    /// Visit individual entries using the directory-page primitive.
    /// Empty directories produce pages but no entry callbacks.
    fn visit_entries_with_options<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: crate::api::ListDirOptions,
        mut callback: impl FnMut(usize, &crate::api::DirEntry) -> Result<std::ops::ControlFlow<()>>,
    ) -> Result<Vec<crate::api::TraversalCompletion>> {
        self.listdirs_all(paths, options, |index, page| {
            for entry in &page.entries {
                if callback(index, entry)?.is_break() {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
    }

    /// Visit complete shallow listings in depth-first order with application policy.
    ///
    /// `order` compares immutable siblings (for example, locale-aware `ls` ordering).
    /// The shared workflow sorts them; callbacks cannot replace validated entries.
    /// `descend` selects directories before their contents are read; it does not
    /// filter entries delivered in their parent's listing. Symlinks are never
    /// traversed. Native child cursors retain their observed parent identity;
    /// path-only descendant opens reject symlinks in every component. Backends
    /// unable to guarantee that return Unsupported. The root uses the supplied
    /// path-resolution policy. `visitor` runs outside backend locks and may stop immediately
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
    /// fs.visit_dirs_ordered("/input",
    ///     fs.limits().walk_options().with_fields(Attributes::MODE | Attributes::SIZE),
    ///     |a, b| a.path().cmp(b.path()),
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
        options: crate::api::ListDirOptions,
        mut order: impl FnMut(&crate::api::DirEntry, &crate::api::DirEntry) -> std::cmp::Ordering,
        mut descend: impl FnMut(&crate::api::DirEntry) -> bool,
        mut visitor: impl FnMut(DirectoryListing, usize) -> Result<crate::api::WalkControl>,
    ) -> Result<crate::api::TraversalCompletion> {
        let options = options.walk_options(self.limits());
        let root = root.as_ref();
        let mut budget = crate::internal::TraversalBudget::new(
            options.max_entries_or(crate::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
            options.max_path_bytes_or(crate::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
        );
        budget.charge(root)?;
        let mut pending = vec![(root.to_path_buf(), 0usize, None)];
        while let Some((path, depth, cursor)) = pending.pop() {
            let (mut entries, seeds) = super::paging::collect_directory(
                self,
                &path,
                cursor,
                budget.remaining_options(
                    crate::api::ListDirOptions::new()
                        .with_fields(options.fields())
                        // Native child seeds resolve relative to the observed
                        // parent; path-only fallback must reject swapped symlinks.
                        .with_allow_symlink_components(
                            depth == 0 && options.allow_symlink_components(),
                        ),
                ),
            )?;
            for entry in &entries {
                budget
                    .charge(entry.path())
                    .map_err(|error| error.with_context("visit_dirs_ordered", entry.path()))?;
            }
            entries.sort_by(&mut order);
            let listing = DirectoryListing { path, entries };
            let mut seeds: std::collections::HashMap<_, _> = seeds
                .into_iter()
                .map(|cursor| (cursor.path().to_path_buf(), cursor))
                .collect();
            let children: Vec<_> = listing
                .entries
                .iter()
                .filter(|entry| entry.attrs().is_dir() && descend(entry))
                .map(|entry| (entry.path().to_path_buf(), seeds.remove(entry.path())))
                .collect();
            match visitor(listing, depth)? {
                crate::api::WalkControl::Stop => {
                    return Ok(crate::api::TraversalCompletion::Stopped);
                }
                crate::api::WalkControl::SkipSubtree => continue,
                crate::api::WalkControl::Continue => {}
            }
            if depth >= options.max_depth_or(Default::default()).get() && !children.is_empty() {
                if !options.truncate_at_max_depth() {
                    return Err(crate::api::Error::errno(0, libc::EFBIG as u32)
                        .with_context("visit_dirs_ordered", &children[0].0));
                }
            } else {
                pending.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|(child, cursor)| (child, depth + 1, cursor)),
                );
            }
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
        self.mkdirs_all(&[MkDirOp::new(path, 0o777)])
    }

    /// Create every directory or select the first error by input order.
    /// Calls vmkdir once; other outcomes and completion details are discarded.
    /// Later directories may have been created. There is no retry or rollback.
    fn mkdirs_all<P: AsRef<Path>>(&self, directories: &[MkDirOp<P>]) -> Result<()> {
        crate::internal::collect_vec_result(self.vmkdir(directories), directories.len()).map(|_| ())
    }

    /// Apply one attribute vector and select the first failing input by position.
    /// Calls vsetattrs once. Other outcomes are discarded; this is not a
    /// committed-prefix or retry guarantee.
    fn setattrs_all<P: crate::api::AsTarget<Self::File>>(
        &self,
        updates: &[SetAttrsOp<P>],
    ) -> Result<()> {
        crate::internal::collect_vec_result(self.vsetattrs(updates), updates.len()).map(|_| ())
    }

    /// Truncate or extend one path or opened object using [`Vfsi::vsetattrs`].
    fn truncate<T: crate::api::AsTarget<Self::File>>(&self, target: T, len: u64) -> Result<()> {
        self.setattrs_all(&[SetAttrsOp::new(target).len(len)])
    }

    /// Change permissions on one path or opened object using [`Vfsi::vsetattrs`].
    fn chmod<T: crate::api::AsTarget<Self::File>>(
        &self,
        target: T,
        permissions: crate::api::Permissions,
    ) -> Result<()> {
        self.setattrs_all(&[SetAttrsOp::new(target).permissions(permissions)])
    }

    /// Change ownership of one path or opened object through [`Vfsi::vsetattrs`].
    /// `None` leaves the corresponding owner/group unchanged. Use the vector
    /// with [`SetAttrsOp::with_follow_final_symlink(false)`](SetAttrsOp::with_follow_final_symlink)
    /// to change a symlink itself.
    fn chown<T: crate::api::AsTarget<Self::File>>(
        &self,
        target: T,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<()> {
        let mut op = SetAttrsOp::new(target);
        if let Some(uid) = uid {
            op = op.uid(uid);
        }
        if let Some(gid) = gid {
            op = op.gid(gid);
        }
        self.setattrs_all(&[op])
    }

    /// Create one directory with explicit Unix permission bits.
    fn create_dir_with_mode(&self, path: impl AsRef<Path>, mode: u32) -> Result<()> {
        self.mkdirs_all(&[MkDirOp::new(path, mode)])
    }

    /// Create all symbolic links or select the first error by input order.
    /// Calls the vector once and discards other outcomes and completion details.
    /// Later requests may have succeeded; there is no retry or rollback.
    fn symlinks_all<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
        crate::internal::collect_vec_result(self.vsymlink(pairs), pairs.len()).map(|_| ())
    }

    /// Create all hard links or select the first error by input order.
    /// Calls the vector once and discards other outcomes and completion details.
    /// Later requests may have succeeded; there is no retry or rollback.
    fn hardlinks_all<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
        crate::internal::collect_vec_result(self.vhardlink(pairs), pairs.len()).map(|_| ())
    }

    /// Create one symbolic link; submit multiple pairs with [`Vfsi::vsymlink`].
    fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
        self.symlinks_all(&[(target, link)])
    }

    /// Read every target or select the first error by input order.
    /// Calls the vector once; other outcomes and completion details are discarded.
    /// An error does not mean execution stopped at that input.
    fn readlinks_all<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<std::path::PathBuf>> {
        crate::internal::collect_vec_result(self.vreadlink(paths), paths.len())
    }

    /// Read one symlink target; submit multiple paths with [`Vfsi::vreadlink`].
    fn read_link(&self, path: impl AsRef<Path>) -> Result<std::path::PathBuf> {
        let mut targets = self.readlinks_all(&[path])?;
        Ok(targets.pop().expect("validated readlink count"))
    }

    /// Create one hard link; submit multiple pairs with [`Vfsi::vhardlink`].
    fn hard_link(&self, source: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
        self.hardlinks_all(&[(source, link)])
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
                    return Err(crate::api::Error::from_kind(
                        0,
                        std::io::ErrorKind::InvalidInput,
                    ));
                }
            }
            match self.mkdirs_all(&[MkDirOp::new(&current, 0o777)]) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !self.attrs(&current)?.is_dir() {
                        return Err(crate::api::Error::errno(0, crate::ERR_NOTDIR)
                            .with_context("create_dir_all", &current));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Rename one vector and deliberately select the first failing input.
    /// Other outcomes are discarded. This provides no prefix or retry guarantee.
    fn renames_all<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: crate::api::RenameMode,
    ) -> Result<()> {
        crate::internal::collect_vec_result(self.vrename(pairs, options), pairs.len()).map(|_| ())
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
        self.renames_all(&[(source, destination)], crate::api::RenameMode::Replace)
    }

    /// Copy one vector and select the first failing input, discarding other
    /// outcomes. This does not provide a prefix or retry guarantee.
    fn copies_all<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        options: CopyOptions,
    ) -> Result<()> {
        crate::internal::collect_vec_result(self.vcopy(pairs, options), pairs.len()).map(|_| ())
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
        self.copy_with_options(source, destination, CopyOptions::default())
    }

    /// Single-target copy with explicit operation semantics; prefer [`Vfsi::vcopy`]
    /// when submitting multiple pairs.
    fn copy_with_options(
        &self,
        source: impl AsRef<Path>,
        destination: impl AsRef<Path>,
        options: CopyOptions,
    ) -> Result<()> {
        self.copies_all(&[(source, destination)], options)
    }

    /// Remove one vector and deliberately select its first failing input.
    /// Other outcomes are discarded; this adds no prefix, rollback or retry guarantee.
    fn removes_all<P: AsRef<Path>>(
        &self,
        paths: &[P],
        mode: crate::api::RemoveMode,
        options: crate::RemoveOptions,
    ) -> Result<()> {
        crate::internal::collect_vec_result(self.vremove(paths, mode, options), paths.len())
            .map(|_| ())
    }

    /// Strict collection for contents removal through retained directory handles.
    /// Successful roots remain emptied after another input fails.
    fn remove_contents_all(
        &self,
        dirs: &[&Self::Dir],
        options: crate::RemoveOptions,
    ) -> Result<()> {
        crate::internal::collect_vec_result(self.vremove_dir_contents(dirs, options), dirs.len())
            .map(|_| ())
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
                crate::api::Error::errno(0, crate::ERR_ISDIR).with_context("remove_file", path)
            );
        }
        self.removes_all(
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
        self.removes_all(
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
        self.removes_all(
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
        self.removes_all(&[path], crate::api::RemoveMode::Tree, options)
    }

    /// Single-target convenience. For multiple directory roots, prefer [`Vfsi::vremove`].
    ///
    /// Recursively empty a directory while keeping its root.
    ///
    /// The root must be a directory, not a symlink. Child symlinks are removed
    /// without following their targets. Errors can leave partially removed
    /// contents; retaining the root does not make the operation transactional.
    /// The path is resolved for this call, not pinned across calls or renames.
    /// Use trusted ancestors; this is not a path-confinement guarantee.
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
    /// Like [`Self::remove_dir_contents`], this resolves the current pathname;
    /// it does not retain a previously opened directory's identity.
    fn remove_dir_contents_with_options(
        &self,
        path: impl AsRef<Path>,
        options: crate::api::RemoveOptions,
    ) -> Result<()> {
        let path = path.as_ref();
        require_directory(self, path, "remove_dir_contents")?;
        self.removes_all(&[path], crate::api::RemoveMode::Contents, options)
    }
}

impl<C: Vfsi + ?Sized> VfsiExt for C {}

fn single_completion<T>(mut values: Vec<T>, operation: &'static str) -> Result<T> {
    if values.len() != 1 {
        return Err(crate::api::Error::invalid_reply(
            None,
            format!("{operation} returned an invalid result count"),
        ));
    }
    Ok(values.remove(0))
}
fn require_directory<C: Vfsi + ?Sized>(fs: &C, path: &Path, operation: &'static str) -> Result<()> {
    if !fs.symlink_attrs(path)?.is_dir() {
        return Err(crate::api::Error::errno(0, crate::ERR_NOTDIR).with_context(operation, path));
    }
    Ok(())
}
fn attrs_query<C: Vfsi + ?Sized>(
    client: &C,
    target: impl crate::AsTarget<C::File>,
    options: crate::api::AttrsOptions,
    operation: &'static str,
) -> Result<Attrs> {
    let target = target.as_target();
    let path = match target {
        crate::Target::Path(path) => path,
        crate::Target::File(file) => file.path(),
    };
    let results = client
        .getattrs_all(&[target], options)
        .map_err(|error| error.with_context(operation, path))?;
    single_completion(results, "vgetattrs").map_err(|error| error.with_context(operation, path))
}

fn single_tree(trees: &mut Vec<Vec<DirectoryListing>>) -> Result<Vec<DirectoryListing>> {
    if trees.len() != 1 {
        return Err(crate::api::Error::invalid_reply(
            None,
            "read_dirs returned an invalid root count",
        ));
    }
    Ok(trees.remove(0))
}
