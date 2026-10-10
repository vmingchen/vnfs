use crate::api::{Attrs, CopyOption, DirectoryListing, MkDirOp, OpenOp, Result, SetAttrsOp};
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
    /// Open one genuine, retained directory through the portable vector engine.
    fn open_dir_handle(&self, path: impl AsRef<Path>) -> Result<Self::Dir> {
        single_completion(self.vopen_dirs(&[path])?, "vopen_dirs")
    }
    /// Remove a retained directory's contents without resolving its old name.
    fn remove_dir_contents_handle(&self, dir: &Self::Dir) -> Result<()> {
        self.remove_dir_contents_handle_with_options(dir, crate::RemoveOptions::default())
    }
    fn remove_dir_contents_handle_with_options(
        &self,
        dir: &Self::Dir,
        options: crate::RemoveOptions,
    ) -> Result<()> {
        self.vremove_dir_contents(&[dir], options)
    }

    /// Build reusable open options for this client.
    fn open_options(&self) -> super::OpenOptions<'_, Self> {
        super::OpenOptions::new(self)
    }

    /// Query one target through the vector filesystem-statistics engine.
    fn statfs<P: crate::AsTarget<Self::File>>(&self, target: P) -> Result<crate::FilesystemStats> {
        single_completion(self.vstatfs(&[target])?, "statfs backend")
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
    /// let result = fs.vread([vfsi_core::api::ReadOp::into(&file, 0, &mut header)], Default::default());
    /// let close = file.close();
    /// let bytes = result?[0].read();
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
        single_completion(self.vopen(&[request])?, "vopen")
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
    /// The aggregate payload is bounded by `limits().read_byte_limit()`.
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
            crate::api::StreamOptions::new().chunk_size(
                std::num::NonZeroUsize::new(self.limits().stream_chunk_size()).unwrap(),
            ),
            callback,
        )
    }

    /// Single-target convenience. For multiple files, prefer [`Vfsi::vstream`]. Backends may process streams sequentially.
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
    ///     "/large.bin", StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(1024 * 1024).unwrap()),
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

    /// Borrow a standard-I/O adapter with an independent cursor, initially zero.
    /// Reads/writes use this client's vectors; collecting reads enforce its
    /// payload budget. The adapter does not own or close the file.
    fn std_io<'a>(
        &'a self,
        file: &'a Self::File,
    ) -> impl std::io::Read + std::io::Write + std::io::Seek + 'a {
        super::io::StdIo::new(self, file)
    }

    /// Synchronize one retained handle through [`Vfsi::vfsync`].
    fn sync_data(&self, file: &Self::File) -> Result<()> {
        self.vfsync(&[file], crate::api::SyncMode::Data)
    }

    /// Synchronize one retained handle's data and metadata through [`Vfsi::vfsync`].
    fn sync_all(&self, file: &Self::File) -> Result<()> {
        self.vfsync(&[file], crate::api::SyncMode::All)
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
    fn attrs_with_options<T: crate::AsTarget<Self::File>>(
        &self,
        path: T,
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
    /// Lifecycle traversal currently retains bounded directory buffers because
    /// the vector page primitive does not expose resumable cursors.
    /// `sort_by_name(true)` also buffers a bounded directory before sorting.
    /// Unsorted entry-only visiting remains paged. This is not a snapshot.
    /// Buffered recursive/lifecycle visiting rejects symlinks in reopened
    /// directory paths, including ancestors. Backends unable to guarantee that
    /// return Unsupported instead of checking and then following the path.
    /// Ordinary shallow listing retains the backend's path resolution; use
    /// `follow_symlinks(false)` to require no-follow directory opens explicitly.
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
    ///     ListDirOptions::new().recursive(true).enter_leave(true).sort_by_name(true),
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
        options: crate::api::ListDirOptions,
        visitor: impl FnMut(&crate::api::WalkEvent) -> Result<crate::api::WalkControl>,
    ) -> Result<crate::api::TraversalCompletion> {
        super::listdir::listdir(self, root.as_ref(), options, visitor)
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

    /// Visit complete shallow listings in depth-first order with application policy.
    ///
    /// `order` compares immutable siblings (for example, locale-aware `ls` ordering).
    /// The shared workflow sorts them; callbacks cannot replace validated entries.
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
    /// fs.visit_dirs_ordered("/input",
    ///     fs.limits().walk_options().fields(Attributes::MODE | Attributes::SIZE),
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
                    .fields(options.attributes())
                    .follow_symlinks(options.follows_symlinks())
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
            entries.sort_by(&mut order);
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
        self.vmkdir(&[MkDirOp::new(path, 0o777)])
    }

    /// Truncate or extend one path or opened object using [`Vfsi::vsetattrs`].
    fn truncate<T: crate::api::AsTarget<Self::File>>(&self, target: T, len: u64) -> Result<()> {
        self.vsetattrs(&[SetAttrsOp::new(target).len(len)])
    }

    /// Change permissions on one path or opened object using [`Vfsi::vsetattrs`].
    fn chmod<T: crate::api::AsTarget<Self::File>>(
        &self,
        target: T,
        permissions: crate::api::Permissions,
    ) -> Result<()> {
        self.vsetattrs(&[SetAttrsOp::new(target).permissions(permissions)])
    }

    /// Change ownership of one path or opened object through [`Vfsi::vsetattrs`].
    /// `None` leaves the corresponding owner/group unchanged. Use the vector
    /// with [`SetAttrsOp::follow_symlinks(false)`](SetAttrsOp::follow_symlinks)
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
        self.vsetattrs(&[op])
    }

    /// Create one directory with explicit Unix permission bits.
    fn create_dir_with_mode(&self, path: impl AsRef<Path>, mode: u32) -> Result<()> {
        self.vmkdir(&[MkDirOp::new(path, mode)])
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
            match self.vmkdir(&[MkDirOp::new(&current, 0o777)]) {
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
        .vgetattrs(&[target], options)
        .map_err(|error| error.with_context(operation, path))?;
    single_completion(results, "vgetattrs").map_err(|error| error.with_context(operation, path))
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
