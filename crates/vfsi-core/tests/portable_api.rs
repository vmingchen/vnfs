//! This target depends only on vfsi-core: extensions must not require a backend.
use std::{
    cell::{Cell, RefCell},
    io,
    path::{Path, PathBuf},
};
use vfsi_core::api::*;
use vfsi_core::{Vfsi, VfsiExt};

#[derive(Debug)]
struct TestFile;
impl io::Read for TestFile {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("unexpected scalar I/O")
    }
}
impl io::Write for TestFile {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("unexpected scalar I/O")
    }
}
impl io::Seek for TestFile {
    fn seek(&mut self, _: io::SeekFrom) -> io::Result<u64> {
        panic!("unexpected scalar I/O")
    }
}
impl FileHandle for TestFile {
    fn path(&self) -> &Path {
        Path::new("/file")
    }
    fn attrs(&self) -> Result<Attrs> {
        panic!("unexpected scalar metadata")
    }
    fn read_at(&self, _: &mut [u8], _: u64) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn write_at(&self, _: &[u8], _: u64) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn read_native(&mut self, _: &mut [u8]) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn read_to_end_with_limit(&mut self, _: usize) -> Result<Vec<u8>> {
        panic!("unexpected scalar I/O")
    }
    fn write_native(&mut self, _: &[u8]) -> Result<usize> {
        panic!("unexpected scalar I/O")
    }
    fn seek_native(&mut self, _: io::SeekFrom) -> Result<u64> {
        panic!("unexpected scalar I/O")
    }
    fn sync_data(&self) -> Result<()> {
        panic!("unexpected scalar I/O")
    }
    fn sync_all(&self) -> Result<()> {
        panic!("unexpected scalar I/O")
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
    listings: Cell<usize>,
    shape: Cell<u8>,
    tree: Cell<bool>,
    metadata: Cell<usize>,
    page_calls: Cell<usize>,
    waves: RefCell<Vec<Vec<PathBuf>>>,
    directory_options: RefCell<Vec<ListDirOptions>>,
}
impl Vfsi for PortableFs {
    fn capabilities(&self) -> Result<vfsi_core::Capabilities> {
        Ok(vfsi_core::Capabilities::empty())
    }
    fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, _: &[(P, Q)]) -> Result<()> {
        panic!("unexpected symlink")
    }
    fn vreadlink<P: AsRef<Path>>(&self, _: &[P]) -> Result<Vec<std::path::PathBuf>> {
        panic!("unexpected readlink")
    }
    fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, _: &[(P, Q)]) -> Result<()> {
        panic!("unexpected hardlink")
    }

    fn vstatfs<P: vfsi_core::AsTarget<Self::File>>(
        &self,
        targets: &[P],
    ) -> vfsi_core::api::Result<Vec<vfsi_core::FilesystemStats>> {
        if targets.is_empty() {
            Ok(Vec::new())
        } else {
            Err(vfsi_core::VfError::unsupported(0))
        }
    }
    fn vsetattrs<P: vfsi_core::AsTarget<Self::File>>(
        &self,
        _: &[vfsi_core::SetAttrsOp<P>],
    ) -> vfsi_core::api::Result<()> {
        Ok(())
    }

    type File = TestFile;
    fn limits(&self) -> ResourceLimits {
        ResourceLimits::default()
    }
    fn vgetattrs<P: AsRef<Path>>(&self, paths: &[P], options: AttrsOptions) -> Result<Vec<Attrs>> {
        assert!(self.tree.get(), "unexpected metadata");
        assert!(!options.follows_symlinks());
        self.metadata.set(self.metadata.get() + 1);
        Ok(paths
            .iter()
            .map(|path| tree_entry(path.as_ref(), true).attrs().clone())
            .collect())
    }
    fn vopen(&self, requests: &[OpenOp]) -> Result<Vec<TestFile>> {
        Ok(requests.iter().map(|_| TestFile).collect())
    }
    fn vread<'a>(
        &self,
        ops: impl IntoIterator<Item = ReadOp<'a, TestFile>>,
        _: ReadOptions,
    ) -> Result<Vec<ReadResult>> {
        self.reads.set(self.reads.get() + 1);
        let mut results: Vec<_> = ops
            .into_iter()
            .enumerate()
            .map(|(index, mut op)| {
                if let Some((_, offset, buffer)) = op.buffer_parts_mut() {
                    buffer.fill(index as u8);
                    ReadResult::buffered(offset, buffer.len(), false)
                } else {
                    assert!(op.whole_file_path().is_some());
                    ReadResult::owned(0, vec![index as u8], true)
                }
            })
            .collect();
        match self.shape.get() {
            1 => {
                results.pop();
            }
            2 => results.push(ReadResult::owned(0, vec![], true)),
            3 => results[0] = ReadResult::buffered(0, 1, true),
            4 => results[0] = ReadResult::owned(1, vec![0], true),
            5 => results[0] = ReadResult::owned(0, vec![0], false),
            6 => return Err(Error::transport(None, "lost reply")),
            _ => {}
        }
        Ok(results)
    }
    fn vwrite<'a>(&self, _: &[WriteOp<'a, TestFile>], _: WriteOptions) -> Result<Vec<WriteResult>> {
        panic!("unexpected write")
    }
    fn vclose(&self, _: &mut [TestFile]) -> Result<()> {
        Ok(())
    }
    fn vmkdir<P: AsRef<Path>>(&self, _: &[vfsi_core::MkDirOp<P>]) -> Result<()> {
        panic!("unexpected mkdir")
    }
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        _: &[(P, Q)],
        _: vfsi_core::api::CopyOption,
    ) -> Result<()> {
        panic!("unexpected copy")
    }
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        _: &[(P, Q)],
        _: RenameOptions,
    ) -> Result<()> {
        panic!("unexpected rename")
    }
    fn vremove<P: AsRef<Path>>(&self, _: &[P], _: RemoveMode, _: RemoveOptions) -> Result<()> {
        panic!("unexpected remove")
    }
    fn vstream<P: AsRef<Path>>(
        &self,
        _: &[P],
        _: StreamOptions,
        _: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
    ) -> Result<Vec<StreamCompletion>> {
        panic!("unexpected stream")
    }
    fn vlistdirs<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: ListDirOptions,
        mut callback: impl FnMut(usize, DirectoryListing) -> Result<ControlFlow<()>>,
    ) -> Result<Vec<TraversalCompletion>> {
        self.listings.set(self.listings.get() + 1);
        self.directory_options.borrow_mut().push(options);
        if self.tree.get() {
            self.waves
                .borrow_mut()
                .push(paths.iter().map(|p| p.as_ref().to_owned()).collect());
        }
        let mut results = Vec::new();
        if self.tree.get() && self.shape.get() == 11 {
            return Err(Error::client(99, libc::EIO as u32));
        }
        for (index, path) in paths.iter().enumerate() {
            let pages = if self.tree.get() {
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
                        } else {
                            path.as_ref().to_owned()
                        },
                        entries,
                    },
                )?
                .is_break();
                if stopped {
                    break;
                }
            }
            results.push(if stopped {
                TraversalCompletion::Stopped
            } else {
                TraversalCompletion::Complete
            });
            if stopped {
                break;
            }
        }
        if self.tree.get() {
            match self.shape.get() {
                7 => {
                    results.pop();
                }
                8 => results.push(TraversalCompletion::Complete),
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
            file: vfsi_core::VfFile::from_path(path.to_str().unwrap()),
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
            file: vfsi_core::VfFile::from_path("/tree/link"),
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
fn listdir_default_stops_before_next_page_without_metadata_or_root_events() {
    let fs = tree_fs();
    let mut seen = Vec::new();
    assert_eq!(
        fs.listdir(
            "/tree",
            ListDirOptions::new().fields(Attributes::MODE),
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
fn listdir_rejects_malformed_page_indices_parents_and_completions_without_replay() {
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
        assert!(error.is_transport(), "shape {shape}: {error}");
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
                ListDirOptions::new().recursive(true).sort_by_name(sort),
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
        ListDirOptions::new()
            .recursive(true)
            .enter_leave(true)
            .sort_by_name(true),
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
                ListDirOptions::new().recursive(true).enter_leave(true),
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
    fs.listdir("/empty", ListDirOptions::new().enter_leave(true), |event| {
        kinds.push(event.kind);
        Ok(WalkControl::Continue)
    })
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
            ListDirOptions::new()
                .recursive(recursive)
                .enter_leave(lifecycle)
                .sort_by_name(sort)
                .fields(Attributes::MODE)
                .max_entries(20)
                .max_path_bytes(1024),
            |_| Ok(WalkControl::Continue),
        )
        .unwrap();
        let calls = fs.directory_options.borrow();
        let first = calls[0].directory_options(fs.limits());
        assert_eq!(first.entry_limit(), 20 - usize::from(lifecycle));
        assert_eq!(
            first.path_byte_limit(),
            1024 - if recursive || lifecycle {
                "/tree".len()
            } else {
                0
            }
        );
        for options in calls.iter() {
            assert_eq!(options.attributes(), Attributes::MODE);
            assert!(!options.is_recursive());
            assert!(!options.emits_enter_leave());
            assert!(!options.sorts_by_name());
            assert_eq!(options.follows_symlinks(), !recursive && !lifecycle);
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
fn extensions_reject_malformed_results_and_do_not_replay_transport_failures() {
    let fs = PortableFs::default();
    for shape in 1..=6 {
        fs.shape.set(shape);
        let before = fs.reads.get();
        let error = fs.read_files(&["/a", "/b"]).unwrap_err();
        assert!(error.is_transport(), "{error}");
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
        .vread([ReadOp::into(&file, 7, &mut buffer)], ReadOptions::new())
        .unwrap();
    assert_eq!(buffer, [0; 3]);
    assert_eq!((result[0].offset(), result[0].read()), (7, 3));
    assert!(result[0].is_buffered());
    buffer.fill(42); // Results retain no borrow of caller storage.
    let owned = ReadResult::owned(2, vec![1, 2, 3], true);
    assert_eq!(owned.read(), owned.data().unwrap().len());
}
