#![cfg(all(feature = "auto", target_os = "linux"))]

use std::{
    cell::{Cell, RefCell},
    ops::ControlFlow,
    path::Path,
};
use vnfs::{directory::*, error::*, files::*, helpers::*, posix::*};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    StreamOffset(u64),
    ShortRead,
    BadRead,
    BadWrite,
    WrongOffset,
    LostWrite,
    Close,
    CrossDevice,
    Rename,
    MiddleReadFailure,
    PartialCrossDevice,
    NonprefixCrossDevice,
    UnknownCrossDevice,
    ShortRename,
    LongRename,
    RenameProgress,
    RejectedRename,
    LostRename,
    MissingSize,
    OversizedSource,
    OwnedShortRead,
    NoReadProgress,
    OversizedOwnedRead,
    LostRead,
    ExtraReadOutcome,
    ReadAfter(usize, usize),
    ReadAfterOpen(usize, usize),
    BadOpenCount,
    ExtraOpenOutcome,
    UnknownExclusiveOpen,
    RejectedExclusiveOpen,
    ExtraCloseOutcome,
    DirectoryBackendFirst,
    DirectoryCallbacksReverse,
    DirectoryCallbacksForward,
    DestinationDenied,
    ShortDestinationMetadata,
    ExtraDestinationMetadata,
}
#[derive(Default)]
struct Calls {
    read_attempts: usize,
    directory_waves: Vec<Vec<std::path::PathBuf>>,
    stream_calls: usize,
    metadata: Vec<usize>,
    identity_pairs: usize,
    readlinks: usize,
    destination_probes: Vec<Vec<std::path::PathBuf>>,
    syncs: Vec<(SyncMode, usize)>,
    attrs: Vec<usize>,
    opens: usize,
    renames: usize,
    reads: usize,
    writes: usize,
    closes: usize,
    copies: usize,
    removes: usize,
    max_bytes: usize,
    max_files: usize,
    open_batches: Vec<usize>,
    read_batches: Vec<Vec<(u64, usize)>>,
}
struct Harness {
    fs: Posix,
    read_failure: Cell<Option<(usize, u32)>>,
    calls: RefCell<Calls>,
    fault: Cell<Fault>,
}
impl Harness {
    fn new(path: &Path) -> Self {
        Self {
            fs: Posix::new(path)
                .unwrap()
                .with_limits(ResourceLimits::new().with_max_read_bytes(64)),
            read_failure: Cell::new(None),
            calls: RefCell::new(Calls::default()),
            fault: Cell::new(Fault::None),
        }
    }
}
impl Vfsi for Harness {
    type File = <Posix as Vfsi>::File;
    type Dir = <Posix as Vfsi>::Dir;
    fn capabilities(&self) -> Result<Capabilities> {
        self.fs.capabilities()
    }
    fn vsetattrs<P: AsTarget<Self::File>>(
        &self,
        targets: &[SetAttrsOp<P>],
    ) -> vfsi_core::VecResult<()> {
        assert!(
            targets
                .iter()
                .all(|op| matches!(op.target().as_target(), Target::File(_)))
        );
        self.calls.borrow_mut().attrs.push(targets.len());
        self.fs.vsetattrs(targets)
    }
    fn vfsync(&self, files: &[&Self::File], mode: SyncMode) -> vnfs::VecResult<()> {
        self.calls.borrow_mut().syncs.push((mode, files.len()));
        self.fs.vfsync(files, mode)
    }
    fn vstatfs<P: AsTarget<Self::File>>(
        &self,
        targets: &[P],
    ) -> vfsi_core::VecResult<FilesystemStats> {
        self.fs.vstatfs(targets)
    }
    fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> vfsi_core::VecResult<()> {
        self.fs.vsymlink(pairs)
    }
    fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> vfsi_core::VecResult<std::path::PathBuf> {
        self.calls.borrow_mut().readlinks += paths.len();
        self.fs.vreadlink(paths)
    }
    fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
    ) -> vfsi_core::VecResult<()> {
        self.fs.vhardlink(pairs)
    }
    fn limits(&self) -> ResourceLimits {
        self.fs.limits()
    }

    fn compare_routed_objects(
        &self,
        pairs: &[(
            vfsi_core::internal::ObservedObject<'_>,
            vfsi_core::internal::ObservedObject<'_>,
        )],
    ) -> Vec<Result<Option<bool>>> {
        self.calls.borrow_mut().identity_pairs += pairs.len();
        self.fs.compare_routed_objects(pairs)
    }

    fn vgetattrs<P: AsTarget<Self::File>>(
        &self,
        p: &[P],
        o: AttrsOptions,
    ) -> vfsi_core::VecResult<Attrs> {
        self.calls.borrow_mut().metadata.push(p.len());
        if self.fault.get() == Fault::MissingSize {
            return Ok(p
                .iter()
                .map(|_| {
                    vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs {
                        returned: Attributes::MODE,
                        ftype: FileType::Regular,
                        mode: libc::S_IFREG | 0o644,
                        size: 100,
                        ..Default::default()
                    })
                })
                .map(Ok)
                .collect());
        }
        let destinations: Vec<_> = p
            .iter()
            .filter_map(|target| match target.as_target() {
                Target::Path(path) if path.parent() == Some(Path::new("/dst")) => {
                    Some(path.to_path_buf())
                }
                _ => None,
            })
            .collect();
        let destination_query = !o.follow_final_symlink() && !destinations.is_empty();
        if destination_query {
            assert_eq!(destinations.len(), p.len());
            self.calls
                .borrow_mut()
                .destination_probes
                .push(destinations.clone());
        }
        let mut results = self.fs.vgetattrs(p, o)?;
        if self.fault.get() == Fault::OversizedSource {
            for (target, result) in p.iter().zip(&mut results) {
                if matches!(target.as_target(), Target::Path(path) if path == Path::new("/src/a")) {
                    *result = Ok(vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs {
                        returned: Attributes::MODE | Attributes::SIZE,
                        ftype: FileType::Regular,
                        mode: libc::S_IFREG | 0o644,
                        size: isize::MAX as u64 + 1,
                        ..Default::default()
                    }));
                }
            }
        }
        if destination_query {
            match self.fault.get() {
                Fault::DestinationDenied => {
                    for (index, path) in destinations.iter().enumerate() {
                        if path == Path::new("/dst/c") {
                            results[index] = Err(OpError::failed(
                                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                                None,
                            ));
                        }
                    }
                }
                Fault::ShortDestinationMetadata => {
                    results.pop();
                }
                Fault::ExtraDestinationMetadata => results.push(Err(OpError::not_attempted(
                    std::io::Error::from(std::io::ErrorKind::Other),
                ))),
                _ => {}
            }
        }
        Ok(results)
    }
    fn vopen(&self, r: &[OpenOp]) -> vnfs::VecResult<Self::File> {
        self.calls.borrow_mut().opens += 1;
        self.calls.borrow_mut().open_batches.push(r.len());
        if r.len() == 1 && r[0].flags().contains(OpenFlags::CREATE_NEW) {
            let cause = || std::io::Error::from_raw_os_error(libc::EEXIST);
            match self.fault.get() {
                Fault::UnknownExclusiveOpen => {
                    return Ok(vec![Err(OpError::unknown(cause(), None))]);
                }
                Fault::RejectedExclusiveOpen => return Err(cause()),
                _ => {}
            }
        }
        let mut files = self.fs.vopen(r)?;
        if self.fault.get() == Fault::ExtraOpenOutcome {
            files.push(Ok(self.fs.open(r[0].path())?));
        }
        if self.fault.get() == Fault::BadOpenCount {
            files.pop();
        }
        Ok(files)
    }
    fn vread<'a>(
        &self,
        r: impl IntoIterator<Item = ReadOp<'a, Self::File>>,
        o: ReadOptions,
    ) -> vnfs::VecResult<ReadOutput> {
        let r: Vec<_> = r.into_iter().collect();
        let ranges: Vec<_> = r
            .iter()
            .filter_map(|op| op.range_parts().map(|(_, offset, length)| (offset, length)))
            .collect();
        let attempt = {
            let mut calls = self.calls.borrow_mut();
            calls.read_batches.push(ranges.clone());
            calls.read_attempts += 1;
            calls.read_attempts
        };
        if let Some((at, errno)) = self.read_failure.get()
            && at == attempt
        {
            self.read_failure.set(None);
            return Err(std::io::Error::from_raw_os_error((errno) as i32));
        }
        let failure = match self.fault.get() {
            Fault::MiddleReadFailure => Some(1),
            Fault::ReadAfter(at, index) if at == attempt => Some(index),
            Fault::ReadAfterOpen(at, index) if at == self.calls.borrow().opens => Some(index),
            Fault::LostRead => {
                return Err(vfsi_core::internal::errors::transport(
                    vfsi_core::TransportKind::Other,
                    "lost read reply",
                ));
            }
            _ => None,
        };
        if let Some(failed) = failure {
            assert!(failed < r.len());
            // A known rejected input has its own slot; healthy siblings still
            // execute. Keep the existing outer-rejection fixture above for std_io.
            let mut outcomes = self.fs.vread(
                r.into_iter()
                    .enumerate()
                    .filter_map(|(index, request)| (index != failed).then_some(request)),
                o,
            )?;
            outcomes.insert(
                failed,
                Err(OpError::not_attempted(std::io::Error::from_raw_os_error(
                    libc::EIO,
                ))),
            );
            return Ok(outcomes);
        }
        let mut out = self.fs.vread(r, o)?;
        let mut c = self.calls.borrow_mut();
        c.reads += 1;
        c.max_files = c.max_files.max(out.len());
        c.max_bytes = c.max_bytes.max(
            out.iter()
                .filter_map(|result| result.as_ref().ok())
                .map(ReadOutput::bytes_read)
                .sum(),
        );
        if self.fault.get() == Fault::ShortRead {
            for result in out.iter_mut().filter_map(|result| result.as_mut().ok()) {
                if result.bytes_read() > 3 {
                    *result = ReadOutput::buffered(result.offset(), 3, false);
                }
            }
        }
        if self.fault.get() == Fault::OwnedShortRead {
            for result in out.iter_mut().filter_map(|result| result.as_mut().ok()) {
                if let Some(data) = result.data().filter(|data| data.len() > 3) {
                    *result = ReadOutput::owned(result.offset(), data[..3].to_vec(), false);
                }
            }
        }
        if self.fault.get() == Fault::NoReadProgress
            && let Some(Ok(read)) = out.first_mut()
        {
            *read = ReadOutput::owned(read.offset(), Vec::new(), false);
        }
        if self.fault.get() == Fault::OversizedOwnedRead
            && let Some(Ok(read)) = out.first_mut()
        {
            *read = ReadOutput::owned(read.offset(), vec![0; ranges[0].1 + 1], true);
        }
        if self.fault.get() == Fault::BadRead {
            out.pop();
        }
        if self.fault.get() == Fault::ExtraReadOutcome {
            out.push(Ok(ReadOutput::owned(0, Vec::new(), true)));
        }
        if self.fault.get() == Fault::WrongOffset
            && let Some(Ok(read)) = out.first_mut()
        {
            *read = ReadOutput::buffered(1, read.bytes_read(), read.eof());
        }
        Ok(out)
    }
    fn vwrite<'a>(
        &self,
        r: &[WriteOp<'a, Self::File>],
        o: WriteOptions,
    ) -> vnfs::VecResult<WriteOutput> {
        self.calls.borrow_mut().writes += 1;
        let mut out = self.fs.vwrite(r, o)?;
        if self.fault.get() == Fault::LostWrite {
            return Ok(out
                .into_iter()
                .map(|_| {
                    Err(vnfs::error::OpError::unknown(
                        vfsi_core::internal::errors::transport(
                            TransportKind::InvalidReply,
                            "lost write reply",
                        ),
                        None,
                    ))
                })
                .collect());
        }
        if self.fault.get() == Fault::BadWrite {
            out[0].as_mut().unwrap().bytes_written += 1;
        }
        Ok(out)
    }
    fn vclose(&self, f: &mut [Self::File]) -> vnfs::VecResult<()> {
        self.calls.borrow_mut().closes += f.len();
        let mut outcomes = self.fs.vclose(f)?;
        if self.fault.get() == Fault::ExtraCloseOutcome {
            outcomes.push(Ok(()));
        }
        if self.fault.get() == Fault::Close && !outcomes.is_empty() {
            outcomes[0] = Err(OpError::failed(
                std::io::Error::from_raw_os_error(libc::EIO),
                None,
            ));
        }
        Ok(outcomes)
    }
    fn vmkdir<P: AsRef<Path>>(&self, p: &[MkDirOp<P>]) -> vfsi_core::VecResult<()> {
        self.fs.vmkdir(p)
    }
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        p: &[(P, Q)],
        options: vnfs::files::CopyOptions,
    ) -> vfsi_core::VecResult<()> {
        self.calls.borrow_mut().copies += 1;
        self.fs.vcopy(p, options)
    }
    fn vremove<P: AsRef<Path>>(
        &self,
        p: &[P],
        m: RemoveMode,
        o: RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        self.calls.borrow_mut().removes += 1;
        self.fs.vremove(p, m, o)
    }
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        p: &[(P, Q)],
        options: RenameMode,
    ) -> vnfs::VecResult<()> {
        use vnfs::error::OpError;
        self.calls.borrow_mut().renames += 1;
        match self.fault.get() {
            Fault::PartialCrossDevice => {
                self.fs
                    .renames_all(&[(p[0].0.as_ref(), p[0].1.as_ref())], options)
                    .map_err(vfsi_core::IndexedError::into_cause)?;
                Ok((0..p.len())
                    .map(|index| {
                        if index == 0 {
                            Ok(())
                        } else {
                            Err(OpError::not_attempted(std::io::Error::from_raw_os_error(
                                libc::EXDEV,
                            )))
                        }
                    })
                    .collect())
            }
            Fault::NonprefixCrossDevice => {
                self.fs
                    .renames_all(
                        &[
                            (p[0].0.as_ref(), p[0].1.as_ref()),
                            (p[2].0.as_ref(), p[2].1.as_ref()),
                        ],
                        options,
                    )
                    .map_err(vfsi_core::IndexedError::into_cause)?;
                Ok(p.iter()
                    .enumerate()
                    .map(|(index, _)| {
                        if index == 1 {
                            Err(OpError::failed(
                                std::io::Error::from_raw_os_error(libc::EXDEV),
                                None,
                            ))
                        } else {
                            Ok(())
                        }
                    })
                    .collect())
            }
            Fault::UnknownCrossDevice => Ok(p
                .iter()
                .map(|_| {
                    Err(OpError::unknown(
                        std::io::Error::from_raw_os_error(libc::EXDEV),
                        None,
                    ))
                })
                .collect()),
            Fault::ShortRename => Ok((0..p.len().saturating_sub(1)).map(|_| Ok(())).collect()),
            Fault::LongRename => Ok((0..p.len() + 1).map(|_| Ok(())).collect()),
            Fault::RenameProgress => Ok(p
                .iter()
                .map(|_| {
                    Err(OpError::failed(
                        std::io::Error::from_raw_os_error(libc::EXDEV),
                        Some(vnfs::error::IoProgress::Write { bytes_written: 1 }),
                    ))
                })
                .collect()),
            Fault::RejectedRename => Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
            Fault::LostRename => {
                self.fs
                    .renames_all(p, options)
                    .map_err(vfsi_core::IndexedError::into_cause)?;
                Ok(p.iter()
                    .map(|_| {
                        Err(OpError::unknown(
                            vfsi_core::internal::errors::transport(
                                TransportKind::InvalidReply,
                                "lost rename reply",
                            ),
                            None,
                        ))
                    })
                    .collect())
            }
            Fault::CrossDevice => Ok(p
                .iter()
                .map(|_| {
                    Err(OpError::failed(
                        std::io::Error::from_raw_os_error(libc::EXDEV),
                        None,
                    ))
                })
                .collect()),
            Fault::Rename => Ok(p
                .iter()
                .map(|_| {
                    Err(OpError::failed(
                        std::io::Error::from_raw_os_error(libc::EIO),
                        None,
                    ))
                })
                .collect()),
            _ => self.fs.vrename(p, options),
        }
    }
    fn vlistdirs<P: AsRef<Path>, C: Into<vfsi_core::api::ListDirControl>>(
        &self,
        p: &[P],
        o: ListDirOptions,
        c: impl FnMut(usize, DirectoryListing) -> Result<C>,
    ) -> vfsi_core::VecResult<TraversalCompletion> {
        self.calls
            .borrow_mut()
            .directory_waves
            .push(p.iter().map(|path| path.as_ref().to_path_buf()).collect());
        let fault = self.fault.get();
        if matches!(
            fault,
            Fault::DirectoryBackendFirst
                | Fault::DirectoryCallbacksReverse
                | Fault::DirectoryCallbacksForward
        ) {
            assert_eq!(p.len(), 2);
            let mut callback = c;
            let mut outcomes: Vec<_> = (0..p.len())
                .map(|_| Ok(TraversalCompletion::Complete))
                .collect();
            // Independent inputs can call back in reverse completion order.
            let order = if fault == Fault::DirectoryCallbacksForward {
                [0, 1]
            } else {
                [1, 0]
            };
            for index in order {
                if index == 0 && fault == Fault::DirectoryBackendFirst {
                    outcomes[0] = Err(OpError::failed(
                        vfsi_core::internal::errors::with_context(
                            vfsi_core::internal::errors::with_message(
                                std::io::Error::from_raw_os_error(libc::EACCES),
                                "selected backend failure",
                            ),
                            "vlistdirs",
                            p[0].as_ref(),
                        ),
                        None,
                    ));
                } else {
                    let mut result = self
                        .fs
                        .vlistdirs(&[p[index].as_ref()], o, |_, page| callback(index, page))?;
                    assert_eq!(result.len(), 1);
                    outcomes[index] = result.pop().unwrap();
                }
            }
            return Ok(outcomes);
        }
        self.fs.vlistdirs(p, o, c)
    }
    fn vstream<P: AsRef<Path>>(
        &self,
        p: &[P],
        o: StreamOptions,
        c: impl FnMut(usize, u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
    ) -> vfsi_core::VecResult<StreamCompletion> {
        self.calls.borrow_mut().stream_calls += 1;
        if let Fault::StreamOffset(offset) = self.fault.get() {
            let mut callback = c;
            for (offset, data) in [(0, b"abc".as_slice()), (offset, b"def".as_slice())] {
                match callback(0, offset, data) {
                    Err(cause) => return Ok(vec![Err(OpError::failed(cause, None))]),
                    Ok(ControlFlow::Break(())) => {
                        return Ok(vec![Ok(StreamCompletion::Stopped {
                            next_offset: offset + data.len() as u64,
                        })]);
                    }
                    Ok(ControlFlow::Continue(())) => {}
                }
            }
            return Ok(vec![Ok(StreamCompletion::Complete)]);
        }
        self.fs.vstream(p, o, c)
    }
}
fn fixture() -> (tempfile::TempDir, Harness) {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("src/nested")).unwrap();
    std::fs::write(t.path().join("src/a"), payload()).unwrap();
    std::fs::write(t.path().join("src/nested/b"), b"second").unwrap();
    std::fs::write(t.path().join("src/empty"), b"").unwrap();
    let fs = Harness::new(t.path());
    (t, fs)
}
fn payload() -> Vec<u8> {
    (0..151).collect()
}

#[test]
fn zero_read_budget_allows_empty_transfer_batches_and_empty_files() {
    let root = tempfile::tempdir().unwrap();
    let mut fs = Harness::new(root.path());
    fs.fs = fs
        .fs
        .with_limits(ResourceLimits::new().with_max_read_bytes(0));
    let empty: [&str; 0] = [];
    assert_eq!(
        copy_items(&fs, &empty, "/unused", TransferOptions::new()).unwrap(),
        TransferSummary::default()
    );
    assert_eq!(
        move_items(&fs, &empty, "/unused", TransferOptions::new()).unwrap(),
        TransferSummary::default()
    );
    assert!(!root.path().join("unused").exists());
    fs.fs.write("/empty", b"").unwrap();
    let copied = copy_items(&fs, &["/empty"], "/copy", TransferOptions::new()).unwrap();
    assert_eq!(copied.files_copied, 1);
    assert_eq!(copied.bytes_copied, Some(0));
    assert_eq!(std::fs::read(root.path().join("copy/empty")).unwrap(), b"");
    let moved = move_items(&fs, &["/empty"], "/move", TransferOptions::new()).unwrap();
    assert_eq!(moved.files_copied, 1);
    assert_eq!(moved.roots_removed, 1);
    assert!(!root.path().join("empty").exists());
    assert_eq!(std::fs::read(root.path().join("move/empty")).unwrap(), b"");
    assert_eq!(fs.calls.borrow().max_bytes, 0);
}

#[test]
fn zero_read_budget_allows_native_copies_and_rename_only_moves() {
    let root = tempfile::tempdir().unwrap();
    let mut fs = Harness::new(root.path());
    fs.fs = fs
        .fs
        .with_limits(ResourceLimits::new().with_max_read_bytes(0));
    fs.fs.write("/file", b"payload").unwrap();
    let options = TransferOptions::new().with_existing(Existing::Replace);
    let copied = copy_items(&fs, &["/file"], "/copy", options).unwrap();
    assert_eq!(copied.files_copied, 1);
    assert_eq!(copied.bytes_copied, None);
    let moved = move_items(&fs, &["/file"], "/move", options).unwrap();
    assert_eq!(moved.roots_renamed, 1);
    assert_eq!(moved.files_copied, 0);
    assert_eq!(moved.roots_removed, 0);
    assert_eq!(
        std::fs::read(root.path().join("copy/file")).unwrap(),
        b"payload"
    );
    assert_eq!(
        std::fs::read(root.path().join("move/file")).unwrap(),
        b"payload"
    );
    assert!(!root.path().join("file").exists());
    let calls = fs.calls.borrow();
    assert_eq!(calls.read_attempts, 0);
    assert_eq!(calls.writes, 0);
    assert_eq!(calls.copies, 1);
    assert_eq!(calls.renames, 1);
}

#[test]
fn zero_read_budget_rejects_nonempty_descriptor_copies_before_file_mutation() {
    for moving in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut fs = Harness::new(root.path());
        fs.fs = fs
            .fs
            .with_limits(ResourceLimits::new().with_max_read_bytes(0));
        fs.fs.write("/file", b"payload").unwrap();
        let error = if moving {
            move_items(&fs, &["/file"], "/dest", TransferOptions::new())
        } else {
            copy_items(&fs, &["/file"], "/dest", TransferOptions::new())
        }
        .unwrap_err();
        assert_eq!(error.cause().kind(), std::io::ErrorKind::FileTooLarge);
        assert_eq!(error.index(), Some(0));
        assert_eq!(
            vfsi_core::internal::errors::path(error.cause()),
            Some(Path::new("/file"))
        );
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"payload");
        assert!(!root.path().join("dest/file").exists());
        let calls = fs.calls.borrow();
        assert_eq!(calls.opens, 0);
        assert_eq!(calls.read_attempts, 0);
        assert_eq!(calls.writes, 0);
        assert_eq!(calls.removes, 0);
    }
}

#[test]
fn zero_read_budget_skips_existing_files_and_copies_empty_siblings() {
    let (root, mut fs) = fixture();
    fs.fs = fs
        .fs
        .with_limits(ResourceLimits::new().with_max_read_bytes(0));
    std::fs::create_dir(root.path().join("dest")).unwrap();
    std::fs::write(root.path().join("dest/a"), b"previous").unwrap();
    fs.fs.write("/empty", b"").unwrap();
    let options = TransferOptions::new().with_existing(Existing::Skip);

    let error = copy_items(
        &fs,
        &["/empty", "/src/a", "/src/nested/b"],
        "/dest",
        options,
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(2));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::FileTooLarge);
    assert!(!root.path().join("dest/empty").exists());
    assert!(!root.path().join("dest/b").exists());
    assert_eq!(fs.calls.borrow().opens, 0);

    let copied = copy_items(&fs, &["/empty", "/src/a"], "/dest", options).unwrap();
    assert_eq!(copied.entries_skipped, 1);
    assert_eq!(copied.files_copied, 1);
    assert_eq!(copied.bytes_copied, Some(0));
    assert_eq!(std::fs::read(root.path().join("dest/empty")).unwrap(), b"");

    let moved = move_items(&fs, &["/src/a"], "/dest", options).unwrap();
    assert_eq!(moved.entries_skipped, 1);
    assert_eq!(moved.roots_removed, 0);
    assert_eq!(std::fs::read(root.path().join("src/a")).unwrap(), payload());
    assert_eq!(
        std::fs::read(root.path().join("dest/a")).unwrap(),
        b"previous"
    );
}

#[test]
fn transfer_buffer_allocation_failure_preserves_existing_destination() {
    let (root, mut fs) = fixture();
    let chunk = isize::MAX as usize + 1;
    fs.fs = fs
        .fs
        .with_limits(ResourceLimits::new().with_max_read_bytes(chunk));
    fs.fault.set(Fault::OversizedSource);
    std::fs::create_dir(root.path().join("dst")).unwrap();
    std::fs::write(root.path().join("dst/a"), b"previous").unwrap();

    let error = copy_items_with_progress(
        &fs,
        &["/src/a"],
        "/dst",
        TransferOptions::new()
            .with_existing(Existing::Replace)
            .with_chunk_size(std::num::NonZeroUsize::new(chunk).unwrap()),
        |_| Ok(ControlFlow::Continue(())),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::OutOfMemory);
    assert_eq!(
        std::fs::read(root.path().join("dst/a")).unwrap(),
        b"previous"
    );
    assert_eq!(fs.calls.borrow().removes, 0);
    assert_eq!(fs.calls.borrow().opens, 0);

    let skipped = copy_items(
        &fs,
        &["/src/a"],
        "/dst",
        TransferOptions::new()
            .with_existing(Existing::Skip)
            .with_chunk_size(std::num::NonZeroUsize::new(chunk).unwrap()),
    )
    .unwrap();
    assert_eq!(skipped.entries_skipped, 1);
    assert_eq!(skipped.files_copied, 0);
    assert_eq!(
        std::fs::read(root.path().join("dst/a")).unwrap(),
        b"previous"
    );
}

#[test]
fn recursive_copy_is_bounded_and_short_reads_have_no_gaps() {
    let (t, fs) = fixture();
    fs.fault.set(Fault::ShortRead);
    let s = copy_tree(
        &fs,
        "/src",
        "/dst",
        TransferOptions::new()
            .with_chunk_size(std::num::NonZeroUsize::new(16).expect("nonzero tuning value"))
            .with_batch_size(std::num::NonZeroUsize::new(2).expect("nonzero tuning value")),
    )
    .unwrap();
    assert_eq!(s.files_copied, 3);
    assert_eq!(s.bytes_copied, Some(157));
    assert_eq!(std::fs::read(t.path().join("dst/a")).unwrap(), payload());
    assert_eq!(
        std::fs::read(t.path().join("dst/nested/b")).unwrap(),
        b"second"
    );
    let c = fs.calls.borrow();
    assert!(c.max_files <= 2 && c.max_bytes <= 32);
    assert_eq!(c.closes, 6);
    assert_eq!(c.copies, 0);
}

#[test]
fn conflict_policies_preserve_hard_links_and_reject_self_copy() {
    let (t, fs) = fixture();
    std::fs::create_dir(t.path().join("dst")).unwrap();
    std::fs::write(t.path().join("dst/a"), b"old").unwrap();
    std::fs::hard_link(t.path().join("dst/a"), t.path().join("unrelated")).unwrap();
    assert!(copy_items(&fs, &["/src/a"], "/dst", TransferOptions::new()).is_err());
    let skipped = copy_items(
        &fs,
        &["/src/a"],
        "/dst",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(skipped.entries_skipped, 1);
    assert_eq!(std::fs::read(t.path().join("dst/a")).unwrap(), b"old");
    let replaced = copy_items(
        &fs,
        &["/src/a"],
        "/dst",
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(replaced.bytes_copied, None);
    assert_eq!(fs.calls.borrow().copies, 1);
    assert_eq!(std::fs::read(t.path().join("unrelated")).unwrap(), b"old");
    assert_eq!(std::fs::read(t.path().join("dst/a")).unwrap(), payload());
    std::fs::remove_file(t.path().join("dst/a")).unwrap();
    std::fs::hard_link(t.path().join("src/a"), t.path().join("dst/a")).unwrap();
    assert!(
        copy_items(
            &fs,
            &["/src/a"],
            "/dst",
            TransferOptions::new().with_existing(Existing::Replace)
        )
        .is_err()
    );
}

#[test]
fn layouts_permissions_and_stats() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (t, fs) = fixture();
    std::fs::set_permissions(
        t.path().join("src/a"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    copy_items(&fs, &["/src"], "/container", TransferOptions::new()).unwrap();
    assert!(t.path().join("container/src/nested/b").exists());
    copy_items(
        &fs,
        &["/src"],
        "/contents",
        TransferOptions::new()
            .with_layout(CopyLayout::Contents)
            .with_preserve_permissions(true),
    )
    .unwrap();
    assert!(t.path().join("contents/nested/b").exists());
    assert_eq!(fs.calls.borrow().attrs, [1, 1, 1]);
    assert_eq!(
        std::fs::metadata(t.path().join("contents/a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    symlink("a", t.path().join("src/link")).unwrap();
    let s = tree_stats(&fs, "/src", ListDirOptions::new()).unwrap();
    assert_eq!(
        (s.files, s.directories, s.symlinks, s.file_bytes),
        (3, 2, 1, 157)
    );
    assert!(copy_tree(&fs, "/src", "/reject", TransferOptions::new()).is_err());
    let s = copy_tree(
        &fs,
        "/src",
        "/skip",
        TransferOptions::new().with_unsupported_entries(UnsupportedEntry::Skip),
    )
    .unwrap();
    assert_eq!(s.entries_skipped, 1);
    assert!(!t.path().join("skip/link").exists());
}

#[test]
fn transfer_root_overlap_reports_the_first_conflict_without_io() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    let mut sources: Vec<_> = (0..4_096).map(|i| format!("/src/file-{i:04}")).collect();
    sources.push("/src/file-2048/child".to_owned());
    sources.push("/src".to_owned());
    let error = copy_items(&fs, &sources, "/dst", TransferOptions::new()).unwrap_err();
    assert_eq!(error.index(), Some(4_096));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert!(fs.calls.borrow().metadata.is_empty());

    for (sources, index) in [
        (["/a-b", "/a/inner", "/a"], 2),
        (["/a-b", "/a", "/a/inner"], 2),
        (["/src/file", "/dst/file/child", "/other"], 1),
    ] {
        let error = copy_items(&fs, &sources, "/dst", TransferOptions::new()).unwrap_err();
        assert_eq!(error.index(), Some(index));
        assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
        assert!(fs.calls.borrow().metadata.is_empty());
    }

    fs.fs.create_dir_all("/a").unwrap();
    fs.fs.write("/a/inner", b"inner").unwrap();
    fs.fs.write("/a-b", b"sibling").unwrap();
    let summary = copy_items(
        &fs,
        &["/a-b", "/a/inner"],
        "/dst",
        TransferOptions::new().with_layout(CopyLayout::Contents),
    )
    .unwrap();
    assert_eq!(summary.files_copied, 2);
    assert_eq!(fs.fs.read("/dst/a-b").unwrap(), b"sibling");
    assert_eq!(fs.fs.read("/dst/inner").unwrap(), b"inner");
}

#[test]
fn overlap_depth_and_budgets_are_explicit() {
    let (t, fs) = fixture();
    for options in [
        TransferOptions::new().with_max_entries(0),
        TransferOptions::new().with_max_path_bytes(1),
    ] {
        assert!(copy_tree(&fs, "/src", "/dst", options).is_err());
        assert!(!t.path().join("dst").exists());
    }
    assert!(copy_tree(&fs, "/src", "/src/dst", TransferOptions::new()).is_err());
    assert!(copy_items(&fs, &["/src", "/src/a"], "/dst", TransferOptions::new()).is_err());
    assert!(copy_tree(&fs, "../src", "/dst", TransferOptions::new()).is_err());
    let s = move_items(
        &fs,
        &["/src"],
        "/dst",
        TransferOptions::new().with_max_depth(DepthLimit::new(0).unwrap()),
    )
    .unwrap();
    assert_eq!(s.entries_skipped, 1);
    assert_eq!(s.roots_removed, 0);
    assert!(t.path().join("src/nested/b").exists());
}

#[test]
fn cancellation_reports_completed_wave_and_retains_sources() {
    let (t, fs) = fixture();
    std::fs::write(t.path().join("src/c"), vec![23; 151]).unwrap();
    let s = move_items_with_progress(
        &fs,
        &["/src/a", "/src/c"],
        "/dst",
        TransferOptions::new()
            .with_chunk_size(std::num::NonZeroUsize::new(16).expect("nonzero tuning value"))
            .with_preserve_permissions(true),
        |p| {
            assert_eq!(p.file_bytes_copied, 16);
            Ok(ControlFlow::Break(()))
        },
    )
    .unwrap();
    assert!(s.stopped);
    assert!(fs.calls.borrow().attrs.iter().all(|&width| width == 0));
    assert_eq!(s.bytes_copied, Some(32));
    assert_eq!(s.roots_removed, 0);
    assert_eq!(std::fs::read(t.path().join("dst/a")).unwrap().len(), 16);
    assert!(t.path().join("src/a").exists());
    assert_eq!(std::fs::read(t.path().join("dst/c")).unwrap().len(), 16);
    assert!(t.path().join("src/c").exists());
    let s = copy_items_with_progress(
        &fs,
        &["/src/a", "/src/c"],
        "/wave",
        TransferOptions::new()
            .with_chunk_size(std::num::NonZeroUsize::new(16).expect("nonzero tuning value")),
        |p| {
            assert_eq!(p.summary.bytes_copied, Some(32));
            Ok(ControlFlow::Break(()))
        },
    )
    .unwrap();
    assert_eq!(s.bytes_copied, Some(32));
    assert_eq!(std::fs::read(t.path().join("wave/c")).unwrap().len(), 16);
}

#[test]
fn malformed_reads_lost_write_and_close_failure_never_delete_or_replay() {
    for fault in [
        Fault::BadRead,
        Fault::WrongOffset,
        Fault::BadWrite,
        Fault::LostWrite,
        Fault::Close,
    ] {
        let (t, fs) = fixture();
        fs.fault.set(fault);
        assert!(
            move_items(
                &fs,
                &["/src/a"],
                "/dst",
                TransferOptions::new().with_chunk_size(
                    std::num::NonZeroUsize::new(16).expect("nonzero tuning value")
                )
            )
            .is_err()
        );
        assert!(t.path().join("src/a").exists());
        let c = fs.calls.borrow();
        assert_eq!(c.removes, 0);
        assert_eq!(c.closes, 2);
        assert_eq!(
            c.writes,
            if fault == Fault::BadRead || fault == Fault::WrongOffset {
                0
            } else if fault == Fault::LostWrite || fault == Fault::BadWrite {
                1
            } else {
                10
            }
        );
    }
}

#[test]
fn move_rename_and_cross_device_fallback_are_not_general_retry() {
    for fault in [Fault::None, Fault::CrossDevice, Fault::Rename] {
        let (t, fs) = fixture();
        fs.fault.set(fault);
        if fault == Fault::CrossDevice {
            std::fs::create_dir_all(t.path().join("dst/src")).unwrap();
            std::fs::write(t.path().join("dst/src/a"), b"previous").unwrap();
        }
        let result = move_items(
            &fs,
            &["/src"],
            "/dst",
            TransferOptions::new().with_existing(Existing::Replace),
        );
        if fault == Fault::Rename {
            assert!(result.is_err());
            assert!(t.path().join("src").exists());
            assert_eq!(fs.calls.borrow().copies, 0);
        } else {
            let s = result.unwrap();
            assert_eq!(s.roots_renamed, u64::from(fault == Fault::None));
            assert_eq!(s.roots_removed, u64::from(fault == Fault::CrossDevice));
            if fault == Fault::CrossDevice {
                let calls = fs.calls.borrow();
                assert_eq!(calls.copies, 0);
                assert!(calls.writes > 0 && calls.max_bytes <= 64);
                assert_eq!(s.bytes_copied, Some(157));
            }
            assert!(!t.path().join("src").exists());
            assert_eq!(
                std::fs::read(t.path().join("dst/src/a")).unwrap(),
                payload()
            );
        }
    }
}

#[test]
fn skipped_move_and_callback_error_keep_source() {
    let (t, fs) = fixture();
    std::fs::create_dir(t.path().join("dst")).unwrap();
    std::fs::write(t.path().join("dst/a"), b"old").unwrap();
    let s = move_items(
        &fs,
        &["/src/a"],
        "/dst",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(s.roots_removed, 0);
    assert!(t.path().join("src/a").exists());
    assert!(
        move_items_with_progress(&fs, &["/src/a"], "/error", TransferOptions::new(), |_| Err(
            std::io::Error::from_raw_os_error(libc::ECANCELED)
        ))
        .is_err()
    );
    assert!(t.path().join("src/a").exists());
}

#[test]
fn vector_counts_and_storage_follow_cohort_limits() {
    let t = tempfile::tempdir().unwrap();
    let fs = Harness::new(t.path());
    std::fs::create_dir(t.path().join("src")).unwrap();
    let sources: Vec<_> = (0..12).map(|i| format!("/src/{i}")).collect();
    for p in &sources {
        std::fs::write(t.path().join(p.trim_start_matches('/')), vec![7; 16]).unwrap();
    }
    copy_items(
        &fs,
        &sources,
        "/batch",
        TransferOptions::new()
            .with_chunk_size(std::num::NonZeroUsize::new(16).expect("nonzero tuning value"))
            .with_preserve_permissions(true),
    )
    .unwrap();
    {
        let c = fs.calls.borrow();
        assert_eq!(c.reads, 3);
        assert_eq!(c.writes, 3);
        assert_eq!(c.max_bytes, 64);
        assert_eq!(c.max_files, 4);
        assert_eq!(c.attrs, [4, 4, 4]);
    }
    *fs.calls.borrow_mut() = Calls::default();
    copy_items(
        &fs,
        &sources,
        "/serial",
        TransferOptions::new()
            .with_batch_size(std::num::NonZeroUsize::new(1).expect("nonzero tuning value"))
            .with_chunk_size(std::num::NonZeroUsize::new(16).expect("nonzero tuning value")),
    )
    .unwrap();
    assert_eq!(fs.calls.borrow().reads, 12);
}

#[test]
fn vector_errors_map_to_original_mixed_root_index() {
    let (t, fs) = fixture();
    std::fs::write(t.path().join("other"), b"other").unwrap();
    fs.fault.set(Fault::MiddleReadFailure);
    let e = copy_items(
        &fs,
        &["/src/nested", "/src/a", "/other"],
        "/dst",
        TransferOptions::new()
            .with_chunk_size(std::num::NonZeroUsize::new(16).expect("nonzero tuning value")),
    )
    .unwrap_err();
    assert_eq!(e.index(), Some(2));
    assert_eq!(
        vfsi_core::internal::errors::path(e.cause()),
        Some(Path::new("/other"))
    );
    assert_eq!(fs.calls.borrow().closes, 4);
}

#[test]
fn aggregate_move_budget_and_progress_span_all_roots() {
    let (t, fs) = fixture();
    let mut seen = Vec::new();
    let s = move_items_with_progress(
        &fs,
        &["/src/a", "/src/empty"],
        "/done",
        TransferOptions::new(),
        |p| {
            seen.push(p.summary.bytes_copied);
            Ok(ControlFlow::Continue(()))
        },
    )
    .unwrap();
    assert_eq!(s.roots_removed, 2);
    assert_eq!(seen, vec![Some(64), Some(64), Some(128), Some(151)]);
    assert!(!t.path().join("src/a").exists());
    // A shared traversal budget cannot reset for each moved tree.
    let (t, fs) = fixture();
    std::fs::create_dir(t.path().join("second")).unwrap();
    std::fs::write(t.path().join("second/f"), b"f").unwrap();
    assert!(
        move_items(
            &fs,
            &["/src/nested", "/second"],
            "/out",
            TransferOptions::new().with_max_entries(3)
        )
        .is_err()
    );
    assert!(t.path().join("second/f").exists());
}

#[test]
fn ancestry_preflight_batches_distinct_prefixes() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir_all(root.path().join("source/one/two/tree")).unwrap();
    std::fs::create_dir_all(root.path().join("target/one/two")).unwrap();

    copy_tree(
        &fs,
        "/source/one/two/tree",
        "/target/one/two/new",
        TransferOptions::new().with_batch_size(std::num::NonZeroUsize::new(3).unwrap()),
    )
    .unwrap();

    let calls = fs.calls.borrow();
    assert_eq!(calls.metadata, [1, 1, 3, 3, 1]);
    assert!(root.path().join("target/one/two/new").is_dir());
}

#[test]
fn directory_aliases_are_rejected_before_copy_or_move() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir_all(root.path().join("source/child")).unwrap();
    std::os::unix::fs::symlink(".", root.path().join("alias")).unwrap();
    let error = move_items(
        &fs,
        &["/source"],
        "/alias/source",
        TransferOptions::new().with_layout(CopyLayout::Contents),
    )
    .unwrap_err();
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert!(root.path().join("source/child").is_dir());
    let error = copy_tree(
        &fs,
        "/source",
        "/alias/source/child/new",
        TransferOptions::new(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!root.path().join("source/child/new").exists());
    std::fs::create_dir_all(root.path().join("parent/same/same/nested")).unwrap();
    let error = move_items(
        &fs,
        &["/parent/same"],
        "/alias/parent",
        TransferOptions::new().with_layout(CopyLayout::Contents),
    )
    .unwrap_err();
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert!(root.path().join("parent/same/same/nested").is_dir());
    assert!(!root.path().join("parent/same/nested").exists());
    assert_eq!(fs.calls.borrow().removes, 0);
}

#[test]
fn directory_moves_check_ancestors_introduced_by_a_link_target() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir_all(root.path().join("base/folder/same/folder/same")).unwrap();
    std::fs::write(
        root.path().join("base/folder/same/folder/same/payload"),
        b"original",
    )
    .unwrap();
    std::os::unix::fs::symlink("base/folder", root.path().join("shortcut")).unwrap();
    let options = TransferOptions::new().with_layout(CopyLayout::Contents);

    let error = move_items(&fs, &["/shortcut/same"], "/base", options).unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(fs.calls.borrow().removes, 0);
    assert!(!root.path().join("base/folder/same/payload").exists());

    let result = move_items(&fs, &["/shortcut/same"], "/separate", options).unwrap();
    assert_eq!(result.roots_removed, 1);
    assert_eq!(
        std::fs::read(root.path().join("separate/folder/same/payload")).unwrap(),
        b"original"
    );
}

#[test]
fn regular_replacement_queries_only_plausible_identity_pairs() {
    for (layout, checks_per_file) in [(CopyLayout::Contents, 2), (CopyLayout::Container, 1)] {
        check_regular_replacement(layout, checks_per_file);
    }
}

fn check_regular_replacement(layout: CopyLayout, checks_per_file: usize) {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir(root.path().join("source")).unwrap();
    std::fs::create_dir(root.path().join("destination")).unwrap();
    let names: Vec<_> = (0..32).map(|index| format!("file-{index}")).collect();
    for name in &names {
        std::fs::write(root.path().join("source").join(name), b"new").unwrap();
        std::fs::write(root.path().join("destination").join(name), b"old").unwrap();
    }
    let sources: Vec<_> = names.iter().map(|name| format!("/source/{name}")).collect();
    let result = copy_items(
        &fs,
        &sources,
        "/destination",
        TransferOptions::new()
            .with_layout(layout)
            .with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(result.files_copied, names.len() as u64);
    assert_eq!(
        fs.calls.borrow().identity_pairs,
        names.len() * checks_per_file
    );
    for name in &names {
        assert_eq!(
            std::fs::read(root.path().join("destination").join(name)).unwrap(),
            b"new"
        );
    }
}

#[test]
fn replacing_one_hard_link_keeps_the_other_source_entry() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    fs.fs.create_dir_all("/src").unwrap();
    fs.fs.create_dir_all("/dst").unwrap();
    fs.fs.write("/src/a", b"new a").unwrap();
    fs.fs.write("/src/b", b"original b").unwrap();
    std::fs::hard_link(root.path().join("src/b"), root.path().join("dst/a")).unwrap();

    let copied = copy_items(
        &fs,
        &["/src/a", "/src/b"],
        "/dst",
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(copied.files_copied, 2);
    assert_eq!(fs.fs.read("/dst/a").unwrap(), b"new a");
    assert_eq!(fs.fs.read("/dst/b").unwrap(), b"original b");
    assert_eq!(fs.fs.read("/src/b").unwrap(), b"original b");
}

#[test]
fn repeated_contents_destinations_are_checked_once() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir(root.path().join("destination")).unwrap();
    std::fs::write(root.path().join("destination/file"), b"previous").unwrap();
    let sources: Vec<_> = (0..12)
        .map(|index| {
            let directory = format!("source-{index}");
            std::fs::create_dir(root.path().join(&directory)).unwrap();
            std::fs::write(root.path().join(&directory).join("file"), [index]).unwrap();
            format!("/{directory}/file")
        })
        .collect();
    let result = copy_items(
        &fs,
        &sources,
        "/destination",
        TransferOptions::new()
            .with_layout(CopyLayout::Contents)
            .with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(result.files_copied, sources.len() as u64);
    assert_eq!(fs.calls.borrow().identity_pairs, sources.len() * 2);
    assert_eq!(
        std::fs::read(root.path().join("destination/file")).unwrap(),
        [11]
    );
}

#[test]
fn directory_transfer_does_not_assume_final_link_and_ancestor_lookup_agree() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir_all(root.path().join("tree/sub/child")).unwrap();
    std::fs::create_dir_all(root.path().join("tree/sub/sub")).unwrap();
    std::fs::create_dir_all(root.path().join("external/sub")).unwrap();
    std::fs::write(root.path().join("tree/sub/payload"), b"retain").unwrap();
    std::os::unix::fs::symlink("/tree/sub/child", root.path().join("external/redirect")).unwrap();
    std::os::unix::fs::symlink("/external/redirect/../sub", root.path().join("shortcut")).unwrap();
    assert!(copy_tree(&fs, "/tree/sub", "/shortcut/new", TransferOptions::new()).is_err());
    assert!(!root.path().join("tree/sub/sub/new").exists());
    assert_eq!(
        std::fs::read(root.path().join("tree/sub/payload")).unwrap(),
        b"retain"
    );
}

#[test]
fn contents_replace_rejects_directory_colliding_with_a_regular_root() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("dst/b")).unwrap();
    std::fs::write(root.path().join("src/a"), b"first").unwrap();
    std::fs::write(root.path().join("src/b"), b"second").unwrap();
    std::fs::write(root.path().join("dst/a"), b"original").unwrap();
    std::fs::write(root.path().join("dst/b/child"), b"keep").unwrap();
    let options = TransferOptions::new()
        .with_layout(CopyLayout::Contents)
        .with_existing(Existing::Replace);

    for moving in [false, true] {
        let result = if moving {
            move_items(&fs, &["/src/a", "/src/b"], "/dst", options)
        } else {
            copy_items(&fs, &["/src/a", "/src/b"], "/dst", options)
        };
        let error = result.unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(root.path().join("src/a")).unwrap(), b"first");
        assert_eq!(std::fs::read(root.path().join("src/b")).unwrap(), b"second");
        assert_eq!(
            std::fs::read(root.path().join("dst/a")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(root.path().join("dst/b/child")).unwrap(),
            b"keep"
        );
    }
}

#[test]
fn aliased_contents_moves_do_not_replace_a_later_source() {
    for mixed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let fs = Harness::new(root.path());
        std::fs::create_dir(root.path().join("base")).unwrap();
        std::fs::create_dir_all(root.path().join("outside/tree")).unwrap();
        std::os::unix::fs::symlink(".", root.path().join("alias")).unwrap();
        std::fs::write(root.path().join("base/file"), b"later source").unwrap();
        let first = if mixed {
            "/outside/tree"
        } else {
            "/outside/file"
        };
        let payload = if mixed {
            "outside/tree/file"
        } else {
            "outside/file"
        };
        std::fs::write(root.path().join(payload), b"first source").unwrap();
        let error = move_items(
            &fs,
            &[first, "/base/file"],
            "/alias/base",
            TransferOptions::new()
                .with_layout(CopyLayout::Contents)
                .with_existing(Existing::Replace),
        )
        .unwrap_err();
        assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            std::fs::read(root.path().join("base/file")).unwrap(),
            b"later source"
        );
        assert_eq!(
            std::fs::read(root.path().join(payload)).unwrap(),
            b"first source"
        );
        assert_eq!(fs.calls.borrow().removes, 0);
    }
}

#[test]
fn skip_preserves_an_existing_alias_and_copies_other_sources() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir(root.path().join("dst")).unwrap();
    std::fs::write(root.path().join("src/a"), b"existing").unwrap();
    std::fs::write(root.path().join("src/b"), b"copy").unwrap();
    std::fs::hard_link(root.path().join("src/a"), root.path().join("dst/a")).unwrap();
    let result = copy_items(
        &fs,
        &["/src/a", "/src/b"],
        "/dst",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(result.entries_skipped, 1);
    assert_eq!(result.files_copied, 1);
    assert_eq!(
        std::fs::read(root.path().join("dst/a")).unwrap(),
        b"existing"
    );
    assert_eq!(std::fs::read(root.path().join("dst/b")).unwrap(), b"copy");
}

#[test]
fn skip_requires_a_definite_per_file_destination_conflict() {
    for (fault, index) in [
        (Fault::UnknownExclusiveOpen, Some(0)),
        (Fault::RejectedExclusiveOpen, None),
    ] {
        let (root, fs) = fixture();
        std::fs::create_dir(root.path().join("out")).unwrap();
        std::fs::write(root.path().join("source"), b"contents").unwrap();
        fs.fault.set(fault);

        let error = copy_items(
            &fs,
            &["/source"],
            "/out",
            TransferOptions::new().with_existing(Existing::Skip),
        )
        .unwrap_err();
        assert_eq!(error.index(), index);
        assert_eq!(error.cause().kind(), std::io::ErrorKind::AlreadyExists);
        let calls = fs.calls.borrow();
        assert_eq!(calls.opens, 2);
        assert_eq!(calls.closes, 1);
        assert_eq!(calls.writes, 0);
        assert!(!root.path().join("out/source").exists());
    }
}

#[test]
fn skipped_source_leaves_its_unresolvable_destination_unprobed() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir_all(root.path().join("source/dir")).unwrap();
    std::fs::create_dir(root.path().join("out")).unwrap();
    std::fs::write(root.path().join("source/dir/file"), b"copy").unwrap();
    std::os::unix::fs::symlink("dir", root.path().join("source/link")).unwrap();
    std::os::unix::fs::symlink("link", root.path().join("out/link")).unwrap();
    let summary = copy_items(
        &fs,
        &["/source/dir", "/source/link"],
        "/out",
        TransferOptions::new().with_unsupported_entries(UnsupportedEntry::Skip),
    )
    .unwrap();
    assert_eq!(summary.entries_skipped, 1);
    assert_eq!(
        std::fs::read(root.path().join("out/dir/file")).unwrap(),
        b"copy"
    );
    assert_eq!(
        std::fs::read_link(root.path().join("out/link")).unwrap(),
        Path::new("link")
    );
}

#[test]
fn skip_regular_destination_loop_preserves_supported_directory_copy() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    std::fs::create_dir_all(root.path().join("source/dir")).unwrap();
    std::fs::create_dir(root.path().join("out")).unwrap();
    std::fs::write(root.path().join("source/dir/file"), b"copy").unwrap();
    std::fs::write(root.path().join("source/link"), b"preserve").unwrap();
    std::os::unix::fs::symlink("link", root.path().join("out/link")).unwrap();
    let summary = move_items(
        &fs,
        &["/source/dir", "/source/link"],
        "/out",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(summary.entries_skipped, 1);
    assert_eq!(summary.roots_removed, 1);
    assert_eq!(
        std::fs::read(root.path().join("out/dir/file")).unwrap(),
        b"copy"
    );
    assert_eq!(
        std::fs::read(root.path().join("source/link")).unwrap(),
        b"preserve"
    );
    assert_eq!(
        std::fs::read_link(root.path().join("out/link")).unwrap(),
        Path::new("link")
    );
}

#[test]
fn replacement_refuses_destination_symlinks() {
    use std::os::unix::fs::symlink;
    let (t, fs) = fixture();
    std::fs::create_dir(t.path().join("dst")).unwrap();
    std::fs::write(t.path().join("outside"), b"keep").unwrap();
    symlink("../outside", t.path().join("dst/a")).unwrap();
    assert!(
        copy_items(
            &fs,
            &["/src/a"],
            "/dst",
            TransferOptions::new().with_existing(Existing::Replace)
        )
        .is_err()
    );
    assert_eq!(std::fs::read(t.path().join("outside")).unwrap(), b"keep");
    std::fs::create_dir(t.path().join("real")).unwrap();
    symlink("real", t.path().join("alias")).unwrap();
    assert!(copy_tree(&fs, "/src", "/alias", TransferOptions::new()).is_err());
    assert!(!t.path().join("real/a").exists());
}

#[test]
fn recursive_callback_errors_keep_original_root_index_and_path() {
    let (t, fs) = fixture();
    std::os::unix::fs::symlink("b", t.path().join("src/nested/link")).unwrap();
    let error = copy_items(
        &fs,
        &["/src/a", "/src/nested"],
        "/out",
        TransferOptions::new(),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(
        vfsi_core::internal::errors::path(error.cause()),
        Some(Path::new("/src/nested/link"))
    );
}

#[derive(Debug)]
struct CallbackSource(std::sync::Arc<usize>);
impl std::fmt::Display for CallbackSource {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "callback source {}", self.0)
    }
}
impl std::error::Error for CallbackSource {}

fn check_recursive_callback_selection(backend_first: bool, reverse: bool) {
    let (root, fs) = fixture();
    std::fs::create_dir(root.path().join("other")).unwrap();
    std::fs::write(root.path().join("other/c"), b"third").unwrap();
    fs.fault.set(if backend_first {
        Fault::DirectoryBackendFirst
    } else if reverse {
        Fault::DirectoryCallbacksReverse
    } else {
        Fault::DirectoryCallbacksForward
    });
    let sources = [std::sync::Arc::new(1), std::sync::Arc::new(3)];
    let mut callbacks = Vec::new();
    let error = copy_items_with_progress(
        &fs,
        &["/src/a", "/src/nested", "/src/empty", "/other"],
        "/out",
        TransferOptions::new(),
        |progress| {
            let source = if progress.source == Path::new("/src/nested/b") {
                Some(0)
            } else if progress.source == Path::new("/other/c") {
                Some(1)
            } else {
                None
            };
            if let Some(source) = source {
                callbacks.push(*sources[source]);
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    CallbackSource(sources[source].clone()),
                ))
            } else {
                Ok(ControlFlow::Continue(()))
            }
        },
    )
    .unwrap_err();
    assert_eq!(
        error.index(),
        Some(1),
        "wave slot zero belongs to original root one"
    );
    assert_eq!(error.cause().kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        callbacks,
        if backend_first {
            vec![3]
        } else if reverse {
            vec![3, 1]
        } else {
            vec![1, 3]
        }
    );
    assert_eq!(
        vfsi_core::internal::errors::path(error.cause()),
        Some(Path::new(if backend_first {
            "/src/nested"
        } else {
            "/src/nested/b"
        }))
    );
    if backend_first {
        assert_eq!(
            vfsi_core::native_status(error.cause()),
            Some(StatusCode::Errno(libc::EACCES as u32))
        );
        assert!(error.to_string().contains("selected backend failure"));
        assert_eq!(std::sync::Arc::strong_count(&sources[0]), 1);
        assert!(!root.path().join("out/nested/b").exists());
    } else {
        assert_eq!(vfsi_core::native_status(error.cause()), None);
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error.cause());
        let mut found = false;
        for _ in 0..8 {
            let Some(current) = source else { break };
            // io::Error exposes its opaque payload through get_ref; a payload
            // does not have to expose itself again as its own source.
            let payload = current
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref);
            if let Some(tag) = current
                .downcast_ref::<CallbackSource>()
                .or_else(|| payload.and_then(|payload| payload.downcast_ref::<CallbackSource>()))
            {
                assert!(std::sync::Arc::ptr_eq(&tag.0, &sources[0]));
                found = true;
                break;
            }
            source = current.source();
        }
        assert!(
            found,
            "the original opaque callback source must remain reachable"
        );
        assert_eq!(std::sync::Arc::strong_count(&sources[0]), 2);
        assert_eq!(
            std::fs::read(root.path().join("out/nested/b")).unwrap(),
            b"second"
        );
    }
    assert_eq!(
        std::sync::Arc::strong_count(&sources[1]),
        1,
        "discarded callback causes release their source"
    );
    drop(error);
    assert_eq!(std::sync::Arc::strong_count(&sources[0]), 1);
    assert_eq!(
        std::fs::read(root.path().join("out/other/c")).unwrap(),
        b"third"
    );
    assert_eq!(
        std::fs::read(root.path().join("src/nested/b")).unwrap(),
        b"second"
    );
    assert_eq!(
        std::fs::read(root.path().join("other/c")).unwrap(),
        b"third"
    );
    let calls = fs.calls.borrow();
    assert_eq!(
        calls.directory_waves,
        [vec![
            std::path::PathBuf::from("/src/nested"),
            "/other".into()
        ]],
        "no traversal replay"
    );
    assert_eq!(
        calls.open_batches.iter().sum::<usize>(),
        calls.closes,
        "all opened handles close on callback failure"
    );
}

#[test]
fn recursive_backend_failure_wins_over_a_later_callback_failure() {
    check_recursive_callback_selection(true, true);
}

#[test]
fn recursive_callbacks_select_lowest_wave_slot_and_keep_original_root_and_source() {
    for reverse in [false, true] {
        check_recursive_callback_selection(false, reverse);
    }
}

#[test]
fn tiny_files_fill_the_budget_without_reserving_whole_chunks() {
    let t = tempfile::tempdir().unwrap();
    let fs = Harness::new(t.path());
    std::fs::create_dir(t.path().join("src")).unwrap();
    let paths: Vec<_> = (0..12).map(|i| format!("/src/{i}")).collect();
    for p in &paths {
        std::fs::write(t.path().join(p.trim_start_matches('/')), b"tiny").unwrap();
    }
    copy_items(&fs, &paths, "/out", TransferOptions::new()).unwrap();
    let calls = fs.calls.borrow();
    assert_eq!(calls.reads, 1);
    assert_eq!(calls.writes, 1);
    assert_eq!(calls.max_files, 12);
    assert_eq!(calls.max_bytes, 48);
}

#[test]
fn bulk_move_copies_closes_and_deletes_in_vectors() {
    let t = tempfile::tempdir().unwrap();
    let fs = Harness::new(t.path());
    std::fs::create_dir(t.path().join("src")).unwrap();
    let paths: Vec<_> = (0..12).map(|i| format!("/src/{i}")).collect();
    for p in &paths {
        std::fs::write(t.path().join(p.trim_start_matches('/')), b"tiny").unwrap();
    }
    let result = move_items(&fs, &paths, "/out", TransferOptions::new()).unwrap();
    assert_eq!(result.roots_removed, 12);
    let calls = fs.calls.borrow();
    assert_eq!(calls.opens, 2);
    assert_eq!(calls.reads, 1);
    assert_eq!(calls.writes, 1);
    assert_eq!(calls.removes, 1);
    assert_eq!(calls.closes, 24);
    for p in &paths {
        assert!(!t.path().join(p.trim_start_matches('/')).exists());
    }
}

#[test]
fn bulk_move_renames_independent_roots_in_one_vector() {
    let t = tempfile::tempdir().unwrap();
    let fs = Harness::new(t.path());
    std::fs::create_dir(t.path().join("src")).unwrap();
    let paths: Vec<_> = (0..12).map(|i| format!("/src/{i}")).collect();
    for p in &paths {
        std::fs::write(t.path().join(p.trim_start_matches('/')), b"tiny").unwrap();
    }
    let result = move_items(
        &fs,
        &paths,
        "/out",
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(result.roots_renamed, 12);
    let calls = fs.calls.borrow();
    assert_eq!(calls.renames, 1);
    assert_eq!(calls.copies, 0);
    assert_eq!(calls.removes, 0);
}

#[test]
fn no_replace_is_atomic_and_reports_existing_destination() {
    let t = tempfile::tempdir().unwrap();
    let fs = Posix::new(t.path()).unwrap();
    std::fs::write(t.path().join("source"), b"source").unwrap();
    std::fs::write(t.path().join("exists"), b"existing").unwrap();
    let error = fs
        .renames_all(&[("/source", "/exists")], RenameMode::NoReplace)
        .unwrap_err();
    assert_eq!(error.cause().kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(t.path().join("source")).unwrap(), b"source");
    assert_eq!(std::fs::read(t.path().join("exists")).unwrap(), b"existing");
    fs.renames_all(&[("/source", "/new")], RenameMode::NoReplace)
        .unwrap();
    assert_eq!(std::fs::read(t.path().join("new")).unwrap(), b"source");
}

#[test]
fn exchange_swaps_names_atomically() {
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path()).unwrap();
    std::fs::write(root.path().join("left"), b"left data").unwrap();
    std::fs::write(root.path().join("right"), b"right data").unwrap();

    fs.renames_all(&[("/left", "/right")], RenameMode::Exchange)
        .unwrap();

    assert_eq!(
        std::fs::read(root.path().join("left")).unwrap(),
        b"right data"
    );
    assert_eq!(
        std::fs::read(root.path().join("right")).unwrap(),
        b"left data"
    );
}

#[test]
fn exchange_requires_both_names_to_exist() {
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path()).unwrap();
    std::fs::write(root.path().join("source"), b"source data").unwrap();

    let error = fs
        .renames_all(&[("/source", "/missing")], RenameMode::Exchange)
        .unwrap_err();

    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        std::fs::read(root.path().join("source")).unwrap(),
        b"source data"
    );
    assert!(!root.path().join("missing").exists());
}

#[test]
fn strict_no_replace_selects_the_error_without_suppressing_later_success() {
    let t = tempfile::tempdir().unwrap();
    let fs = Posix::new(t.path()).unwrap();
    std::fs::write(t.path().join("a"), b"a").unwrap();
    std::fs::write(t.path().join("b"), b"b").unwrap();
    std::fs::write(t.path().join("c"), b"c").unwrap();
    std::fs::write(t.path().join("exists"), b"old").unwrap();
    let error = fs
        .renames_all(
            &[("/a", "/x"), ("/b", "/exists"), ("/c", "/y")],
            RenameMode::NoReplace,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(std::fs::read(t.path().join("x")).unwrap(), b"a");
    assert_eq!(std::fs::read(t.path().join("y")).unwrap(), b"c");
    assert_eq!(std::fs::read(t.path().join("b")).unwrap(), b"b");
    assert_eq!(std::fs::read(t.path().join("exists")).unwrap(), b"old");
}

#[test]
fn partial_cross_device_rename_copies_only_explicit_unattempted_inputs() {
    let (t, fs) = fixture();
    fs.fault.set(Fault::PartialCrossDevice);
    let result = move_items(
        &fs,
        &["/src/a", "/src/empty", "/src/nested"],
        "/out",
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(result.roots_renamed, 1);
    assert_eq!(result.roots_removed, 2);
    assert_eq!(fs.calls.borrow().renames, 1);
    assert_eq!(std::fs::read(t.path().join("out/a")).unwrap(), payload());
    assert_eq!(std::fs::read(t.path().join("out/empty")).unwrap(), b"");
    assert_eq!(
        std::fs::read(t.path().join("out/nested/b")).unwrap(),
        b"second"
    );
    for p in ["src/a", "src/empty", "src/nested"] {
        assert!(!t.path().join(p).exists());
    }
}

#[test]
fn lost_batch_rename_reply_never_falls_back_or_replays() {
    let (t, fs) = fixture();
    fs.fault.set(Fault::LostRename);
    let result = move_items(
        &fs,
        &["/src/a", "/src/empty"],
        "/out",
        TransferOptions::new().with_existing(Existing::Replace),
    );
    assert!(vfsi_core::transport_kind(result.unwrap_err().cause()).is_some());
    let calls = fs.calls.borrow();
    assert_eq!(calls.renames, 1);
    assert_eq!(calls.opens, 0);
    assert_eq!(calls.copies, 0);
    assert_eq!(calls.removes, 0);
    assert_eq!(std::fs::read(t.path().join("out/a")).unwrap(), payload());
}

#[test]
fn move_wave_keeps_skipped_roots_but_deletes_complete_siblings() {
    let (t, fs) = fixture();
    std::fs::create_dir(t.path().join("out")).unwrap();
    std::fs::write(t.path().join("out/a"), b"old").unwrap();
    let result = move_items(
        &fs,
        &["/src/a", "/src/empty", "/src/nested"],
        "/out",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(result.roots_removed, 2);
    assert_eq!(result.entries_skipped, 1);
    assert!(t.path().join("src/a").exists());
    assert!(!t.path().join("src/empty").exists());
    assert!(!t.path().join("src/nested").exists());
    assert_eq!(std::fs::read(t.path().join("out/a")).unwrap(), b"old");
}

#[test]
fn batch_copy_error_or_close_error_retains_every_source() {
    for fault in [Fault::LostWrite, Fault::Close] {
        let (t, fs) = fixture();
        fs.fault.set(fault);
        assert!(
            move_items(
                &fs,
                &["/src/a", "/src/empty"],
                "/out",
                TransferOptions::new()
            )
            .is_err()
        );
        assert!(t.path().join("src/a").exists());
        assert!(t.path().join("src/empty").exists());
        let calls = fs.calls.borrow();
        assert_eq!(calls.removes, 0);
        assert_eq!(calls.closes, 4);
        if fault == Fault::LostWrite {
            assert_eq!(calls.writes, 1);
        }
    }
}

#[test]
fn standard_io_adapter_uses_vectors_and_has_an_independent_retained_cursor() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    fs.fs.write("/io", b"abcdef").unwrap();
    let file = fs
        .open_options()
        .read(true)
        .write(true)
        .open("/io")
        .unwrap();
    let mut io = fs.std_io(&file);
    let mut prefix = [0; 2];
    io.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"ab");
    assert_eq!(io.stream_position().unwrap(), 2);
    assert_eq!(io.seek(SeekFrom::End(-2)).unwrap(), 4);
    io.write_all(b"XY").unwrap();
    io.flush().unwrap();
    assert_eq!(fs.calls.borrow().writes, 1);
    assert_eq!(fs.calls.borrow().syncs, [(SyncMode::Data, 1)]);
    assert_eq!(fs.calls.borrow().metadata, [1]);
    assert_eq!(io.stream_position().unwrap(), 6);
    assert_eq!(fs.std_io(&file).stream_position().unwrap(), 0);
    fs.rename("/io", "/retained").unwrap();
    fs.fs.write("/io", b"replacement").unwrap();
    assert_eq!(io.seek(SeekFrom::End(0)).unwrap(), 6);
    io.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    io.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"abcdXY");
    assert_eq!(fs.attrs(Target::file(&file)).unwrap().len(), Some(6));
    fs.sync_all(&file).unwrap();
    assert_eq!(fs.calls.borrow().syncs.last(), Some(&(SyncMode::All, 1)));
    let reads = fs.calls.borrow().reads;
    fs.fault.set(Fault::BadRead);
    let mut bad_io = fs.std_io(&file);
    assert_eq!(
        bad_io.read(&mut prefix).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(bad_io.stream_position().unwrap(), 0);
    assert_eq!(fs.calls.borrow().reads, reads + 1);
    fs.fault.set(Fault::WrongOffset);
    assert!(bad_io.read(&mut prefix).is_err());
    assert_eq!(bad_io.stream_position().unwrap(), 0);
    fs.fault.set(Fault::BadWrite);
    assert!(bad_io.write_all(b"x").is_err());
    assert_eq!(bad_io.stream_position().unwrap(), 0);
}
#[test]
fn standard_io_requires_a_seek_after_an_uncertain_write_without_progress() {
    use std::io::{Seek, SeekFrom, Write};
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    fs.fs.write("/io", b"before").unwrap();
    let file = fs.open_options().append(true).open("/io").unwrap();
    let mut writer = fs.std_io(&file);

    fs.fault.set(Fault::LostWrite);
    let error = writer.write_all(b"+once").unwrap_err();
    assert_eq!(
        vfsi_core::transport_kind(&error),
        Some(TransportKind::InvalidReply)
    );
    assert_eq!(fs.fs.read("/io").unwrap(), b"before+once");

    fs.fault.set(Fault::None);
    assert!(writer.stream_position().is_err());
    assert!(writer.write_all(b"+again").is_err());
    assert_eq!(fs.calls.borrow().writes, 1);

    assert_eq!(writer.seek(SeekFrom::End(0)).unwrap(), 11);
    writer.write_all(b"+next").unwrap();
    assert_eq!(fs.fs.read("/io").unwrap(), b"before+once+next");
}

#[test]
fn absent_sizes_make_copy_and_tree_statistics_fail_instead_of_reporting_zero() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    fs.fs.write("/source", b"content").unwrap();
    fs.fault.set(Fault::MissingSize);
    assert_eq!(
        tree_stats(&fs, "/source", ListDirOptions::new())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        copy_items(&fs, &["/source"], "/dest", TransferOptions::new())
            .unwrap_err()
            .cause()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(fs.calls.borrow().opens, 0);
    assert_eq!(fs.fs.read("/source").unwrap(), b"content");
}

#[test]
fn collecting_adapter_retries_interrupted_reads_and_exact_budget_probes() {
    use std::io::{Read, Seek};
    for string in [false, true] {
        for interrupted_at in [1, 2] {
            let root = tempfile::tempdir().unwrap();
            let mut fs = Harness::new(root.path());
            fs.fs = fs
                .fs
                .with_limits(ResourceLimits::new().with_max_read_bytes(3));
            fs.fs.write("/file", b"abc").unwrap();
            fs.read_failure
                .set(Some((interrupted_at, libc::EINTR as u32)));
            let file = fs.open("/file").unwrap();
            let mut io = fs.std_io(&file);
            if string {
                let mut output = String::from("prefix");
                assert_eq!(io.read_to_string(&mut output).unwrap(), 3);
                assert_eq!(output, "prefixabc");
            } else {
                let mut output = vec![9];
                assert_eq!(io.read_to_end(&mut output).unwrap(), 3);
                assert_eq!(output, [9, b'a', b'b', b'c']);
            }
            assert_eq!(io.stream_position().unwrap(), 3);
            assert_eq!(fs.calls.borrow().read_attempts, 3);
        }
    }
    // Other errors must neither retry nor leave resized, unread bytes in output.
    for failed_at in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let mut fs = Harness::new(root.path());
        fs.fs = fs
            .fs
            .with_limits(ResourceLimits::new().with_max_read_bytes(3));
        fs.fs.write("/file", b"abcdef").unwrap();
        fs.read_failure.set(Some((failed_at, libc::EIO as u32)));
        let file = fs.open("/file").unwrap();
        let mut output = vec![9];
        let mut io = fs.std_io(&file);
        assert!(io.read_to_end(&mut output).is_err());
        assert_eq!(
            output,
            if failed_at == 1 {
                vec![9]
            } else {
                vec![9, b'a', b'b', b'c']
            }
        );
        assert_eq!(
            io.stream_position().unwrap(),
            if failed_at == 1 { 0 } else { 3 }
        );
        assert_eq!(fs.calls.borrow().read_attempts, failed_at);
    }
}

#[test]
fn collecting_adapter_uses_bounded_stream_chunks_without_small_read_waves() {
    use std::io::Read;
    let root = tempfile::tempdir().unwrap();
    let mut fs = Harness::new(root.path());
    fs.fs = fs.fs.with_limits(ResourceLimits::default());
    let chunk = vfsi_core::api::DEFAULT_READ_STREAM_CHUNK_BYTES;
    let payload = vec![0x5a; chunk + 17];
    fs.fs.write("/large", &payload).unwrap();
    let file = fs.open("/large").unwrap();
    let mut output = vec![9];
    assert_eq!(
        fs.std_io(&file).read_to_end(&mut output).unwrap(),
        payload.len()
    );
    assert_eq!(&output[1..], payload);
    let calls = fs.calls.borrow();
    // Two bounded data reads plus an EOF probe: a positive short POSIX read
    // proves progress, not EOF. Keep the exact bound to catch small-read waves.
    assert_eq!(calls.read_attempts, 3);
    assert_eq!(calls.reads, 3);
    assert_eq!(calls.max_files, 1);
    assert_eq!(calls.max_bytes, chunk);
}

#[test]
fn shared_open_builder_dispatches_scalar_and_batch_opens_through_vfsi() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    let mut options = fs.open_options();
    options.read(true).write(true).create_new(true);
    let first = options.open("/one").unwrap();
    assert_eq!(fs.calls.borrow().opens, 1);
    let rest = options.clone().vopen(&["/two", "/three"]).unwrap();
    assert_eq!(rest.len(), 2);
    assert_eq!(fs.calls.borrow().opens, 2);
    first.close().unwrap();
    fs.close_files(rest).unwrap();
}

#[path = "support/writer_copies.rs"]
mod writer_copies;

#[test]
fn move_uses_nonprefix_rename_outcomes_without_replaying_completed_roots() {
    let (root, fs) = fixture();
    fs.fault.set(Fault::NonprefixCrossDevice);
    let result = move_items(
        &fs,
        &["/src/a", "/src/empty", "/src/nested"],
        "/out",
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(result.roots_renamed, 2);
    assert_eq!(result.roots_removed, 1);
    assert_eq!(fs.calls.borrow().renames, 1);
    assert_eq!(fs.calls.borrow().removes, 1);
    assert_eq!(std::fs::read(root.path().join("out/a")).unwrap(), payload());
    assert_eq!(std::fs::read(root.path().join("out/empty")).unwrap(), b"");
    assert_eq!(
        std::fs::read(root.path().join("out/nested/b")).unwrap(),
        b"second"
    );
    for name in ["src/a", "src/empty", "src/nested"] {
        assert!(!root.path().join(name).exists());
    }
}

#[test]
fn move_never_falls_back_on_unknown_or_malformed_rename_outcomes() {
    for fault in [
        Fault::UnknownCrossDevice,
        Fault::ShortRename,
        Fault::LongRename,
        Fault::RenameProgress,
    ] {
        let (root, fs) = fixture();
        fs.fault.set(fault);
        let error = move_items(
            &fs,
            &["/src/a", "/src/empty"],
            "/out",
            TransferOptions::new().with_existing(Existing::Replace),
        )
        .unwrap_err();
        if fault == Fault::UnknownCrossDevice {
            assert_eq!(error.cause().kind(), std::io::ErrorKind::CrossesDevices);
            assert_eq!(error.index(), Some(0));
        } else {
            assert_eq!(
                vfsi_core::transport_kind(error.cause()),
                Some(TransportKind::InvalidReply)
            );
        }
        let calls = fs.calls.borrow();
        assert_eq!(calls.renames, 1);
        assert_eq!(calls.opens, 0);
        assert_eq!(calls.copies, 0);
        assert_eq!(calls.removes, 0);
        assert_eq!(std::fs::read(root.path().join("src/a")).unwrap(), payload());
        assert!(root.path().join("src/empty").exists());
        assert!(!root.path().join("out/a").exists());
    }
}

#[test]
fn move_can_copy_after_a_whole_vector_pre_dispatch_rename_rejection() {
    let (root, fs) = fixture();
    fs.fault.set(Fault::RejectedRename);
    let result = move_items(
        &fs,
        &["/src/a", "/src/empty"],
        "/out",
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(result.roots_renamed, 0);
    assert_eq!(result.roots_removed, 2);
    assert_eq!(fs.calls.borrow().renames, 1);
    assert_eq!(std::fs::read(root.path().join("out/a")).unwrap(), payload());
    assert!(!root.path().join("src/a").exists());
}

#[test]
fn contents_moves_replace_colliding_roots_in_input_order() {
    for existing in [false, true] {
        for progress in [false, true] {
            let (t, fs) = fixture();
            std::fs::create_dir_all(t.path().join("a")).unwrap();
            std::fs::create_dir_all(t.path().join("b")).unwrap();
            std::fs::write(t.path().join("a/x"), b"first").unwrap();
            std::fs::write(t.path().join("b/x"), b"second").unwrap();
            if existing {
                std::fs::create_dir(t.path().join("out")).unwrap();
                std::fs::write(t.path().join("out/x"), b"old").unwrap();
            }
            let options = TransferOptions::new()
                .with_layout(CopyLayout::Contents)
                .with_existing(Existing::Replace);
            let sources = ["/a/x", "/b/x"];
            let summary = if progress {
                move_items_with_progress(&fs, &sources, "/out", options, |_| {
                    Ok(ControlFlow::Continue(()))
                })
            } else {
                move_items(&fs, &sources, "/out", options)
            }
            .unwrap();
            assert_eq!(summary.roots_removed, 2);
            assert_eq!(std::fs::read(t.path().join("out/x")).unwrap(), b"second");
            assert!(!t.path().join("a/x").exists());
            assert!(!t.path().join("b/x").exists());
        }
    }
}

#[test]
fn single_writer_rejects_discontinuous_stream_offsets_before_writing_them() {
    for offset in [0, 2, 3, 4] {
        let root = tempfile::tempdir().unwrap();
        let fs = Harness::new(root.path());
        fs.fault.set(Fault::StreamOffset(offset));
        let mut output = Vec::new();
        let result = copy_to_writer(&fs, "/source", &mut output, StreamOptions::new());
        if offset == 3 {
            assert_eq!(result.unwrap(), 6);
            assert_eq!(output, b"abcdef");
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                vfsi_core::transport_kind(&(error)),
                Some(TransportKind::InvalidReply)
            );
            assert_eq!(
                output, b"abc",
                "never append a repeated, overlapping or gapped chunk"
            );
        }
        assert_eq!(fs.calls.borrow().stream_calls, 1, "never replay the stream");
    }
}

#[test]
fn replacement_preflight_batches_existing_and_missing_destinations_before_mutation() {
    for preserve in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let fs = Harness::new(root.path());
        std::fs::create_dir(root.path().join("dst")).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(root.path().join(name), name.as_bytes()).unwrap();
        }
        for name in ["a", "c"] {
            std::fs::write(root.path().join("dst").join(name), b"old").unwrap();
        }
        let result = copy_items(
            &fs,
            &["/a", "/b", "/c"],
            "/dst",
            TransferOptions::new()
                .with_existing(Existing::Replace)
                .with_preserve_permissions(preserve),
        )
        .unwrap();
        assert_eq!(result.files_copied, 3);
        for name in ["a", "b", "c"] {
            assert_eq!(
                std::fs::read(root.path().join("dst").join(name)).unwrap(),
                name.as_bytes()
            );
        }
        assert_eq!(
            fs.calls.borrow().destination_probes,
            [vec![
                std::path::PathBuf::from("/dst/a"),
                "/dst/b".into(),
                "/dst/c".into()
            ]]
        );
        assert_eq!(fs.calls.borrow().removes, 1);
    }
}

#[test]
fn replacement_preflight_errors_preserve_every_destination_and_original_root_index() {
    for preserve in [false, true] {
        for fault in [
            Fault::DestinationDenied,
            Fault::ShortDestinationMetadata,
            Fault::ExtraDestinationMetadata,
        ] {
            let root = tempfile::tempdir().unwrap();
            let fs = Harness::new(root.path());
            std::fs::create_dir(root.path().join("dst")).unwrap();
            for name in ["a", "b", "c"] {
                std::fs::write(root.path().join(name), b"new").unwrap();
            }
            for name in ["a", "c"] {
                std::fs::write(root.path().join("dst").join(name), b"old").unwrap();
            }
            fs.fault.set(fault);
            let error = copy_items(
                &fs,
                &["/a", "/b", "/c"],
                "/dst",
                TransferOptions::new()
                    .with_existing(Existing::Replace)
                    .with_preserve_permissions(preserve),
            )
            .unwrap_err();
            if fault == Fault::DestinationDenied {
                assert_eq!(error.index(), Some(2));
                assert_eq!(error.cause().kind(), std::io::ErrorKind::PermissionDenied);
                assert_eq!(
                    vfsi_core::internal::errors::path(error.cause()),
                    Some(Path::new("/c"))
                );
            } else {
                assert_eq!(error.index(), None);
                assert_eq!(
                    vfsi_core::transport_kind(error.cause()),
                    Some(vfsi_core::TransportKind::InvalidReply)
                );
            }
            assert_eq!(std::fs::read(root.path().join("dst/a")).unwrap(), b"old");
            assert_eq!(std::fs::read(root.path().join("dst/c")).unwrap(), b"old");
            assert!(!root.path().join("dst/b").exists());
            let calls = fs.calls.borrow();
            assert_eq!(calls.removes, 0);
            assert_eq!(calls.copies, 0);
            assert_eq!(calls.writes, 0);
            assert_eq!(calls.closes, if preserve { 3 } else { 0 });
        }
    }
}

#[test]
fn replacement_preflight_skips_new_directory_destinations() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    for name in ["a", "b"] {
        std::fs::write(root.path().join(name), b"new").unwrap();
    }
    copy_items(
        &fs,
        &["/a", "/b"],
        "/dst",
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert!(fs.calls.borrow().destination_probes.is_empty());
    assert_eq!(fs.calls.borrow().removes, 0);
}

#[test]
fn shared_ancestor_alias_reads_each_link_once() {
    let root = tempfile::tempdir().unwrap();
    let fs = Harness::new(root.path());
    fs.fs.create_dir_all("/real/tree").unwrap();
    fs.fs.write("/real/tree/child", b"tree").unwrap();
    fs.fs.symlink("real", "/middle").unwrap();
    fs.fs.symlink("middle", "/shortcut").unwrap();
    let mut sources = vec!["/shortcut/tree".to_owned()];
    for index in 0..8 {
        let path = format!("file-{index}");
        fs.fs.write(format!("/real/{path}"), b"regular").unwrap();
        sources.push(format!("/shortcut/{path}"));
    }
    let copied = copy_items(&fs, &sources, "/dest", TransferOptions::new()).unwrap();
    assert_eq!(copied.files_copied, 9);
    let calls = fs.calls.borrow();
    assert_eq!(calls.readlinks, 2);
    assert!(
        calls.metadata.len() <= 6,
        "the alias chain is shared by all sources: {:?}",
        calls.metadata
    );
    assert_eq!(fs.fs.read("/dest/tree/child").unwrap(), b"tree");
    assert_eq!(fs.fs.read("/dest/file-7").unwrap(), b"regular");
}

#[test]
#[ignore = "requires a private directory bind mount fixture"]
fn overlapping_bind_aliases_preserve_directory_and_regular_sources() {
    let path = std::env::var_os("VFSI_TRANSFER_BIND_ROOT").expect("bind fixture root");
    let fs = Posix::new(Path::new(&path)).unwrap();
    fs.create_dir("/s").unwrap();
    let copied = copy_tree(
        &fs,
        "/s",
        "/d",
        TransferOptions::new().with_max_path_bytes(8),
    )
    .unwrap();
    assert_eq!(copied.directories_processed, 1);

    let alias = fs.symlink_attrs("/mirror/same").unwrap();
    let original = fs.symlink_attrs("/base/folder/same").unwrap();
    assert!(alias.fs_obj_id().is_some());
    assert_eq!(alias.fs_obj_id(), original.fs_obj_id());

    let copied = copy_items(
        &fs,
        &["/stranger", "/mirror/file"],
        "/base/folder",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(copied.entries_skipped, 1);
    assert_eq!(copied.files_copied, 1);
    assert_eq!(
        fs.read("/base/folder/stranger/folder/file").unwrap(),
        b"replacement contents"
    );
    assert_eq!(
        fs.read("/base/folder/file").unwrap(),
        b"original regular file"
    );

    // The directory destination is a different mount, while the absent
    // regular destination would create a file inside the directory source.
    let error = copy_items(
        &fs,
        &["/inverse/source", "/inverse/other/new"],
        "/inverse/mirror",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(1), "{error:?}");
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        fs.symlink_attrs("/inverse/source/sub/new")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );

    let options = TransferOptions::new().with_layout(CopyLayout::Contents);
    let error = move_items(&fs, &["/mirror/same"], "/base", options).unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        fs.read("/base/folder/same/folder/same/payload").unwrap(),
        b"directory payload"
    );
    assert_eq!(
        fs.symlink_attrs("/base/folder/same/payload")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound,
    );

    let error = move_items(
        &fs,
        &["/stranger", "/mirror/file"],
        "/base",
        options.with_existing(Existing::Replace),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(fs.read("/mirror/file").unwrap(), b"original regular file");
    assert_eq!(
        fs.read("/stranger/folder/file").unwrap(),
        b"replacement contents"
    );
    assert_eq!(
        fs.symlink_attrs("/base/file").unwrap_err().kind(),
        std::io::ErrorKind::NotFound,
    );

    let replace = options.with_existing(Existing::Replace);
    fn check_nested_alias(fs: &impl Vfsi, replace: TransferOptions) {
        let error = move_items(fs, &["/deep/a", "/deep/b"], "/deep/dest", replace).unwrap_err();
        assert_eq!(error.index(), Some(0));
        assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(fs.read("/deep/a/first").unwrap(), b"first source");
        assert_eq!(fs.read("/deep/a/sub/file").unwrap(), b"replacement payload");
        assert_eq!(fs.read("/deep/b/nested/file").unwrap(), b"later source");
        assert_eq!(
            fs.symlink_attrs("/deep/dest/first").unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
        );
    }

    check_nested_alias(&fs, replace);
    let native = vfsi_posix::connect(Path::new(&path)).unwrap();
    check_nested_alias(&native, replace);

    let error = copy_items(
        &fs,
        &["/inverse/source"],
        "/inverse/mirror",
        replace.with_max_depth(DepthLimit::new(1).unwrap()),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(fs.read("/inverse/source/file").unwrap(), b"top payload");
    assert_eq!(
        fs.read("/inverse/source/sub/file").unwrap(),
        b"nested payload"
    );

    let copied = copy_items(&fs, &["/occluded/source"], "/occluded/elsewhere", options).unwrap();
    assert_eq!(copied.files_copied, 1);
    assert_eq!(
        fs.read("/occluded/elsewhere/child/visible").unwrap(),
        b"visible payload"
    );

    let moved = move_items(&fs, &["/mirror/same"], "/safe", options).unwrap();
    assert_eq!(moved.roots_removed, 1);
    assert_eq!(
        fs.read("/safe/folder/same/payload").unwrap(),
        b"directory payload"
    );

    fs.create_dir_all("/skip/tree").unwrap();
    fs.write("/skip/tree/item", b"copy").unwrap();
    fs.write("/skip/loop", b"skip").unwrap();
    fs.symlink("loop", "/safe/loop").unwrap();
    let copied = copy_items(
        &fs,
        &["/skip/tree", "/skip/loop"],
        "/safe",
        TransferOptions::new().with_existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(copied.entries_skipped, 1);
    assert_eq!(fs.read("/safe/tree/item").unwrap(), b"copy");
    assert_eq!(fs.read_link("/safe/loop").unwrap(), Path::new("loop"));
}
