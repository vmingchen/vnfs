use std::path::{Path, PathBuf};
use vnfs::directory::{Attributes, AttrsOptions};
use vnfs::files::{Capabilities, Vfsi, VfsiExt};

pub fn check_links_and_modes(fs: &impl Vfsi, directory: &str) {
    check_rename_results(fs, directory);
    check_copy_results(fs, directory);
    assert!(
        fs.capabilities().unwrap().contains(
            Capabilities::SYMLINKS | Capabilities::HARDLINKS | Capabilities::POSIX_METADATA
        )
    );
    fs.vmkdir::<&str>(&[]).unwrap();
    fs.symlinks_all::<&str, &str>(&[]).unwrap();
    fs.hardlinks_all::<&str, &str>(&[]).unwrap();
    assert!(fs.vreadlink::<&str>(&[]).unwrap().is_empty());

    let directories: Vec<_> = (0..64)
        .map(|i| vnfs::directory::MkDirOp::new(format!("{directory}/dir-{i}"), 0o700 | (i % 8)))
        .collect();
    fs.vmkdir(&directories)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap();
    let paths: Vec<_> = directories.iter().map(|op| op.path()).collect();
    let attrs = fs
        .getattrs_all(&paths, AttrsOptions::new().with_fields(Attributes::MODE))
        .unwrap();
    assert_eq!(attrs.len(), directories.len());
    for (attrs, op) in attrs.iter().zip(&directories) {
        assert!(attrs.is_dir());
        assert_eq!(attrs.permissions().unwrap().mode() & 0o7777, op.mode());
    }
    fs.create_dir_with_mode(format!("{directory}/scalar-dir"), 0o751)
        .unwrap();
    assert_eq!(
        fs.attrs(format!("{directory}/scalar-dir"))
            .unwrap()
            .permissions()
            .unwrap()
            .mode()
            & 0o7777,
        0o751
    );

    let files: Vec<_> = (0..64).map(|i| format!("{directory}/file-{i}")).collect();
    let writes: Vec<_> = files
        .iter()
        .map(|path| (path, b"contents".as_slice()))
        .collect();
    fs.write_files(&writes).unwrap();
    let links: Vec<_> = (0..64)
        .map(|i| format!("{directory}/symbolic-{i}"))
        .collect();
    let targets: Vec<_> = (0..64).map(|i| format!("file-{i}")).collect();
    let requests: Vec<_> = targets.iter().zip(&links).collect();
    fs.symlinks_all(&requests).unwrap();
    let read = fs.readlinks_all(&links).unwrap();
    assert_eq!(read, targets.iter().map(PathBuf::from).collect::<Vec<_>>());
    for attrs in fs
        .getattrs_all(&links, AttrsOptions::new().with_follow_final_symlink(false))
        .unwrap()
    {
        assert!(attrs.is_symlink());
    }

    let hardlinks: Vec<_> = (0..64).map(|i| format!("{directory}/hard-{i}")).collect();
    let requests: Vec<_> = files.iter().zip(&hardlinks).collect();
    fs.hardlinks_all(&requests).unwrap();
    let attrs = fs
        .getattrs_all(
            &files,
            AttrsOptions::new().with_fields(Attributes::FILEID | Attributes::NLINK),
        )
        .unwrap();
    let linked = fs
        .getattrs_all(
            &hardlinks,
            AttrsOptions::new().with_fields(Attributes::FILEID | Attributes::NLINK),
        )
        .unwrap();
    for (source, target) in attrs.iter().zip(&linked) {
        assert!(source.file_id().is_some());
        assert_eq!(source.file_id(), target.file_id());
        assert_eq!(source.nlink(), Some(2));
    }
    assert_eq!(
        fs.read_files(&hardlinks).unwrap(),
        vec![b"contents".to_vec(); 64]
    );

    // Hard-linking a symlink must link the symlink object rather than its target.
    let symlink_hard = format!("{directory}/hard-symbolic");
    fs.hard_link(&links[0], &symlink_hard).unwrap();
    assert!(fs.symlink_attrs(&symlink_hard).unwrap().is_symlink());
    assert_eq!(fs.read_link(&symlink_hard).unwrap(), Path::new("file-0"));

    // Targets are data: retain non-UTF-8, relative, absolute and dangling text.
    use std::os::unix::ffi::OsStringExt;
    let raw = PathBuf::from(std::ffi::OsString::from_vec(b"../dangling-\xff".to_vec()));
    let raw_link = format!("{directory}/raw-link");
    let absolute_link = format!("{directory}/absolute-link");
    fs.symlinks_all(&[
        (&raw, &raw_link),
        (&PathBuf::from("/missing/../target"), &absolute_link),
    ])
    .unwrap();
    assert_eq!(
        fs.readlinks_all(&[&raw_link, &absolute_link]).unwrap(),
        [raw.clone(), PathBuf::from("/missing/../target")]
    );

    // Native errors identify the original vector item, without replaying.
    let fresh = format!("{directory}/fresh-symbolic");
    let error = fs
        .symlinks_all(&[("file-0", &fresh), ("file-1", &links[1])])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let fresh_hard = format!("{directory}/fresh-hard");
    let error = fs
        .hardlinks_all(&[(&files[0], &fresh_hard), (&files[1], &hardlinks[1])])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let error = fs.readlinks_all(&[&links[0], &files[0]]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    // Rich results retain both sides of failures, including unchanged raw bytes.
    let missing = format!("{directory}/missing-link");
    let inputs = [&links[0], &files[0], &raw_link, &missing, &absolute_link];
    let outcomes = fs.vreadlink(&inputs).unwrap();
    assert_eq!(outcomes.len(), inputs.len());
    for (index, expected) in [
        (0, PathBuf::from("file-0")),
        (2, raw.clone()),
        (4, PathBuf::from("/missing/../target")),
    ] {
        assert_eq!(outcomes[index].as_ref().unwrap(), &expected);
    }
    for (index, kind) in [
        (1, std::io::ErrorKind::InvalidInput),
        (3, std::io::ErrorKind::NotFound),
    ] {
        let error = outcomes[index].as_ref().unwrap_err();
        assert_eq!(error.state(), vnfs::error::FailureState::Failed);
        assert_eq!(error.cause().kind(), kind);
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(Path::new(inputs[index])));
        assert!(error.progress().is_none());
    }

    // A failed creation cannot suppress independent requests on either side.
    let new_symbols = [
        format!("{directory}/mixed-symbol-first"),
        format!("{directory}/mixed-symbol-last"),
    ];
    let missing_link_parent = format!("{directory}/missing-parent/link");
    let symbol_ops = [
        (raw.as_path(), new_symbols[0].as_str()),
        (Path::new("collision"), links[1].as_str()),
        (Path::new("missing-parent"), missing_link_parent.as_str()),
        (Path::new("last-target"), new_symbols[1].as_str()),
    ];
    let outcomes = fs.vsymlink(&symbol_ops).unwrap();
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes[0].is_ok() && outcomes[3].is_ok());
    for (index, kind) in [
        (1, std::io::ErrorKind::AlreadyExists),
        (2, std::io::ErrorKind::NotFound),
    ] {
        let error = outcomes[index].as_ref().unwrap_err();
        assert_ne!(error.state(), vnfs::error::FailureState::Unknown);
        assert_eq!(error.cause().kind(), kind);
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(Path::new(symbol_ops[index].1)));
        assert!(error.progress().is_none());
    }
    assert_eq!(fs.read_link(&new_symbols[0]).unwrap(), raw);
    assert_eq!(
        fs.read_link(&new_symbols[1]).unwrap(),
        Path::new("last-target")
    );
    let new_hard = [
        format!("{directory}/mixed-hard-first"),
        format!("{directory}/mixed-hard-last"),
    ];
    let missing_source = format!("{directory}/missing-source");
    let rejected_hard = format!("{directory}/rejected-hard");
    let hard_ops = [
        (files[0].as_str(), new_hard[0].as_str()),
        (files[1].as_str(), hardlinks[1].as_str()),
        (missing_source.as_str(), rejected_hard.as_str()),
        (files[2].as_str(), new_hard[1].as_str()),
    ];
    let outcomes = fs.vhardlink(&hard_ops).unwrap();
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes[0].is_ok() && outcomes[3].is_ok());
    for (index, kind) in [
        (1, std::io::ErrorKind::AlreadyExists),
        (2, std::io::ErrorKind::NotFound),
    ] {
        let error = outcomes[index].as_ref().unwrap_err();
        assert_ne!(error.state(), vnfs::error::FailureState::Unknown);
        assert_eq!(error.cause().kind(), kind);
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(Path::new(hard_ops[index].1)));
        assert!(error.progress().is_none());
    }
    assert_eq!(
        fs.read_files(&new_hard).unwrap(),
        vec![b"contents".to_vec(); 2]
    );
    assert_eq!(
        fs.symlink_attrs(&rejected_hard).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let new_dir = format!("{directory}/fresh-dir");
    let error = fs
        .vmkdir(&[
            vnfs::directory::MkDirOp::new(std::path::Path::new(&new_dir), 0o711),
            vnfs::directory::MkDirOp::new(directories[0].path(), 0o755),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs.attrs(&new_dir).unwrap().permissions().unwrap().mode() & 0o7777,
        0o711
    );
    let duplicate = format!("{directory}/duplicate-dir");
    let error = fs
        .vmkdir(&[
            vnfs::directory::MkDirOp::new(&duplicate, 0o700),
            vnfs::directory::MkDirOp::new(&duplicate, 0o755),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(
        fs.attrs(&duplicate).unwrap().permissions().unwrap().mode() & 0o7777,
        0o700,
        "duplicate rejection does not suppress the first independent create"
    );
    let first = format!("{directory}/mixed-dir-first");
    let last = format!("{directory}/mixed-dir-last");
    let missing = format!("{directory}/missing-dir-parent/child");
    let dirs = [
        vnfs::directory::MkDirOp::new(first.as_str(), 0o700),
        vnfs::directory::MkDirOp::new(new_dir.as_str(), 0o755),
        vnfs::directory::MkDirOp::new(missing.as_str(), 0o700),
        vnfs::directory::MkDirOp::new(last.as_str(), 0o751),
    ];
    let results = fs.vmkdir(&dirs).unwrap();
    assert_eq!(results.len(), dirs.len());
    assert!(results[0].is_ok() && results[3].is_ok());
    for (index, kind) in [
        (1, std::io::ErrorKind::AlreadyExists),
        (2, std::io::ErrorKind::NotFound),
    ] {
        let error = results[index].as_ref().unwrap_err();
        assert_ne!(error.state(), vnfs::error::FailureState::Unknown);
        assert_eq!(error.cause().kind(), kind);
        assert_eq!(error.cause().index(), None);
        assert_eq!(error.cause().path(), Some(dirs[index].path()));
    }
    for (path, mode) in [(&first, 0o700), (&last, 0o751)] {
        assert_eq!(
            fs.attrs(path).unwrap().permissions().unwrap().mode() & 0o7777,
            mode
        );
    }
}

fn check_rename_results(fs: &impl Vfsi, directory: &str) {
    use vnfs::{directory::RenameMode, error::FailureState};
    assert!(
        fs.vrename::<&str, &str>(&[], RenameMode::Replace)
            .unwrap()
            .is_empty()
    );
    let path = |name: &str| format!("{directory}/rename-{name}");
    let (first, last, missing) = (path("first"), path("last"), path("missing"));
    let (one, two, three) = (path("dst-first"), path("dst-missing"), path("dst-last"));
    fs.write_files(&[
        (&first, b"first".as_slice()),
        (&last, b"last".as_slice()),
        (&one, b"old".as_slice()),
    ])
    .unwrap();
    let pairs = [(&first, &one), (&missing, &two), (&last, &three)];
    let results = fs.vrename(&pairs, RenameMode::Replace).unwrap();
    assert_eq!(results.len(), 3);
    assert!(results[0].is_ok() && results[2].is_ok());
    let error = results[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().path(), Some(Path::new(&missing)));
    assert!(error.progress().is_none());
    assert_eq!(fs.read(&one).unwrap(), b"first");
    assert_eq!(fs.read(&three).unwrap(), b"last");
    assert_eq!(
        fs.attrs(&first).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        fs.attrs(&last).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    fs.write_files(&[(&first, b"again".as_slice()), (&last, b"later".as_slice())])
        .unwrap();
    let error = fs.renames_all(&pairs, RenameMode::Replace).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(fs.read(&one).unwrap(), b"again");
    assert_eq!(fs.read(&three).unwrap(), b"later");
    let link = path("link");
    let moved = path("moved-link");
    fs.symlink("rename-dst-last", &link).unwrap();
    fs.rename(&link, &moved).unwrap();
    assert!(fs.symlink_attrs(&moved).unwrap().is_symlink());
    assert_eq!(fs.read_link(&moved).unwrap(), Path::new("rename-dst-last"));
    assert_eq!(fs.read(&three).unwrap(), b"later");
}

fn check_copy_results(fs: &impl Vfsi, directory: &str) {
    use vnfs::{error::FailureState, files::CopyOptions};
    let options = CopyOptions::new();
    assert!(fs.vcopy::<&str, &str>(&[], options).unwrap().is_empty());
    let path = |name| format!("{directory}/copy-{name}");
    let (first, missing, last) = (path("first"), path("missing"), path("last"));
    let (one, two, three) = (path("one"), path("two"), path("three"));
    fs.write_files(&[
        (&first, b"one".as_slice()),
        (&last, b"three".as_slice()),
        (&one, b"stale tail".as_slice()),
    ])
    .unwrap();
    let pairs = [(&first, &one), (&missing, &two), (&last, &three)];
    let results = fs.vcopy(&pairs, options).unwrap();
    assert_eq!(results.len(), 3);
    assert!(results[0].is_ok() && results[2].is_ok());
    let error = results[1].as_ref().unwrap_err();
    assert!(matches!(
        error.state(),
        FailureState::Failed | FailureState::NotAttempted
    ));
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().path(), Some(Path::new(&two)));
    assert!(error.progress().is_none());
    assert_eq!(fs.read(&one).unwrap(), b"one");
    assert_eq!(fs.read(&three).unwrap(), b"three");
    assert_eq!(fs.read(&first).unwrap(), b"one");
    assert_eq!(fs.read(&last).unwrap(), b"three");
    let error = fs.copies_all(&pairs, options).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(fs.read(&three).unwrap(), b"three");
    fs.copy(&first, &one).unwrap();
}
