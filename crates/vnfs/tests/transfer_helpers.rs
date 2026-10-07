#![cfg(all(feature = "auto", target_os = "linux"))]

use std::{
    cell::{Cell, RefCell},
    ops::ControlFlow,
    path::Path,
};
use vnfs::{helpers::*, *};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    ShortRead,
    BadRead,
    BadWrite,
    WrongOffset,
    LostWrite,
    Close,
    CrossDevice,
    Rename,
    IndexedRead,
    PartialCrossDevice,
    LostRename,
}
#[derive(Default)]
struct Calls {
    opens: usize,
    renames: usize,
    reads: usize,
    writes: usize,
    closes: usize,
    copies: usize,
    removes: usize,
    max_bytes: usize,
    max_files: usize,
}
struct Harness {
    fs: Mounted,
    calls: RefCell<Calls>,
    fault: Cell<Fault>,
}
impl Harness {
    fn new(path: &Path) -> Self {
        Self {
            fs: Mounted::new(path).unwrap().with_limits(ResourceLimits {
                max_read_bytes: 64,
                ..Default::default()
            }),
            calls: RefCell::new(Calls::default()),
            fault: Cell::new(Fault::None),
        }
    }
}
impl Vfsi for Harness {
    type File = <Mounted as Vfsi>::File;
    fn capabilities(&self) -> Result<Capabilities> {
        self.fs.capabilities()
    }
    fn vsetattrs<P: AsTarget<Self::File>>(&self, targets: &[SetAttrsOp<P>]) -> Result<()> {
        self.fs.vsetattrs(targets)
    }
    fn vstatfs<P: AsTarget<Self::File>>(&self, targets: &[P]) -> Result<Vec<FilesystemStats>> {
        self.fs.vstatfs(targets)
    }
    fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
        self.fs.vsymlink(pairs)
    }
    fn vreadlink<P: AsRef<Path>>(&self, paths: &[P]) -> Result<Vec<std::path::PathBuf>> {
        self.fs.vreadlink(paths)
    }
    fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, pairs: &[(P, Q)]) -> Result<()> {
        self.fs.vhardlink(pairs)
    }
    fn limits(&self) -> ResourceLimits {
        self.fs.limits()
    }
    fn vgetattrs<P: AsRef<Path>>(&self, p: &[P], o: AttrsOptions) -> Result<Vec<Attrs>> {
        self.fs.vgetattrs(p, o)
    }
    fn vopen(&self, r: &[OpenOp]) -> Result<Vec<Self::File>> {
        self.calls.borrow_mut().opens += 1;
        self.fs.vopen(r)
    }
    fn vread<'a>(
        &self,
        r: impl IntoIterator<Item = ReadOp<'a, Self::File>>,
        o: ReadOptions,
    ) -> Result<Vec<ReadResult>> {
        if self.fault.get() == Fault::IndexedRead {
            return Err(Error::client(1, libc::EIO as u32));
        }
        let mut out = self.fs.vread(r, o)?;
        let mut c = self.calls.borrow_mut();
        c.reads += 1;
        c.max_files = c.max_files.max(out.len());
        c.max_bytes = c.max_bytes.max(out.iter().map(ReadResult::read).sum());
        if self.fault.get() == Fault::ShortRead {
            for result in &mut out {
                if result.read() > 3 {
                    *result = ReadResult::buffered(result.offset(), 3, false);
                }
            }
        }
        if self.fault.get() == Fault::BadRead {
            out.pop();
        }
        if self.fault.get() == Fault::WrongOffset {
            out[0] = ReadResult::buffered(1, out[0].read(), out[0].eof());
        }
        Ok(out)
    }
    fn vwrite<'a>(
        &self,
        r: &[WriteOp<'a, Self::File>],
        o: WriteOptions,
    ) -> Result<Vec<WriteResult>> {
        self.calls.borrow_mut().writes += 1;
        let mut out = self.fs.vwrite(r, o)?;
        if self.fault.get() == Fault::LostWrite {
            return Err(Error::transport_with_kind(
                None,
                TransportKind::InvalidReply,
                "lost write reply",
            ));
        }
        if self.fault.get() == Fault::BadWrite {
            out[0].written += 1;
        }
        Ok(out)
    }
    fn vclose(&self, f: &mut [Self::File]) -> Result<()> {
        self.calls.borrow_mut().closes += f.len();
        self.fs.vclose(f)?;
        if self.fault.get() == Fault::Close {
            return Err(Error::client(0, libc::EIO as u32));
        }
        Ok(())
    }
    fn vmkdir<P: AsRef<Path>>(&self, p: &[MkDirOp<P>]) -> Result<()> {
        self.fs.vmkdir(p)
    }
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        p: &[(P, Q)],
        options: vnfs::CopyOption,
    ) -> Result<()> {
        self.calls.borrow_mut().copies += 1;
        self.fs.vcopy(p, options)
    }
    fn vremove<P: AsRef<Path>>(&self, p: &[P], m: RemoveMode, o: RemoveOptions) -> Result<()> {
        self.calls.borrow_mut().removes += 1;
        self.fs.vremove(p, m, o)
    }
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        p: &[(P, Q)],
        options: RenameOptions,
    ) -> Result<()> {
        self.calls.borrow_mut().renames += 1;
        match self.fault.get() {
            Fault::PartialCrossDevice => {
                self.fs
                    .vrename(&[(p[0].0.as_ref(), p[0].1.as_ref())], options)?;
                Err(Error::client(1, libc::EXDEV as u32))
            }
            Fault::LostRename => {
                self.fs.vrename(p, options)?;
                Err(Error::transport_with_kind(
                    None,
                    TransportKind::InvalidReply,
                    "lost rename reply",
                ))
            }
            Fault::CrossDevice => Err(Error::client(0, libc::EXDEV as u32)),
            Fault::Rename => Err(Error::client(0, libc::EIO as u32)),
            _ => self.fs.vrename(p, options),
        }
    }
    fn vlistdirs<P: AsRef<Path>>(
        &self,
        p: &[P],
        o: ListDirOptions,
        c: impl FnMut(usize, DirectoryListing) -> Result<ControlFlow<()>>,
    ) -> Result<Vec<TraversalCompletion>> {
        self.fs.vlistdirs(p, o, c)
    }
    fn vstream<P: AsRef<Path>>(
        &self,
        p: &[P],
        o: StreamOptions,
        c: impl FnMut(usize, u64, &[u8]) -> Result<bool>,
    ) -> Result<Vec<StreamCompletion>> {
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
fn recursive_copy_is_bounded_and_short_reads_have_no_gaps() {
    let (t, fs) = fixture();
    fs.fault.set(Fault::ShortRead);
    let s = copy_tree(
        &fs,
        "/src",
        "/dst",
        CopyOptions::new().chunk_bytes(16).batch_size(2),
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
    assert!(copy_items(&fs, &["/src/a"], "/dst", CopyOptions::new()).is_err());
    let skipped = copy_items(
        &fs,
        &["/src/a"],
        "/dst",
        CopyOptions::new().existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(skipped.entries_skipped, 1);
    assert_eq!(std::fs::read(t.path().join("dst/a")).unwrap(), b"old");
    let replaced = copy_items(
        &fs,
        &["/src/a"],
        "/dst",
        CopyOptions::new().existing(Existing::Replace),
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
            CopyOptions::new().existing(Existing::Replace)
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
    copy_items(&fs, &["/src"], "/container", CopyOptions::new()).unwrap();
    assert!(t.path().join("container/src/nested/b").exists());
    copy_items(
        &fs,
        &["/src"],
        "/contents",
        CopyOptions::new()
            .layout(CopyLayout::Contents)
            .preserve_permissions(true),
    )
    .unwrap();
    assert!(t.path().join("contents/nested/b").exists());
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
    assert!(copy_tree(&fs, "/src", "/reject", CopyOptions::new()).is_err());
    let s = copy_tree(
        &fs,
        "/src",
        "/skip",
        CopyOptions::new().unsupported_entries(UnsupportedEntry::Skip),
    )
    .unwrap();
    assert_eq!(s.entries_skipped, 1);
    assert!(!t.path().join("skip/link").exists());
}

#[test]
fn overlap_depth_and_budgets_are_explicit() {
    let (t, fs) = fixture();
    for options in [
        CopyOptions::new().max_entries(0),
        CopyOptions::new().max_path_bytes(1),
        CopyOptions::new().batch_size(0),
        CopyOptions::new().chunk_bytes(0),
    ] {
        assert!(copy_tree(&fs, "/src", "/dst", options).is_err());
        assert!(!t.path().join("dst").exists());
    }
    assert!(copy_tree(&fs, "/src", "/src/dst", CopyOptions::new()).is_err());
    assert!(copy_items(&fs, &["/src", "/src/a"], "/dst", CopyOptions::new()).is_err());
    assert!(copy_tree(&fs, "../src", "/dst", CopyOptions::new()).is_err());
    let s = move_items(
        &fs,
        &["/src"],
        "/dst",
        CopyOptions::new().depth(DepthLimit::new(0)),
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
        CopyOptions::new().chunk_bytes(16),
        |p| {
            assert_eq!(p.file_bytes_copied, 16);
            Ok(ControlFlow::Break(()))
        },
    )
    .unwrap();
    assert!(s.stopped);
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
        CopyOptions::new().chunk_bytes(16),
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
        assert!(move_items(&fs, &["/src/a"], "/dst", CopyOptions::new().chunk_bytes(16)).is_err());
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
        let result = move_items(
            &fs,
            &["/src"],
            "/dst",
            CopyOptions::new().existing(Existing::Replace),
        );
        if fault == Fault::Rename {
            assert!(result.is_err());
            assert!(t.path().join("src").exists());
            assert_eq!(fs.calls.borrow().copies, 0);
        } else {
            let s = result.unwrap();
            assert_eq!(s.roots_renamed, u64::from(fault == Fault::None));
            assert_eq!(s.roots_removed, u64::from(fault == Fault::CrossDevice));
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
        CopyOptions::new().existing(Existing::Skip),
    )
    .unwrap();
    assert_eq!(s.roots_removed, 0);
    assert!(t.path().join("src/a").exists());
    assert!(
        move_items_with_progress(&fs, &["/src/a"], "/error", CopyOptions::new(), |_| Err(
            Error::client(0, libc::ECANCELED as u32)
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
    copy_items(&fs, &sources, "/batch", CopyOptions::new().chunk_bytes(16)).unwrap();
    {
        let c = fs.calls.borrow();
        assert_eq!(c.reads, 3);
        assert_eq!(c.writes, 3);
        assert_eq!(c.max_bytes, 64);
        assert_eq!(c.max_files, 4);
    }
    *fs.calls.borrow_mut() = Calls::default();
    copy_items(
        &fs,
        &sources,
        "/serial",
        CopyOptions::new().batch_size(1).chunk_bytes(16),
    )
    .unwrap();
    assert_eq!(fs.calls.borrow().reads, 12);
}

#[test]
fn vector_errors_map_to_original_mixed_root_index() {
    let (t, fs) = fixture();
    std::fs::write(t.path().join("other"), b"other").unwrap();
    fs.fault.set(Fault::IndexedRead);
    let e = copy_items(
        &fs,
        &["/src/nested", "/src/a", "/other"],
        "/dst",
        CopyOptions::new().chunk_bytes(16),
    )
    .unwrap_err();
    assert_eq!(e.index(), Some(2));
    assert_eq!(e.path(), Some(Path::new("/other")));
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
        CopyOptions::new(),
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
            CopyOptions::new().max_entries(3)
        )
        .is_err()
    );
    assert!(t.path().join("second/f").exists());
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
            CopyOptions::new().existing(Existing::Replace)
        )
        .is_err()
    );
    assert_eq!(std::fs::read(t.path().join("outside")).unwrap(), b"keep");
    std::fs::create_dir(t.path().join("real")).unwrap();
    symlink("real", t.path().join("alias")).unwrap();
    assert!(copy_tree(&fs, "/src", "/alias", CopyOptions::new()).is_err());
    assert!(!t.path().join("real/a").exists());
}

#[test]
fn recursive_callback_errors_keep_original_root_index_and_path() {
    let (t, fs) = fixture();
    std::os::unix::fs::symlink("b", t.path().join("src/nested/link")).unwrap();
    let error =
        copy_items(&fs, &["/src/a", "/src/nested"], "/out", CopyOptions::new()).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.path(), Some(Path::new("/src/nested/link")));
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
    copy_items(&fs, &paths, "/out", CopyOptions::new()).unwrap();
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
    let result = move_items(&fs, &paths, "/out", CopyOptions::new()).unwrap();
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
        CopyOptions::new().existing(Existing::Replace),
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
    let fs = Mounted::new(t.path()).unwrap();
    std::fs::write(t.path().join("source"), b"source").unwrap();
    std::fs::write(t.path().join("exists"), b"existing").unwrap();
    let error = fs
        .vrename(&[("/source", "/exists")], RenameOptions::NoReplace)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(t.path().join("source")).unwrap(), b"source");
    assert_eq!(std::fs::read(t.path().join("exists")).unwrap(), b"existing");
    fs.vrename(&[("/source", "/new")], RenameOptions::NoReplace)
        .unwrap();
    assert_eq!(std::fs::read(t.path().join("new")).unwrap(), b"source");
}

#[test]
fn exchange_swaps_names_atomically() {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path()).unwrap();
    std::fs::write(root.path().join("left"), b"left data").unwrap();
    std::fs::write(root.path().join("right"), b"right data").unwrap();

    fs.vrename(&[("/left", "/right")], RenameOptions::Exchange)
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
    let fs = Mounted::new(root.path()).unwrap();
    std::fs::write(root.path().join("source"), b"source data").unwrap();

    let error = fs
        .vrename(&[("/source", "/missing")], RenameOptions::Exchange)
        .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        std::fs::read(root.path().join("source")).unwrap(),
        b"source data"
    );
    assert!(!root.path().join("missing").exists());
}

#[test]
fn no_replace_vector_reports_the_failing_pair_after_a_successful_prefix() {
    let t = tempfile::tempdir().unwrap();
    let fs = Mounted::new(t.path()).unwrap();
    std::fs::write(t.path().join("a"), b"a").unwrap();
    std::fs::write(t.path().join("b"), b"b").unwrap();
    std::fs::write(t.path().join("exists"), b"old").unwrap();
    let error = fs
        .vrename(&[("/a", "/x"), ("/b", "/exists")], RenameOptions::NoReplace)
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(std::fs::read(t.path().join("x")).unwrap(), b"a");
    assert_eq!(std::fs::read(t.path().join("b")).unwrap(), b"b");
    assert_eq!(std::fs::read(t.path().join("exists")).unwrap(), b"old");
}

#[test]
fn partial_cross_device_rename_reconciles_without_replaying_prefix() {
    let (t, fs) = fixture();
    fs.fault.set(Fault::PartialCrossDevice);
    let result = move_items(
        &fs,
        &["/src/a", "/src/empty", "/src/nested"],
        "/out",
        CopyOptions::new().existing(Existing::Replace),
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
        CopyOptions::new().existing(Existing::Replace),
    );
    assert!(result.unwrap_err().is_transport());
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
        CopyOptions::new().existing(Existing::Skip),
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
        assert!(move_items(&fs, &["/src/a", "/src/empty"], "/out", CopyOptions::new()).is_err());
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
