#![cfg(all(feature = "posix", unix))]

use std::{
    cell::RefCell,
    ops::ControlFlow,
    path::{Path, PathBuf},
};
use vnfs::{directory::*, error::*, files::*, helpers::*, posix::*};

#[derive(Default)]
struct Calls {
    opens: Vec<usize>,
    reads: Vec<usize>,
    writes: Vec<usize>,
    copies: Vec<usize>,
}
struct Counted {
    fs: Posix,
    calls: RefCell<Calls>,
}
impl Vfsi for Counted {
    type File = <Posix as Vfsi>::File;
    type Dir = <Posix as Vfsi>::Dir;
    fn capabilities(&self) -> Result<Capabilities> {
        self.fs.capabilities()
    }
    fn limits(&self) -> ResourceLimits {
        self.fs.limits()
    }
    fn vstatfs<P: AsTarget<Self::File>>(&self, p: &[P]) -> Result<Vec<FilesystemStats>> {
        self.fs.vstatfs(p)
    }
    fn vgetattrs<P: AsTarget<Self::File>>(&self, p: &[P], o: AttrsOptions) -> Result<Vec<Attrs>> {
        self.fs.vgetattrs(p, o)
    }
    fn vsetattrs<P: AsTarget<Self::File>>(&self, p: &[SetAttrsOp<P>]) -> Result<()> {
        self.fs.vsetattrs(p)
    }
    fn vfsync(&self, p: &[&Self::File], o: SyncMode) -> Result<()> {
        self.fs.vfsync(p, o)
    }
    fn vsymlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, p: &[(P, Q)]) -> Result<()> {
        self.fs.vsymlink(p)
    }
    fn vreadlink<P: AsRef<Path>>(&self, p: &[P]) -> Result<Vec<PathBuf>> {
        self.fs.vreadlink(p)
    }
    fn vhardlink<P: AsRef<Path>, Q: AsRef<Path>>(&self, p: &[(P, Q)]) -> Result<()> {
        self.fs.vhardlink(p)
    }
    fn vopen(&self, p: &[OpenOp]) -> Result<Vec<Self::File>> {
        self.calls.borrow_mut().opens.push(p.len());
        self.fs.vopen(p)
    }
    fn vread<'a>(
        &self,
        p: impl IntoIterator<Item = ReadOp<'a, Self::File>>,
        o: ReadOptions,
    ) -> Result<Vec<ReadResult>> {
        let p: Vec<_> = p.into_iter().collect();
        self.calls.borrow_mut().reads.push(p.len());
        self.fs.vread(p, o)
    }
    fn vwrite<'a>(
        &self,
        p: &[WriteOp<'a, Self::File>],
        o: WriteOptions,
    ) -> Result<Vec<WriteResult>> {
        self.calls.borrow_mut().writes.push(p.len());
        self.fs.vwrite(p, o)
    }
    fn vclose(&self, p: &mut [Self::File]) -> Result<()> {
        self.fs.vclose(p)
    }
    fn vmkdir<P: AsRef<Path>>(&self, p: &[MkDirOp<P>]) -> Result<()> {
        self.fs.vmkdir(p)
    }
    fn vcopy<P: AsRef<Path>, Q: AsRef<Path>>(&self, p: &[(P, Q)], o: CopyOption) -> Result<()> {
        self.calls.borrow_mut().copies.push(p.len());
        self.fs.vcopy(p, o)
    }
    fn vremove<P: AsRef<Path>>(&self, p: &[P], m: RemoveMode, o: RemoveOptions) -> Result<()> {
        self.fs.vremove(p, m, o)
    }
    fn vrename<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        p: &[(P, Q)],
        o: RenameOptions,
    ) -> Result<()> {
        self.fs.vrename(p, o)
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
        c: impl FnMut(usize, u64, &[u8]) -> Result<ControlFlow<()>>,
    ) -> Result<Vec<StreamCompletion>> {
        self.fs.vstream(p, o, c)
    }
}
fn fixture() -> (tempfile::TempDir, Counted) {
    let root = tempfile::tempdir().unwrap();
    for dir in ["left/nested", "right/nested"] {
        std::fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    for (path, data) in [
        ("left/same", b"first".as_slice()),
        ("right/same", b"second"),
        ("left/nested/item", b"left nested"),
        ("right/nested/item", b"right nested"),
    ] {
        std::fs::write(root.path().join(path), data).unwrap();
    }
    let fs = Counted {
        fs: Posix::new(root.path()).unwrap(),
        calls: RefCell::new(Calls::default()),
    };
    (root, fs)
}

#[test]
fn contents_replace_colliding_regular_roots_in_input_order() {
    for batch in [1, 64] {
        for preexisting in [false, true] {
            for preserve in [false, true] {
                let (root, fs) = fixture();
                if preexisting {
                    std::fs::create_dir(root.path().join("out")).unwrap();
                    std::fs::write(root.path().join("out/same"), b"old").unwrap();
                }
                let summary = copy_items(
                    &fs,
                    &["/left/same", "/right/same"],
                    "/out",
                    CopyOptions::new()
                        .layout(CopyLayout::Contents)
                        .existing(Existing::Replace)
                        .batch_size(batch)
                        .preserve_permissions(preserve),
                )
                .unwrap();
                assert_eq!(
                    std::fs::read(root.path().join("out/same")).unwrap(),
                    b"second"
                );
                assert_eq!(summary.files_copied, 2);
                assert_eq!(
                    std::fs::read(root.path().join("left/same")).unwrap(),
                    b"first"
                );
            }
        }
    }
}

#[test]
fn contents_conflicts_error_or_skip_after_first_completed_root() {
    for batch in [1, 64] {
        for policy in [Existing::Error, Existing::Skip] {
            let (root, fs) = fixture();
            let result = copy_items(
                &fs,
                &["/left/same", "/right/same"],
                "/out",
                CopyOptions::new()
                    .layout(CopyLayout::Contents)
                    .existing(policy)
                    .batch_size(batch),
            );
            if policy == Existing::Error {
                assert_eq!(result.unwrap_err().index(), Some(1));
            } else {
                let summary = result.unwrap();
                assert_eq!(summary.files_copied, 1);
                assert_eq!(summary.entries_skipped, 1);
            }
            assert_eq!(
                std::fs::read(root.path().join("out/same")).unwrap(),
                b"first"
            );
        }
    }
}

#[test]
fn contents_directory_roots_complete_before_later_regular_roots() {
    for batch in [1, 64] {
        for reverse in [false, true] {
            let (root, fs) = fixture();
            let sources = if reverse {
                ["/right/same", "/left"]
            } else {
                ["/left", "/right/same"]
            };
            let summary = copy_items(
                &fs,
                &sources,
                "/out",
                CopyOptions::new()
                    .layout(CopyLayout::Contents)
                    .existing(Existing::Replace)
                    .batch_size(batch),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(root.path().join("out/same")).unwrap(),
                if reverse {
                    b"first".as_slice()
                } else {
                    b"second"
                }
            );
            assert_eq!(
                std::fs::read(root.path().join("out/nested/item")).unwrap(),
                b"left nested"
            );
            assert_eq!(summary.files_copied, 3);
        }
    }
}

#[test]
fn contents_directory_collisions_follow_root_order_at_every_depth() {
    for batch in [1, 64] {
        let (root, fs) = fixture();
        let summary = copy_items(
            &fs,
            &["/left", "/right"],
            "/out",
            CopyOptions::new()
                .layout(CopyLayout::Contents)
                .existing(Existing::Replace)
                .batch_size(batch),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("out/same")).unwrap(),
            b"second"
        );
        assert_eq!(
            std::fs::read(root.path().join("out/nested/item")).unwrap(),
            b"right nested"
        );
        assert_eq!(summary.files_copied, 4);
    }
}

#[test]
fn disjoint_regular_roots_keep_vector_io_in_both_layouts() {
    for layout in [CopyLayout::Container, CopyLayout::Contents] {
        for native in [false, true] {
            let (root, fs) = fixture();
            let summary = copy_items(
                &fs,
                &["/left/same", "/right/nested/item"],
                "/out",
                CopyOptions::new().layout(layout).existing(if native {
                    Existing::Replace
                } else {
                    Existing::Error
                }),
            )
            .unwrap();
            assert_eq!(summary.files_copied, 2);
            assert_eq!(
                std::fs::read(root.path().join("out/same")).unwrap(),
                b"first"
            );
            assert_eq!(
                std::fs::read(root.path().join("out/item")).unwrap(),
                b"right nested"
            );
            let calls = fs.calls.borrow();
            if native {
                assert_eq!(calls.copies, [2]);
                assert_eq!(calls.opens, [2]);
            } else {
                assert_eq!(calls.opens, [2, 2]);
                assert_eq!(calls.reads, [2]);
                assert_eq!(calls.writes, [2]);
            }
        }
    }
}

#[test]
fn contents_cohorts_share_traversal_budget() {
    let (root, fs) = fixture();
    let error = copy_items(
        &fs,
        &["/left", "/right"],
        "/out",
        CopyOptions::new()
            .layout(CopyLayout::Contents)
            .existing(Existing::Replace)
            .max_entries(5),
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(
        std::fs::read(root.path().join("out/same")).unwrap(),
        b"first"
    );
    assert_eq!(
        std::fs::read(root.path().join("out/nested/item")).unwrap(),
        b"left nested"
    );
}

#[test]
fn contents_cancellation_keeps_later_conflicting_root_unstarted() {
    let (root, fs) = fixture();
    let summary = copy_items_with_progress(
        &fs,
        &["/left/same", "/right/same"],
        "/out",
        CopyOptions::new()
            .layout(CopyLayout::Contents)
            .existing(Existing::Replace),
        |_| Ok(ControlFlow::Break(())),
    )
    .unwrap();
    assert!(summary.stopped);
    assert_eq!(summary.files_copied, 1);
    assert_eq!(
        std::fs::read(root.path().join("out/same")).unwrap(),
        b"first"
    );
    assert_eq!(fs.calls.borrow().opens, [1, 1]);
}

#[test]
fn contents_collision_splits_only_the_necessary_file_cohort() {
    let (root, fs) = fixture();
    let summary = copy_items(
        &fs,
        &["/left/same", "/right/nested/item", "/right/same"],
        "/out",
        CopyOptions::new()
            .layout(CopyLayout::Contents)
            .existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(summary.files_copied, 3);
    assert_eq!(
        std::fs::read(root.path().join("out/same")).unwrap(),
        b"second"
    );
    assert_eq!(
        std::fs::read(root.path().join("out/item")).unwrap(),
        b"right nested"
    );
    assert_eq!(fs.calls.borrow().copies, [2, 1]);
    assert_eq!(fs.calls.borrow().opens, [2, 1]);
}
