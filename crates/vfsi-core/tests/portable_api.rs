//! This target depends only on vfsi-core: extensions must not require a backend.

#[path = "support/allocation.rs"]
mod allocation;

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
};
use vfsi_core::api::*;
use vfsi_core::{Vfsi, VfsiExt};

#[derive(Debug)]
struct TestFile;
impl FileHandle for TestFile {
    fn path(&self) -> &Path {
        Path::new("/file")
    }
    fn try_close(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_closed(&self) -> bool {
        false
    }
    fn close(self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct PortableFs {
    reads: Cell<usize>,
    streams: Cell<usize>,
    writes: Cell<usize>,
    stream_index_shift: Cell<usize>,
    listings: Cell<usize>,
    shape: Cell<u8>,
    tree: Cell<bool>,
    metadata: Cell<usize>,
    page_calls: Cell<usize>,
    closed: Cell<usize>,
    waves: RefCell<Vec<Vec<PathBuf>>>,
    directory_options: RefCell<Vec<ListDirOptions>>,
    removals: RefCell<Vec<(Vec<PathBuf>, RemoveOptions)>>,
}
impl Vfsi for PortableFs {
    fn capabilities(&self) -> Result<vfsi_core::Capabilities> {
        Ok(vfsi_core::Capabilities::empty())
    }
    fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, _: &[(P, Q)]) -> vfsi_core::VecResult<()> {
        panic!("unexpected symlink")
    }
    fn vreadlink<P: AsRef<Path>>(&self, _: &[P]) -> vfsi_core::VecResult<std::path::PathBuf> {
        panic!("unexpected readlink")
    }
    fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, _: &[(P, Q)]) -> vfsi_core::VecResult<()> {
        panic!("unexpected hardlink")
    }

    fn vstatfs<P: vfsi_core::AsTarget<Self::File>>(
        &self,
        targets: &[P],
    ) -> vfsi_core::VecResult<vfsi_core::FilesystemStats> {
        if targets.is_empty() {
            Ok(Vec::new())
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
    }
    fn vfsync(&self, files: &[&Self::File], _: SyncMode) -> vfsi_core::VecResult<()> {
        Ok(files.iter().map(|_| Ok(())).collect())
    }
    fn vsetattrs<P: vfsi_core::AsTarget<Self::File>>(
        &self,
        updates: &[vfsi_core::SetAttrsOp<P>],
    ) -> vfsi_core::VecResult<()> {
        Ok(updates.iter().map(|_| Ok(())).collect())
    }

    type File = TestFile;
    type Dir = TestFile;
    fn limits(&self) -> ResourceLimits {
        ResourceLimits::default()
    }
    fn vgetattrs<P: AsTarget<Self::File>>(
        &self,
        paths: &[P],
        options: AttrsOptions,
    ) -> vfsi_core::VecResult<Attrs> {
        assert!(self.tree.get(), "unexpected metadata");
        assert!(!options.follow_final_symlink());
        self.metadata.set(self.metadata.get() + 1);
        Ok(paths
            .iter()
            .map(|path| {
                tree_entry(
                    match path.as_target() {
                        Target::Path(path) => path,
                        Target::File(file) => FileHandle::path(file),
                    },
                    true,
                )
                .attrs()
                .clone()
            })
            .map(Ok)
            .collect())
    }
    fn vopen(&self, requests: &[OpenOp]) -> vfsi_core::VecResult<TestFile> {
        Ok(requests.iter().map(|_| Ok(TestFile)).collect())
    }
    fn vread<'a>(
        &self,
        ops: impl IntoIterator<Item = ReadOp<'a, TestFile>>,
        _: ReadOptions,
    ) -> vfsi_core::VecResult<ReadOutput> {
        self.reads.set(self.reads.get() + 1);
        if self.shape.get() == 6 {
            return Err(vfsi_core::internal::errors::transport(
                vfsi_core::TransportKind::Other,
                "rejected before dispatch",
            ));
        }
        if matches!(self.shape.get(), 23 | 24) {
            if self.reads.get() > 1 {
                return Ok(vec![Ok(ReadOutput::buffered(0, 0, true))]);
            }
            let progress = if self.shape.get() == 23 {
                IoProgress::Read(ReadOutput::buffered(1, 1, false))
            } else {
                IoProgress::Write { bytes_written: 1 }
            };
            return Ok(vec![Err(OpError::unknown(
                std::io::Error::from(ErrorKind::Interrupted),
                Some(progress),
            ))]);
        }
        let mut results: Vec<_> = ops
            .into_iter()
            .take(if matches!(self.shape.get(), 7 | 20) {
                1
            } else {
                usize::MAX
            })
            .enumerate()
            .map(|(index, mut op)| {
                if let Some((_, offset, buffer)) = op.buffer_parts_mut() {
                    buffer.fill(index as u8);
                    ReadOutput::buffered(offset, buffer.len(), false)
                } else if let Some((_, offset, length)) = op.range_parts() {
                    ReadOutput::owned(offset, vec![index as u8; length.min(1)], false)
                } else {
                    assert!(op.whole_file_path().is_some());
                    ReadOutput::owned(0, vec![index as u8], true)
                }
            })
            .collect();
        match self.shape.get() {
            1 => {
                results.pop();
            }
            2 => results.push(ReadOutput::owned(0, vec![], true)),
            3 => results[0] = ReadOutput::buffered(0, 1, true),
            4 => results[0] = ReadOutput::owned(1, vec![0], true),
            5 => results[0] = ReadOutput::owned(0, vec![0], false),
            20 => results.push(ReadOutput::owned(0, vec![1], true)),
            _ => {}
        }
        Ok(results.into_iter().map(Ok).collect())
    }
    fn vwrite<'a>(
        &self,
        requests: &[WriteOp<'a, TestFile>],
        _: WriteOptions,
    ) -> vfsi_core::VecResult<WriteOutput> {
        self.writes.set(self.writes.get() + 1);
        let (offset, bytes_written) = match self.shape.get() {
            21 => return Ok(Vec::new()),
            22 => {
                return Ok(vec![Err(OpError::failed(
                    std::io::Error::from(ErrorKind::Interrupted),
                    Some(IoProgress::Write { bytes_written: 2 }),
                ))]);
            }
            17 => (5, 1),
            18 => (5, 3),
            19 => (u64::MAX, 1),
            _ => panic!("unexpected write"),
        };
        Ok(requests
            .iter()
            .map(|request| {
                Ok(WriteOutput {
                    offset,
                    bytes_written: if request.data().is_empty() {
                        0
                    } else {
                        bytes_written
                    },
                    stable: false,
                })
            })
            .collect())
    }
    fn vclose(&self, files: &mut [TestFile]) -> vfsi_core::VecResult<()> {
        self.closed.set(self.closed.get() + files.len());
        Ok(files.iter().map(|_| Ok(())).collect())
    }
    fn vmkdir<P: AsRef<Path>>(&self, _: &[vfsi_core::MkDirOp<P>]) -> vfsi_core::VecResult<()> {
        panic!("unexpected mkdir")
    }
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        _pairs: &[(P, Q)],
        _: vfsi_core::api::CopyOptions,
    ) -> vfsi_core::VecResult<()> {
        panic!("unexpected copy")
    }
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        pairs: &[(P, Q)],
        _: RenameMode,
    ) -> vfsi_core::VecResult<()> {
        if pairs.is_empty() {
            Ok(Vec::new())
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
    }
    fn vremove<P: AsRef<Path>>(
        &self,
        paths: &[P],
        mode: RemoveMode,
        options: RemoveOptions,
    ) -> vfsi_core::VecResult<()> {
        assert_eq!(mode, RemoveMode::Contents);
        self.removals.borrow_mut().push((
            paths.iter().map(|path| path.as_ref().to_owned()).collect(),
            options,
        ));
        if self.shape.get() == 6 {
            Err(vfsi_core::internal::errors::transport(
                vfsi_core::TransportKind::Other,
                "lost removal reply",
            ))
        } else {
            Ok(paths.iter().map(|_| Ok(())).collect())
        }
    }
    fn vstream<P: AsRef<Path>>(
        &self,
        paths: &[P],
        _: StreamOptions,
        mut callback: impl FnMut(usize, u64, &[u8]) -> Result<std::ops::ControlFlow<()>>,
    ) -> vfsi_core::VecResult<StreamCompletion> {
        assert!(self.shape.get() == 13, "unexpected stream");
        self.streams.set(self.streams.get() + 1);
        Ok(paths
            .iter()
            .enumerate()
            .map(|(index, _)| {
                if index == 1 {
                    Err(vfsi_core::OpError::failed(
                        std::io::Error::from_raw_os_error(libc::ENOENT),
                        None,
                    ))
                } else {
                    callback(index + self.stream_index_shift.get(), 0, b"payload")
                        .map(|decision| {
                            if decision.is_break() {
                                StreamCompletion::Stopped { next_offset: 7 }
                            } else {
                                StreamCompletion::Complete
                            }
                        })
                        .map_err(|cause| vfsi_core::OpError::failed(cause, None))
                }
            })
            .collect())
    }
    fn vlistdirs<P: AsRef<Path>, C: Into<vfsi_core::api::ListDirControl>>(
        &self,
        paths: &[P],
        options: ListDirOptions,
        mut callback: impl FnMut(usize, DirectoryListing) -> Result<C>,
    ) -> vfsi_core::VecResult<TraversalCompletion> {
        self.listings.set(self.listings.get() + 1);
        self.directory_options.borrow_mut().push(options);
        if self.tree.get() {
            self.waves
                .borrow_mut()
                .push(paths.iter().map(|p| p.as_ref().to_owned()).collect());
        }
        let mut results = Vec::new();
        if self.tree.get() && self.shape.get() == 11 {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        for (index, path) in paths.iter().enumerate() {
            let outcome = (|| -> vfsi_core::OpResult<TraversalCompletion> {
                if self.shape.get() == 14 {
                    return Ok(TraversalCompletion::Stopped);
                }
                if self.shape.get() == 15 && index == 0 {
                    return Ok(TraversalCompletion::Complete);
                }
                let pages = if self.shape.get() == 16 {
                    vec![vec![tree_entry(&path.as_ref().join(".."), true)]]
                } else if self.tree.get() {
                    match path.as_ref().to_str().unwrap() {
                        "/tree" => vec![
                            vec![
                                tree_entry(Path::new("/tree/z"), false),
                                tree_entry(Path::new("/tree/prune"), true),
                            ],
                            vec![
                                tree_entry(Path::new("/tree/a"), false),
                                tree_entry(Path::new("/tree/keep"), true),
                                tree_entry(Path::new("/tree/other"), true),
                                tree_symlink(),
                            ],
                        ],
                        "/empty" => vec![vec![]],
                        _ => vec![vec![tree_entry(&path.as_ref().join("file"), false)]],
                    }
                } else {
                    vec![vec![]]
                };
                let mut stopped = false;
                for entries in pages {
                    self.page_calls.set(self.page_calls.get() + 1);
                    stopped = callback(
                        if self.tree.get() && self.shape.get() == 9 {
                            99
                        } else {
                            index
                        },
                        DirectoryListing {
                            path: if self.tree.get() && self.shape.get() == 10 {
                                "/wrong-parent".into()
                            } else if self.shape.get() == 15 {
                                path.as_ref().components().collect()
                            } else {
                                path.as_ref().to_owned()
                            },
                            entries,
                        },
                    )
                    .map_err(|cause| vfsi_core::OpError::failed(cause, None))?
                    .into()
                        == vfsi_core::api::ListDirControl::Stop;
                    if stopped {
                        break;
                    }
                }
                Ok(if stopped {
                    TraversalCompletion::Stopped
                } else {
                    TraversalCompletion::Complete
                })
            })();
            let stopped = outcome
                .as_ref()
                .is_ok_and(|completion| *completion == TraversalCompletion::Stopped);
            results.push(outcome);
            if stopped {
                results.extend((index + 1..paths.len()).map(|_| Ok(TraversalCompletion::Stopped)));
                break;
            }
        }
        if self.shape.get() == 12 && !results.is_empty() {
            results[0] = Err(vfsi_core::OpError::failed(
                std::io::Error::from_raw_os_error(libc::EIO),
                Some(vfsi_core::IoProgress::Write { bytes_written: 1 }),
            ));
        }
        if self.tree.get() {
            match self.shape.get() {
                7 => {
                    results.pop();
                }
                8 => results.push(Ok(TraversalCompletion::Complete)),
                _ => {}
            }
        }
        Ok(results)
    }
}

fn tree_entry(path: &Path, directory: bool) -> DirEntry {
    DirEntry::new(
        path.to_owned(),
        vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs {
            file: vfsi_core::FileOperand::from_path(path.to_str().unwrap()),
            masks: vfsi_core::AttrMask::MODE,
            ftype: if directory {
                vfsi_core::VfType::Directory
            } else {
                vfsi_core::VfType::Regular
            },
            ..Default::default()
        }),
    )
}
fn tree_symlink() -> DirEntry {
    DirEntry::new(
        "/tree/link".into(),
        vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs {
            file: vfsi_core::FileOperand::from_path("/tree/link"),
            masks: vfsi_core::AttrMask::MODE,
            ftype: vfsi_core::VfType::Symlink,
            ..Default::default()
        }),
    )
}
fn tree_fs() -> PortableFs {
    PortableFs {
        tree: Cell::new(true),
        ..Default::default()
    }
}

#[test]
fn std_io_collectors_reject_malformed_read_failures_without_retry() {
    use std::io::{Read, Seek};

    let file = TestFile;
    for shape in [23, 24] {
        let fs = PortableFs {
            shape: Cell::new(shape),
            ..Default::default()
        };
        let mut io = fs.std_io(&file);
        let mut output = b"existing".to_vec();
        let error = io.read_to_end(&mut output).unwrap_err();
        assert_eq!(
            vfsi_core::transport_kind(&error),
            Some(vfsi_core::TransportKind::InvalidReply),
        );
        assert_eq!(output, b"existing");
        assert_eq!(io.stream_position().unwrap(), 0);
        assert_eq!(fs.reads.get(), 1);
    }
}

#[test]
fn malformed_std_io_write_replies_require_cursor_recovery() {
    use std::io::{Seek, SeekFrom, Write};

    let fs = PortableFs::default();
    let file = TestFile;
    for shape in [18, 19, 21, 22] {
        let mut io = fs.std_io(&file);
        io.seek(SeekFrom::Start(5)).unwrap();
        fs.shape.set(shape);
        assert_eq!(
            vfsi_core::transport_kind(&io.write(b"x").unwrap_err()),
            Some(vfsi_core::TransportKind::InvalidReply),
        );

        let calls = fs.writes.get();
        fs.shape.set(17);
        assert!(io.stream_position().is_err(), "shape {shape}");
        assert!(io.write(b"y").is_err(), "shape {shape}");
        assert_eq!(fs.writes.get(), calls);

        io.seek(SeekFrom::Start(5)).unwrap();
        assert_eq!(io.write(b"z").unwrap(), 1);
        assert_eq!(io.stream_position().unwrap(), 6);
        assert_eq!(fs.writes.get(), calls + 1);
    }
}

#[test]
fn path_contents_helpers_dispatch_once_without_handles_or_listing() {
    let fs = tree_fs();
    let path = Path::new("/empty");
    let options = RemoveOptions::new()
        .with_batch_size(std::num::NonZeroUsize::new(2))
        .with_continue_on_error(true)
        .with_retries(1);
    fs.remove_dir_contents(path).unwrap();
    fs.remove_dir_contents_with_options(path, options).unwrap();
    assert_eq!(
        *fs.removals.borrow(),
        [
            (vec![path.to_owned()], RemoveOptions::default()),
            (vec![path.to_owned()], options)
        ]
    );
    assert_eq!(fs.metadata.get(), 2, "one no-follow validation per call");
    assert_eq!(
        fs.listings.get(),
        0,
        "do not collect entries in the extension"
    );

    fs.shape.set(6);
    let error = fs
        .remove_dir_contents_with_options(path, options)
        .unwrap_err();
    assert!(vfsi_core::transport_kind(&(error)).is_some());
    let _: &std::io::Error = &error;
    assert_eq!(
        fs.removals.borrow().len(),
        3,
        "never replay a lost mutation reply"
    );
    assert_eq!(fs.metadata.get(), 3);
}

#[test]
fn listdir_default_stops_before_next_page_without_metadata_or_root_events() {
    let fs = tree_fs();
    let mut seen = Vec::new();
    assert_eq!(
        fs.listdir(
            "/tree",
            ListDirOptions::new().with_fields(Attributes::MODE),
            |event| {
                assert_eq!(event.kind, WalkEventKind::Entry);
                assert_eq!(event.depth, 1);
                seen.push(event.entry.path().to_owned());
                Ok(WalkControl::Stop)
            }
        )
        .unwrap(),
        TraversalCompletion::Stopped
    );
    assert_eq!(seen, [Path::new("/tree/z")]);
    assert_eq!(fs.page_calls.get(), 1);
    assert_eq!(fs.metadata.get(), 0);
}

#[test]
fn listdir_requires_a_root_page_even_for_an_empty_directory() {
    let fs = tree_fs();
    for recursive in [false, true] {
        let options = ListDirOptions::new().with_recursive(recursive);
        let visitor = |_: &WalkEvent| panic!("empty directory must not emit an entry");
        fs.shape.set(0);
        assert_eq!(
            fs.listdir("/empty", options, visitor).unwrap(),
            TraversalCompletion::Complete
        );
        fs.shape.set(15);
        let error = fs.listdir("/empty", options, visitor).unwrap_err();
        assert_eq!(
            vfsi_core::transport_kind(&error),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
    }
    assert_eq!(fs.listings.get(), 4);
    assert_eq!(fs.page_calls.get(), 2);
}

#[test]
fn listdir_preserves_callback_error_provenance_in_every_presentation_mode() {
    for sort in [false, true] {
        for lifecycle in [false, true] {
            for expected in [
                std::io::Error::from_raw_os_error(libc::ENOSPC),
                vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(13)),
                vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Smb(0xc000_0022)),
                vfsi_core::internal::errors::transport(
                    vfsi_core::TransportKind::Connection,
                    "nested operation lost its reply",
                ),
                vfsi_core::internal::errors::transport(
                    vfsi_core::TransportKind::Other,
                    "unattributed nested failure",
                ),
            ] {
                let fs = tree_fs();
                let expected = vfsi_core::internal::errors::with_context(
                    expected,
                    "nested operation",
                    "/nested/file",
                );
                let facts = (
                    expected.kind(),
                    vfsi_core::native_status(&expected),
                    vfsi_core::transport_kind(&expected),
                    expected.to_string(),
                );
                let source =
                    expected.get_ref().unwrap() as *const (dyn std::error::Error + Send + Sync);
                let mut expected = Some(expected);
                let mut calls = 0;
                let actual = fs
                    .listdir(
                        "/tree",
                        WalkOptions::new(ListDirOptions::new())
                            .with_sort_by_name(sort)
                            .with_enter_leave(lifecycle),
                        |_| {
                            calls += 1;
                            Err(expected.take().expect("callback error must not replay"))
                        },
                    )
                    .unwrap_err();
                assert_eq!(
                    (
                        actual.kind(),
                        vfsi_core::native_status(&actual),
                        vfsi_core::transport_kind(&actual),
                        actual.to_string()
                    ),
                    facts,
                    "sort={sort}, lifecycle={lifecycle}"
                );
                assert_eq!(
                    vfsi_core::internal::errors::operation(&actual),
                    Some("nested operation")
                );
                assert_eq!(
                    vfsi_core::internal::errors::path(&actual),
                    Some(Path::new("/nested/file"))
                );
                assert!(std::ptr::eq(actual.get_ref().unwrap(), source));
                assert_eq!(calls, 1, "stop immediately after the callback failure");
            }
        }
    }
}

#[test]
fn listdir_rejects_malformed_pages_and_preserves_unindexed_rejections_without_replay() {
    for shape in 7..=11 {
        let fs = tree_fs();
        fs.shape.set(shape);
        let mut calls = 0;
        let error = fs
            .listdir("/tree", ListDirOptions::new(), |_| {
                calls += 1;
                Ok(WalkControl::Continue)
            })
            .unwrap_err();
        if shape == 11 {
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            assert_eq!(vfsi_core::transport_kind(&error), None);
        } else {
            assert_eq!(
                vfsi_core::transport_kind(&error),
                Some(vfsi_core::TransportKind::InvalidReply),
                "shape {shape}: {error}"
            );
        }
        assert_eq!(fs.listings.get(), 1, "never replay a malformed reply");
        if shape >= 9 {
            assert_eq!(calls, 0);
        }
    }
}

#[test]
fn listdir_recursive_entry_pruning_requires_lifecycle_configuration() {
    for sort in [false, true] {
        let fs = tree_fs();
        let error = fs
            .listdir(
                "/tree",
                vfsi_core::api::WalkOptions::new(ListDirOptions::new().with_recursive(true))
                    .with_sort_by_name(sort),
                |event| {
                    assert_eq!(event.kind, WalkEventKind::Entry);
                    Ok(if event.entry.attrs().is_dir() {
                        WalkControl::SkipSubtree
                    } else {
                        WalkControl::Continue
                    })
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("enter_leave(true)"));
        assert_eq!(fs.waves.borrow().len(), 1);
    }
}

#[test]
fn listdir_lifecycle_sorts_prunes_and_balances_directory_events() {
    let fs = tree_fs();
    let mut seen = Vec::new();
    fs.listdir(
        "/tree",
        vfsi_core::api::WalkOptions::new(ListDirOptions::new().with_recursive(true))
            .with_enter_leave(true)
            .with_sort_by_name(true),
        |event| {
            seen.push((event.kind, event.entry.path().to_owned(), event.depth));
            Ok(
                if event.kind == WalkEventKind::Enter
                    && event.entry.path() == Path::new("/tree/prune")
                {
                    WalkControl::SkipSubtree
                } else {
                    WalkControl::Continue
                },
            )
        },
    )
    .unwrap();
    use WalkEventKind::{Enter, Entry, Leave};
    assert_eq!(
        seen,
        [
            (Enter, "/tree", 0),
            (Entry, "/tree/a", 1),
            (Enter, "/tree/keep", 1),
            (Entry, "/tree/keep/file", 2),
            (Leave, "/tree/keep", 1),
            (Entry, "/tree/link", 1),
            (Enter, "/tree/other", 1),
            (Entry, "/tree/other/file", 2),
            (Leave, "/tree/other", 1),
            (Enter, "/tree/prune", 1),
            (Leave, "/tree/prune", 1),
            (Entry, "/tree/z", 1),
            (Leave, "/tree", 0),
        ]
        .map(|(kind, path, depth)| (kind, Path::new(path).to_owned(), depth))
    );
    assert!(
        fs.waves
            .borrow()
            .iter()
            .flatten()
            .all(|p| p != Path::new("/tree/prune") && p != Path::new("/tree/link"))
    );
}

#[test]
fn listdir_lifecycle_root_pruning_stop_and_empty_directory_need_no_child_reads() {
    for decision in [WalkControl::SkipSubtree, WalkControl::Stop] {
        let fs = tree_fs();
        let mut kinds = Vec::new();
        let completion = fs
            .listdir(
                "/tree",
                vfsi_core::api::WalkOptions::new(ListDirOptions::new().with_recursive(true))
                    .with_enter_leave(true),
                |event| {
                    kinds.push(event.kind);
                    Ok(if event.kind == WalkEventKind::Enter {
                        decision
                    } else {
                        WalkControl::Continue
                    })
                },
            )
            .unwrap();
        assert_eq!(fs.page_calls.get(), 0);
        if decision == WalkControl::Stop {
            assert_eq!(completion, TraversalCompletion::Stopped);
            assert_eq!(kinds, [WalkEventKind::Enter]);
        } else {
            assert_eq!(completion, TraversalCompletion::Complete);
            assert_eq!(kinds, [WalkEventKind::Enter, WalkEventKind::Leave]);
        }
    }
    let fs = tree_fs();
    let mut kinds = Vec::new();
    fs.listdir(
        "/empty",
        vfsi_core::api::WalkOptions::new(ListDirOptions::new()).with_enter_leave(true),
        |event| {
            kinds.push(event.kind);
            Ok(WalkControl::Continue)
        },
    )
    .unwrap();
    assert_eq!(kinds, [WalkEventKind::Enter, WalkEventKind::Leave]);
}

#[test]
fn buffered_listdir_forwards_remaining_budgets_fields_and_resolution_policy() {
    // Scripted pages exercise the adapter, not a second implementation of
    // traversal limits. Real-backend policy cases live in directory_pages.rs.
    for (recursive, lifecycle, sort) in [
        (false, false, true),
        (true, false, true),
        (false, true, false),
        (true, true, false),
    ] {
        let fs = tree_fs();
        fs.listdir(
            "/tree",
            vfsi_core::api::WalkOptions::new(
                ListDirOptions::new()
                    .with_recursive(recursive)
                    .with_fields(Attributes::MODE)
                    .with_max_entries(20)
                    .with_max_path_bytes(1024),
            )
            .with_enter_leave(lifecycle)
            .with_sort_by_name(sort),
            |_| Ok(WalkControl::Continue),
        )
        .unwrap();
        let calls = fs.directory_options.borrow();
        let first = calls[0].directory_options(fs.limits());
        assert_eq!(
            first.max_entries_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
            20 - usize::from(lifecycle)
        );
        assert_eq!(
            first.max_path_bytes_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
            1024 - if recursive || lifecycle {
                "/tree".len()
            } else {
                0
            }
        );
        for options in calls.iter() {
            assert_eq!(options.fields(), Attributes::MODE);
            assert!(!options.recursive());
            assert_eq!(options.allow_symlink_components(), !recursive && !lifecycle);
        }
    }
}

#[test]
fn blanket_extensions_preserve_vector_dispatch_and_root_grouping() {
    let fs = PortableFs::default();
    assert_eq!(fs.read_files(&["/a", "/b"]).unwrap(), [vec![0], vec![1]]);
    assert_eq!(fs.reads.get(), 1);
    assert_eq!(fs.read("/a").unwrap(), [0]);
    assert_eq!(fs.reads.get(), 2);
    let trees = fs
        .read_dirs_with_options(&["/a", "/b"], ListDirOptions::new())
        .unwrap();
    assert_eq!(fs.listings.get(), 1);
    assert_eq!(trees[0][0].path, Path::new("/a"));
    assert_eq!(trees[1][0].path, Path::new("/b"));
}

#[test]
fn directory_visitors_do_not_publish_malformed_entries() {
    let fs = PortableFs::default();
    fs.shape.set(16);
    let error = fs
        .listdirs_all(
            &["/tree"],
            ListDirOptions::new(),
            |_, _| -> Result<ControlFlow<()>> {
                panic!("malformed page must not reach the caller");
            },
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(
        vfsi_core::transport_kind(error.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
}

#[test]
fn strict_read_and_write_helpers_validate_backend_successes() {
    let fs = PortableFs::default();
    let error = fs
        .read_files_with_options(
            &["/a", "/b"],
            ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(1).unwrap()),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(
        vfsi_core::transport_kind(error.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );

    let file = TestFile;
    let writes = [WriteOp::at(&file, 0, b"ab")];
    fs.shape.set(17);
    let short = fs.write_batch(&writes, WriteOptions::new()).unwrap();
    assert_eq!((short[0].offset, short[0].bytes_written), (5, 1));
    for shape in [17, 18, 19] {
        fs.shape.set(shape);
        let error = fs
            .write_batch(&writes, WriteOptions::new().with_write_all(true))
            .unwrap_err();
        assert_eq!(error.index(), Some(0));
        assert_eq!(
            vfsi_core::transport_kind(error.cause()),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
    }
}

#[test]
fn whole_file_batch_adds_no_allocations_for_validation() {
    let fs = PortableFs::default();
    let ops = || (0..4096).map(|_| ReadOp::whole("/file"));
    let (_, backend_allocations, backend_bytes) =
        allocation::measure(|| fs.vread(ops(), ReadOptions::new()).unwrap());
    let (results, allocations, bytes) =
        allocation::measure(|| fs.read_batch(ops(), ReadOptions::new()).unwrap());
    assert_eq!(results.len(), 4096);
    assert!(
        results
            .iter()
            .all(|read| read.bytes_read() == 1 && read.eof())
    );
    assert_eq!((allocations, bytes), (backend_allocations, backend_bytes));
}

#[test]
fn read_batch_keeps_bounds_aligned_across_whole_file_runs() {
    let fs = PortableFs::default();
    let mut buffer = [0; 2];
    let results = fs
        .read_batch(
            [
                ReadOp::whole("/a"),
                ReadOp::whole("/b"),
                ReadOp::range(&TestFile, 7, 2),
                ReadOp::whole("/c"),
                ReadOp::whole("/d"),
                ReadOp::into(&TestFile, 9, &mut buffer),
                ReadOp::whole("/e"),
                ReadOp::whole("/f"),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(
        results.iter().map(ReadOutput::offset).collect::<Vec<_>>(),
        [0, 0, 7, 0, 0, 9, 0, 0],
    );
    assert_eq!(results[2].data(), Some([2].as_slice()));
    assert_eq!(results[5].bytes_read(), 2);
    assert_eq!(buffer, [5, 5]);
}

#[test]
fn read_batch_checks_buffer_and_range_successes() {
    let fs = PortableFs::default();
    let file = TestFile;
    let mut buffer = [0; 2];
    let results = fs
        .read_batch(
            [
                ReadOp::range(&file, 7, 2),
                ReadOp::into(&file, 9, &mut buffer),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(results[0].data(), Some([0].as_slice()));
    assert_eq!(results[1].bytes_read(), buffer.len());
    assert_eq!(buffer, [1, 1]);

    let error = fs
        .read_batch(
            [ReadOp::range(&file, 7, 2), ReadOp::range(&file, 9, 2)],
            ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(3).unwrap()),
        )
        .unwrap_err();
    assert_eq!(error.index(), None);
    assert_eq!(
        vfsi_core::transport_kind(error.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );

    let mut empty = [];
    fs.shape.set(3);
    let error = fs
        .read_batch([ReadOp::into(&file, 0, &mut empty)], ReadOptions::new())
        .unwrap_err();
    assert_eq!(error.index(), Some(0));

    fs.shape.set(4);
    let error = fs
        .read_batch([ReadOp::range(&file, 7, 2)], ReadOptions::new())
        .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(
        vfsi_core::transport_kind(error.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
}

#[test]
fn file_replacement_rejects_a_success_at_the_wrong_offset() {
    let fs = PortableFs::default();
    fs.shape.set(17);
    let error = fs.write("/file", b"x").unwrap_err();
    assert_eq!(
        vfsi_core::transport_kind(&error),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
    let error = fs
        .write_files(&[("/empty", b"".as_slice()), ("/bad", b"x")])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(
        vfsi_core::transport_kind(error.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
    assert_eq!(fs.closed.get(), 2);
}

#[test]
fn directory_collection_rejects_incomplete_or_misassociated_pages() {
    let fs = tree_fs();
    for shape in [10, 14, 15] {
        fs.shape.set(shape);
        let error = fs
            .read_dirs_with_options(&["/tree"], ListDirOptions::new())
            .unwrap_err();
        assert_eq!(error.index(), Some(0));
        assert_eq!(
            vfsi_core::transport_kind(error.cause()),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
    }
}

#[test]
fn directory_collection_bounds_entries_and_retained_paths() {
    let fs = tree_fs();
    let error = fs
        .read_dirs_with_options(&["/one", "/two"], ListDirOptions::new().with_max_entries(1))
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.cause().raw_os_error(), Some(libc::EFBIG));

    let fs = tree_fs();
    let options = ListDirOptions::new().with_recursive(true);
    let trees = fs.read_dirs_with_options(&["/tree"], options).unwrap();
    let listing = &trees[0][0];
    let path_bytes = listing.path.as_os_str().len()
        + listing
            .entries
            .iter()
            .map(|entry| entry.path().as_os_str().len())
            .sum::<usize>();
    assert_eq!(
        fs.read_dirs_with_options(&["/tree"], options.with_max_path_bytes(path_bytes))
            .unwrap()[0][0]
            .entries
            .len(),
        6,
        "count the directory path only once across its two pages",
    );
    let error = fs
        .read_dirs_with_options(&["/tree"], options.with_max_path_bytes(path_bytes - 1))
        .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.cause().raw_os_error(), Some(libc::EFBIG));
}

#[test]
fn extensions_reject_malformed_results_and_do_not_replay_transport_failures() {
    let fs = PortableFs::default();
    for shape in 1..=6 {
        fs.shape.set(shape);
        let before = fs.reads.get();
        let error = fs.read_files(&["/a", "/b"]).unwrap_err();
        assert!(
            vfsi_core::transport_kind(error.cause()).is_some(),
            "{error}"
        );
        assert_eq!(fs.reads.get(), before + 1);
        if shape == 6 {
            assert_eq!(error.index(), None);
        }
    }
}

#[test]
fn consuming_reads_release_caller_buffers_and_owned_counts_are_derived() {
    let fs = PortableFs::default();
    let file = TestFile;
    let mut buffer = [99; 3];
    let result = fs
        .read_batch([ReadOp::into(&file, 7, &mut buffer)], ReadOptions::new())
        .unwrap();
    assert_eq!(buffer, [0; 3]);
    assert_eq!((result[0].offset(), result[0].bytes_read()), (7, 3));
    assert!(result[0].is_buffered());
    buffer.fill(42); // Results retain no borrow of caller storage.
    let owned = ReadOutput::owned(2, vec![1, 2, 3], true);
    assert_eq!(owned.bytes_read(), owned.data().unwrap().len());
}

impl DirHandle for TestFile {
    fn path(&self) -> &Path {
        FileHandle::path(self)
    }
    fn try_close(&mut self) -> Result<()> {
        FileHandle::try_close(self)
    }
    fn is_closed(&self) -> bool {
        FileHandle::is_closed(self)
    }
    fn close(self) -> Result<()> {
        FileHandle::close(self)
    }
}

#[test]
fn directory_defaults_match_with_or_without_explicit_options() {
    let fs = PortableFs::default();
    fs.read_dir("/a").unwrap();
    fs.read_dir_with_options("/b", ListDirOptions::new())
        .unwrap();
    fs.read_dirs(&["/c"]).unwrap();
    fs.read_dirs_with_options(&["/d"], ListDirOptions::new())
        .unwrap();
    fs.listdirs_all(&["/e"], ListDirOptions::new(), |_, _| {
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    fs.listdir("/f", ListDirOptions::new(), |_| Ok(WalkControl::Continue))
        .unwrap();
    let options = fs.directory_options.take();
    assert_eq!(options.len(), 6);
    for options in options {
        assert_eq!(options.fields(), Attributes::stat());
        assert!(!options.fields().intersects(
            Attributes::UID
                | Attributes::GID
                | Attributes::ATIME
                | Attributes::MTIME
                | Attributes::CTIME
                | Attributes::CHANGE
        ));
    }
    let selected = Attributes::UID | Attributes::GID | Attributes::MTIME | Attributes::CHANGE;
    fs.read_dir_with_options("/g", ListDirOptions::new().with_fields(selected))
        .unwrap();
    assert_eq!(
        fs.directory_options.borrow()[0].fields(),
        selected | Attributes::MODE
    );
}

#[test]
fn collecting_and_page_adapters_reject_unhandled_policies_before_io() {
    let fs = PortableFs::default();
    assert_eq!(
        fs.read_dir_with_options("/missing", ListDirOptions::new().with_recursive(true))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(fs.listings.get(), 0);
    assert_eq!(fs.metadata.get(), 0);
}

#[test]
fn consuming_directory_entries_reuses_the_owned_path_allocation() {
    let path = PathBuf::from("/a/long-owned-directory-entry");
    let pointer = path.as_os_str().as_encoded_bytes().as_ptr();
    let attrs = vfsi_core::metadata_from_attrs(vfsi_core::VfAttrs {
        size: 27,
        returned: Attributes::SIZE,
        ..Default::default()
    });
    let (path, attrs) = DirEntry::new(path, attrs).into_parts();
    assert_eq!(path.as_os_str().as_encoded_bytes().as_ptr(), pointer);
    assert_eq!(attrs.len(), Some(27));
}

#[test]
fn directory_budget_overrides_remain_distinct_from_client_defaults() {
    let defaults = ResourceLimits::new()
        .with_max_directory_entries(7)
        .with_max_directory_path_bytes(11)
        .with_max_walk_depth(DepthLimit::new(3).unwrap());
    let inherited = ListDirOptions::new();
    assert_eq!(inherited.max_entries(), None);
    assert_eq!(inherited.max_path_bytes(), None);
    assert_eq!(inherited.max_depth(), None);
    let resolved = inherited.resolve(defaults);
    assert_eq!(resolved.max_entries(), Some(7));
    assert_eq!(resolved.max_path_bytes(), Some(11));
    assert_eq!(resolved.max_depth(), Some(DepthLimit::new(3).unwrap()));
    for budget in [0, 1, usize::MAX] {
        let options = inherited
            .with_max_entries(budget)
            .with_max_path_bytes(budget);
        assert_eq!(options.max_entries(), Some(budget));
        assert_eq!(options.max_path_bytes(), Some(budget));
        assert_eq!(options.resolve(defaults).max_entries(), Some(budget));
        assert_eq!(options.resolve(defaults).max_path_bytes(), Some(budget));
    }
    let walk = WalkOptions::new(inherited)
        .with_enter_leave(true)
        .with_sort_by_name(true)
        .with_listing_options(resolved);
    assert_eq!(walk.listing_options(), resolved);
    assert!(walk.enter_leave() && walk.sort_by_name());
}

#[test]
fn default_portable_paging_collects_complete_roots_without_backend_types() {
    let fs = tree_fs();
    let pages = fs
        .dir_pages_all(
            &["/tree", "/empty"],
            vec![None, None],
            ListDirOptions::new(),
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].listing.entries.len(), 6);
    assert!(pages[1].listing.entries.is_empty());
    assert!(
        pages
            .iter()
            .all(|p| p.continuation.is_none() && p.children.is_empty())
    );
    let fs = tree_fs();
    assert_eq!(
        vfsi_core::native_status(
            fs.dir_pages_all(
                &["/tree"],
                vec![None],
                ListDirOptions::new().with_max_entries(1),
                std::num::NonZeroUsize::new(1).unwrap()
            )
            .unwrap_err()
            .cause()
        ),
        Some(vfsi_core::StatusCode::Errno(libc::EFBIG as u32))
    );
    assert_eq!(
        fs.page_calls.get(),
        1,
        "stop collecting before a second over-budget page"
    );
}

#[test]
fn read_batch_checks_unconsumed_inputs_without_replaying_the_vector() {
    for shape in [7, 20] {
        let fs = PortableFs::default();
        fs.shape.set(shape);
        let error = fs
            .read_batch(
                [ReadOp::whole("/a"), ReadOp::whole("/b")],
                Default::default(),
            )
            .unwrap_err();
        assert_eq!(error.index(), None);
        assert_eq!(
            vfsi_core::transport_kind(error.cause()),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
        assert_eq!(fs.reads.get(), 1);
    }
}

#[test]
fn read_batch_treats_the_first_none_as_iterator_exhaustion() {
    let fs = PortableFs::default();
    let mut requests = [Some(ReadOp::whole("/a")), None, Some(ReadOp::whole("/b"))].into_iter();
    let results = fs
        .read_batch(
            std::iter::from_fn(|| requests.next().flatten()),
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn fallback_pages_quarantine_byte_progress_without_losing_healthy_roots() {
    let fs = PortableFs::default();
    fs.shape.set(12);
    let mut outcomes = fs
        .vlistdir_pages(
            &["/bad", "/healthy"],
            vec![None, None],
            ListDirOptions::new(),
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .unwrap()
        .into_iter();
    let error = outcomes.next().unwrap().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
    let _: &std::io::Error = error.cause();
    assert!(error.progress().is_none());
    assert!(outcomes.next().unwrap().is_ok());
    assert_eq!(fs.listings.get(), 1, "one vector, without replay");
}

#[test]
fn fallback_pages_require_a_listing_even_for_an_empty_directory() {
    let fs = PortableFs::default();
    fs.shape.set(15);
    let mut outcomes = fs
        .vlistdir_pages(
            &["/missing-page", "/empty//"],
            vec![None, None],
            ListDirOptions::new(),
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .unwrap()
        .into_iter();
    let error = outcomes.next().unwrap().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
    assert_eq!(
        vfsi_core::transport_kind(error.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
    let page = outcomes.next().unwrap().unwrap();
    assert_eq!(
        page.listing.path.as_os_str(),
        Path::new("/empty//").as_os_str()
    );
    assert!(page.listing.entries.is_empty());
    assert_eq!(fs.listings.get(), 1, "the missing page is not replayed");
}

#[test]
fn fallback_pages_keep_known_roots_when_completion_slots_are_missing() {
    let fs = tree_fs();
    fs.shape.set(7);
    let mut outcomes = fs
        .vlistdir_pages(
            &["/tree", "/empty"],
            vec![None, None],
            ListDirOptions::new(),
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .unwrap()
        .into_iter();
    let page = outcomes.next().unwrap().unwrap();
    assert_eq!(page.listing.entries.len(), 6);
    let error = outcomes.next().unwrap().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
    let _: &std::io::Error = error.cause();
    assert_eq!(fs.listings.get(), 1, "missing completion is not replayed");
}

#[test]
fn strict_stream_collection_dispatches_once_and_does_not_invent_a_prefix() {
    let fs = PortableFs::default();
    fs.shape.set(13);
    let mut seen = Vec::new();
    let error = fs
        .streams_all(
            &["/a", "/missing", "/b"],
            StreamOptions::new(),
            |index, _, bytes| {
                assert_eq!(bytes, b"payload");
                seen.push(index);
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        seen,
        [0, 2],
        "strict collection does not suppress a healthy suffix"
    );
    assert_eq!(
        fs.streams.get(),
        1,
        "never replay callbacks to collect a prefix"
    );
}

#[test]
fn scalar_stream_preserves_nested_callback_error_after_rich_normalization() {
    let fs = PortableFs::default();
    fs.shape.set(13);
    let expected = vfsi_core::internal::errors::with_context(
        vfsi_core::internal::errors::transport(
            vfsi_core::TransportKind::Other,
            "nested callback operation",
        ),
        "nested",
        "/nested",
    );
    let message = expected.to_string();
    let source = expected.get_ref().unwrap() as *const (dyn std::error::Error + Send + Sync);
    let mut expected = Some(expected);
    let actual = fs
        .read_stream_with_options("/a", StreamOptions::new(), |_, _| {
            Err(expected.take().expect("callback error must not replay"))
        })
        .unwrap_err();
    assert_eq!(actual.to_string(), message);
    assert_eq!(
        vfsi_core::transport_kind(&actual),
        Some(vfsi_core::TransportKind::Other)
    );
    assert_eq!(
        vfsi_core::internal::errors::operation(&actual),
        Some("nested")
    );
    assert_eq!(
        vfsi_core::internal::errors::path(&actual),
        Some(Path::new("/nested"))
    );
    assert!(std::ptr::eq(actual.get_ref().unwrap(), source));
    assert_eq!(fs.streams.get(), 1);
}

#[test]
fn scalar_stream_rejects_foreign_callback_indices_before_delivery() {
    for index in [0, 1, 99] {
        let fs = PortableFs::default();
        fs.shape.set(13);
        fs.stream_index_shift.set(index);
        let mut delivered = Vec::new();
        let result = fs.read_stream_with_options("/a", StreamOptions::new(), |offset, bytes| {
            assert_eq!(offset, 0);
            delivered.extend_from_slice(bytes);
            Ok(std::ops::ControlFlow::Continue(()))
        });
        if index == 0 {
            assert_eq!(result.unwrap(), StreamCompletion::Complete);
            assert_eq!(delivered, b"payload");
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                vfsi_core::transport_kind(&(error)),
                Some(vfsi_core::TransportKind::InvalidReply)
            );
            assert!(
                delivered.is_empty(),
                "never deliver a different input's bytes"
            );
        }
        assert_eq!(fs.streams.get(), 1, "never replay a malformed stream");
    }
}
