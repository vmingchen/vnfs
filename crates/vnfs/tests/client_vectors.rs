#![cfg(all(feature = "auto", target_os = "linux"))]
use vnfs::directory::{Attributes, AttrsOptions};
use vnfs::files::{OpenFlags, OpenOp, Vfsi, VfsiExt, WriteOp};

fn writes<C: Vfsi>(fs: &C) {
    let files = fs
        .vopen(&[
            OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
            OpenOp::new("/b", OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE),
        ])
        .unwrap();
    let payload = b"payload";
    let op = WriteOp::at(&files[0], 3, payload);
    assert!(std::ptr::eq(op.file(), &files[0]));
    assert_eq!(op.data().as_ptr(), payload.as_ptr());
    assert_eq!(op.offset(), 3);
    let results = fs
        .vwrite(&[op, WriteOp::at(&files[1], 0, b"abc")], Default::default())
        .unwrap();
    assert_eq!(
        results
            .iter()
            .map(|r| (r.offset, r.bytes_written))
            .collect::<Vec<_>>(),
        [(3, 7), (0, 3)]
    );
    assert_eq!(
        std::io::Seek::stream_position(&mut fs.std_io(&files[0])).unwrap(),
        0
    );
    fs.vwrite(
        &[
            WriteOp::at(&files[1], 0, b"first"),
            WriteOp::at(&files[1], 2, b"XX"),
        ],
        vnfs::files::WriteOptions::new().with_write_all(true),
    )
    .unwrap();
    assert_eq!(fs.read_files(&["/b"]).unwrap(), [b"fiXXt".to_vec()]);
    fs.vwrite(
        &[WriteOp::at(&files[0], 0, b"")],
        vnfs::files::WriteOptions::new().with_write_all(true),
    )
    .unwrap();
    fs.close_files(files).unwrap();
}

#[test]
fn portable_writes_on_mounted_and_auto() {
    for auto in [false, true] {
        let root = tempfile::tempdir().unwrap();
        if auto {
            writes(&vnfs::mounted::Auto::new(root.path()).unwrap());
        } else {
            writes(&vnfs::posix::Posix::new(root.path()).unwrap());
        }
    }
}

#[test]
fn complete_writes_reject_the_entire_invalid_batch_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let fs = vnfs::posix::Posix::new(root.path()).unwrap();
    let other = vnfs::posix::Posix::new(root.path()).unwrap();
    let file = fs.create("/a").unwrap();
    fs.vwrite(
        &[WriteOp::at(&file, 0, b"keep")],
        vnfs::files::WriteOptions::new().with_write_all(true),
    )
    .unwrap();
    let foreign = other.create("/b").unwrap();
    let error = fs
        .vwrite(
            &[
                WriteOp::at(&file, 0, b"bad!"),
                WriteOp::at(&foreign, 0, b""),
            ],
            vnfs::files::WriteOptions::new().with_write_all(true),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let error = fs
        .vwrite(
            &[
                WriteOp::at(&file, 0, b"bad!"),
                WriteOp::at(&file, u64::MAX, b"xx"),
            ],
            vnfs::files::WriteOptions::new().with_write_all(true),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(fs.read_files(&["/a"]).unwrap(), [b"keep".to_vec()]);
    assert_eq!(error.operation(), Some("vwrite_native"));
    assert_eq!(error.path(), Some(std::path::Path::new("/a")));
    assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
    let mut closed = fs.create("/closed").unwrap();
    closed.try_close().unwrap();
    assert_eq!(
        fs.vwrite(
            &[WriteOp::at(&file, 0, b"bad!"), WriteOp::at(&closed, 0, b"")],
            vnfs::files::WriteOptions::new().with_write_all(true)
        )
        .unwrap_err()
        .index(),
        Some(1)
    );
    assert_eq!(fs.read_files(&["/a"]).unwrap(), [b"keep".to_vec()]);
}

fn attrs_query<C: Vfsi>(fs: &C) {
    assert!(
        fs.attrs_with_options("/link", AttrsOptions::new())
            .unwrap()
            .is_file()
    );
    assert!(
        fs.attrs_with_options(
            "/link",
            AttrsOptions::new().with_follow_final_symlink(false)
        )
        .unwrap()
        .is_symlink()
    );
    let scalar = fs
        .attrs_with_options("/a", AttrsOptions::new().with_fields(Attributes::FILEID))
        .unwrap();
    assert!(scalar.file_id().is_some());
    assert_eq!(scalar.len(), None);
    let follow = fs
        .vgetattrs(&["/link", "/a"], vnfs::directory::AttrsOptions::new())
        .unwrap();
    assert!(follow.iter().all(|m| m.is_file() && m.len() == Some(3)));
    let links = fs
        .vgetattrs(
            &["/a", "/link", "/dangling"],
            AttrsOptions::new().with_follow_final_symlink(false),
        )
        .unwrap();
    assert!(links[0].is_file());
    assert!(links[1].is_symlink());
    assert!(links[2].is_symlink());
    assert!(fs.attrs("/link").unwrap().is_file());
    assert!(fs.symlink_attrs("/link").unwrap().is_symlink());
    assert!(fs.attrs("/dangling").is_err());
    let partial = fs
        .vgetattrs(&["/a"], AttrsOptions::new().with_fields(Attributes::FILEID))
        .unwrap();
    assert!(partial[0].file_id().is_some());
    assert_eq!(
        partial[0].len(),
        None,
        "unrequested size must remain absent/default"
    );
    assert_eq!(
        fs.vgetattrs(&["/a", "/missing"], vnfs::directory::AttrsOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        fs.vgetattrs(&["/a", "/dangling"], vnfs::directory::AttrsOptions::new())
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert!(
        fs.vgetattrs::<&str>(&[], AttrsOptions::default())
            .unwrap()
            .is_empty()
    );
    assert!(
        fs.attrs_with_options(
            "/link",
            vnfs::directory::AttrsOptions::new()
                .with_fields(Attributes::MODE)
                .with_follow_final_symlink(false)
        )
        .unwrap()
        .is_symlink()
    );
}

#[test]
fn consolidated_metadata_fields_and_symlinks_on_mounted_and_auto() {
    for auto in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"abc").unwrap();
        std::os::unix::fs::symlink("a", root.path().join("link")).unwrap();
        std::os::unix::fs::symlink("missing", root.path().join("dangling")).unwrap();
        if auto {
            attrs_query(&vnfs::mounted::Auto::new(root.path()).unwrap());
        } else {
            attrs_query(&vnfs::posix::Posix::new(root.path()).unwrap());
        }
    }
}

struct ChangingTarget<'a, F> {
    first: vnfs::files::Target<'a, F>,
    later: vnfs::files::Target<'a, F>,
    calls: std::cell::Cell<usize>,
}
impl<'a, F> ChangingTarget<'a, F> {
    fn new(first: vnfs::files::Target<'a, F>, later: vnfs::files::Target<'a, F>) -> Self {
        Self {
            first,
            later,
            calls: std::cell::Cell::new(0),
        }
    }
}
impl<F> vnfs::files::AsTarget<F> for ChangingTarget<'_, F> {
    fn as_target(&self) -> vnfs::files::Target<'_, F> {
        let calls = self.calls.replace(self.calls.get() + 1);
        if calls == 0 { self.first } else { self.later }
    }
}

fn stable_targets<C: Vfsi>(fs: &C, other: &C) {
    use vnfs::directory::SetAttrsOp;
    use vnfs::files::{FileHandle, Target};
    let file = fs.create("/original").unwrap();
    fs.vwrite(&[WriteOp::at(&file, 0, b"original")], Default::default())
        .unwrap();
    let foreign = other.create("/foreign").unwrap();
    let targets = [ChangingTarget::new(
        Target::File(&file),
        Target::File(&foreign),
    )];
    assert_eq!(
        fs.vgetattrs(&targets, AttrsOptions::new()).unwrap()[0].len(),
        Some(8)
    );
    assert_eq!(targets[0].calls.get(), 1);
    targets[0].calls.set(0);
    assert_eq!(fs.vstatfs(&targets).unwrap().len(), 1);
    assert_eq!(targets[0].calls.get(), 1);
    let updates = [SetAttrsOp::new(ChangingTarget::new(
        Target::File(&file),
        Target::File(&foreign),
    ))
    .len(2)];
    fs.vsetattrs(&updates).unwrap();
    assert_eq!(updates[0].target().calls.get(), 1);
    assert_eq!(fs.attrs("/original").unwrap().len(), Some(2));
    assert_eq!(other.attrs("/foreign").unwrap().len(), Some(0));

    // Error context must use the same prepared operand, too.
    let missing = std::path::Path::new("/missing");
    let targets = [ChangingTarget::new(
        Target::Path(missing),
        Target::File(&file),
    )];
    let error = fs.vgetattrs(&targets, AttrsOptions::new()).unwrap_err();
    assert_eq!(error.path(), Some(missing));
    assert_eq!(targets[0].calls.get(), 1);
    targets[0].calls.set(0);
    let error = fs.vstatfs(&targets).unwrap_err();
    assert_eq!(error.path(), Some(missing));
    assert_eq!(targets[0].calls.get(), 1);
    let updates = [SetAttrsOp::new(ChangingTarget::new(
        Target::Path(missing),
        Target::File(&file),
    ))
    .len(0)];
    let error = fs.vsetattrs(&updates).unwrap_err();
    assert_eq!(error.path(), Some(missing));
    assert_eq!(updates[0].target().calls.get(), 1);
    assert_eq!(fs.attrs("/original").unwrap().len(), Some(2));
    file.close().unwrap();
    foreign.close().unwrap();
}

#[test]
fn custom_targets_are_prepared_once_on_mounted_and_auto() {
    for auto in [false, true] {
        let root = tempfile::tempdir().unwrap();
        if auto {
            stable_targets(
                &vnfs::mounted::Auto::new(root.path()).unwrap(),
                &vnfs::mounted::Auto::new(root.path()).unwrap(),
            );
        } else {
            stable_targets(
                &vnfs::posix::Posix::new(root.path()).unwrap(),
                &vnfs::posix::Posix::new(root.path()).unwrap(),
            );
        }
    }
}

#[test]
fn path_contents_removal_works_without_retained_directory_handles() {
    fn check(fs: &impl Vfsi) {
        use vnfs::directory::RemoveOptions;
        fs.create_dir("/dir").unwrap();
        fs.write("/dir/keep", b"keep").unwrap();
        assert!(fs.vopen_dirs::<&str>(&[]).unwrap().is_empty());
        assert_eq!(
            fs.vopen_dirs(&["/dir"]).err().unwrap().kind(),
            vnfs::error::ErrorKind::Unsupported
        );
        fs.vremove_dir_contents(&[], Default::default()).unwrap();
        assert_eq!(fs.read("/dir/keep").unwrap(), b"keep");

        fs.create_dir_all("/dir/sub").unwrap();
        fs.write("/dir/sub/file", b"nested").unwrap();
        fs.create_dir("/outside").unwrap();
        fs.write("/outside/keep", b"outside").unwrap();
        fs.symlink("../outside", "/dir/link").unwrap();
        fs.symlink("dir", "/dir-link").unwrap();
        for path in ["/dir-link", "/dir/keep"] {
            assert_eq!(
                fs.remove_dir_contents(path).unwrap_err().kind(),
                vnfs::error::ErrorKind::NotADirectory
            );
            assert_eq!(fs.read("/dir/keep").unwrap(), b"keep");
        }
        fs.remove_dir_contents("/dir").unwrap();
        assert!(fs.attrs("/dir").unwrap().is_dir());
        assert!(fs.read_dir("/dir").unwrap().is_empty());
        assert_eq!(fs.read("/outside/keep").unwrap(), b"outside");

        fs.write("/dir/file", b"original").unwrap();
        fs.rename("/dir", "/moved").unwrap();
        fs.create_dir("/dir").unwrap();
        fs.write("/dir/file", b"replacement").unwrap();
        fs.remove_dir_contents("/dir").unwrap();
        assert!(fs.read_dir("/dir").unwrap().is_empty());
        assert_eq!(fs.read("/moved/file").unwrap(), b"original");
        fs.remove_dir_contents_with_options("/moved", RemoveOptions::default())
            .unwrap();
        assert!(fs.attrs("/moved").unwrap().is_dir());
        assert!(fs.read_dir("/moved").unwrap().is_empty());
        assert_eq!(fs.read("/outside/keep").unwrap(), b"outside");
    }
    for auto in [false, true] {
        let root = tempfile::tempdir().unwrap();
        if auto {
            check(&vnfs::mounted::Auto::new(root.path()).unwrap());
        } else {
            check(&vnfs::posix::Posix::new(root.path()).unwrap());
        }
    }
}
