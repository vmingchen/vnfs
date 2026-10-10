use vnfs::directory::{Attributes, AttrsOptions, Permissions};
use vnfs::files::{Vfsi, VfsiExt};

pub fn check_many(fs: &impl Vfsi, directory: &str) {
    fs.vsetattrs::<&str>(&[]).unwrap();
    let paths: Vec<_> = (0..96).map(|i| format!("{directory}/file-{i}")).collect();
    let writes: Vec<_> = paths
        .iter()
        .map(|path| (path, b"abcdefgh".as_slice()))
        .collect();
    fs.write_files(&writes).unwrap();
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let updates: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            vnfs::directory::SetAttrsOp::new(path)
                .permissions(Permissions::from_mode(0o600 | ((i % 8) as u32)))
                .len(if i % 2 == 0 { 3 } else { 100 + i as u64 })
                .modified(modified)
        })
        .collect();
    fs.vsetattrs(&updates).unwrap();
    let options =
        AttrsOptions::new().fields(Attributes::MODE | Attributes::SIZE | Attributes::MTIME);
    let metadata = fs.vgetattrs(&paths, options).unwrap();
    assert_eq!(metadata.len(), paths.len());
    for (i, item) in metadata.iter().enumerate() {
        assert_eq!(
            item.permissions().unwrap().mode() & 0o7777,
            0o600 | ((i % 8) as u32)
        );
        assert_eq!(
            item.len(),
            Some(if i % 2 == 0 { 3 } else { 100 + i as u64 })
        );
        assert_eq!(item.modified(), Some(modified));
    }
    // Size-only mutations preserve permissions and retain zero as a valid size.
    let sizes: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            vnfs::directory::SetAttrsOp::new(path)
                .len(i as u64)
                .follow_symlinks(false)
        })
        .collect();
    fs.vsetattrs(&sizes).unwrap();
    let metadata = fs.vgetattrs(&paths, options).unwrap();
    for (i, item) in metadata.iter().enumerate() {
        assert_eq!(item.len(), Some(i as u64));
        assert_eq!(
            item.permissions().unwrap().mode() & 0o7777,
            0o600 | ((i % 8) as u32)
        );
    }
    let missing = format!("{directory}/missing");
    let error = fs
        .vsetattrs(&[
            vnfs::directory::SetAttrsOp::new(&paths[0]).len(7),
            vnfs::directory::SetAttrsOp::new(&missing).len(7),
            vnfs::directory::SetAttrsOp::new(&paths[1]).len(7),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::ENOENT as u32);
    check_mixed_policies(fs, directory);
}

fn check_mixed_policies(fs: &impl Vfsi, directory: &str) {
    use vnfs::directory::SetAttrsOp;
    let first = format!("{directory}/mixed-first");
    let last = format!("{directory}/mixed-last");
    let link = format!("{directory}/mixed-link");
    fs.write_files(&[(&first, b"abcdefghij"), (&last, b"abcdefghij")])
        .unwrap();
    fs.symlink("mixed-first", &link).unwrap();
    // A failing no-follow wave must not send the following wave.
    let error = fs
        .vsetattrs(&[
            SetAttrsOp::new(&first).len(2),
            SetAttrsOp::new(&link).len(4).follow_symlinks(false),
            SetAttrsOp::new(&last).len(1),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(fs.attrs(&first).unwrap().len().unwrap(), 2);
    assert_eq!(fs.attrs(&last).unwrap().len().unwrap(), 10);
    // Do not regroup nonadjacent equal policies: these updates alias one object.
    fs.vsetattrs(&[
        SetAttrsOp::new(&link).len(5),
        SetAttrsOp::new(&first).len(6).follow_symlinks(false),
        SetAttrsOp::new(&link).len(7),
    ])
    .unwrap();
    assert_eq!(fs.attrs(&first).unwrap().len().unwrap(), 7);
    assert!(fs.symlink_attrs(&link).unwrap().is_symlink());
    // Validation spans policy boundaries and must precede the first mutation.
    let error = fs
        .vsetattrs(&[
            SetAttrsOp::new(&first).len(1).follow_symlinks(false),
            SetAttrsOp::new(&last).uid(u32::MAX),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EINVAL as u32);
    assert_eq!(fs.attrs(&first).unwrap().len().unwrap(), 7);
}

/// Open objects remain targets even when their original names are reused.
pub fn check_handles(fs: &impl Vfsi, directory: &str) {
    use vnfs::files::{FileHandle, OpenFlags, OpenOp, Target};
    let paths: Vec<_> = (0..64).map(|i| format!("{directory}/handle-{i}")).collect();
    fs.write_files(
        &paths
            .iter()
            .map(|path| (path, b"original".as_slice()))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let requests: Vec<_> = paths
        .iter()
        .map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE))
        .collect();
    let mut files = fs.vopen(&requests).unwrap();
    let renamed: Vec<_> = paths.iter().map(|path| format!("{path}-moved")).collect();
    fs.vrename(
        &paths.iter().zip(&renamed).collect::<Vec<_>>(),
        vnfs::directory::RenameOptions::Replace,
    )
    .unwrap();
    fs.write_files(
        &paths
            .iter()
            .map(|path| (path, b"replacement".as_slice()))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let updates: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            vnfs::directory::SetAttrsOp::new(Target::File(file))
                .len(i as u64)
                .permissions(Permissions::from_mode(0o640))
                .follow_symlinks(false)
        })
        .collect();
    // no-follow must still address the opened objects.
    fs.vsetattrs(&updates).unwrap();
    for (i, file) in files.iter().enumerate() {
        let attrs = fs.attrs(Target::file(file)).unwrap();
        assert_eq!(attrs.len(), Some(i as u64));
        assert_eq!(attrs.permissions().unwrap().mode() & 0o7777, 0o640);
        assert_eq!(fs.attrs(&paths[i]).unwrap().len().unwrap(), 11);
    }
    fs.vsetattrs(&[
        vnfs::directory::SetAttrsOp::new(Target::File(&files[0])).len(13),
        vnfs::directory::SetAttrsOp::new(Target::Path(std::path::Path::new(&paths[0]))).len(17),
    ])
    .unwrap();
    assert_eq!(
        fs.attrs(Target::file(&files[0])).unwrap().len().unwrap(),
        13
    );
    assert_eq!(fs.attrs(&paths[0]).unwrap().len().unwrap(), 17);
    fs.vsetattrs(&[vnfs::directory::SetAttrsOp::new(Target::File(&files[0])).len(19)])
        .unwrap();
    fs.chmod(Target::File(&files[0]), Permissions::from_mode(0o600))
        .unwrap();
    assert_eq!(
        fs.attrs(Target::file(&files[0])).unwrap().len().unwrap(),
        19
    );
    assert_eq!(
        fs.attrs(Target::file(&files[0]))
            .unwrap()
            .permissions()
            .unwrap()
            .mode()
            & 0o7777,
        0o600
    );
    files[1].try_close().unwrap();
    let error = fs
        .vsetattrs(&[
            vnfs::directory::SetAttrsOp::new(Target::File(&files[0])).len(5),
            vnfs::directory::SetAttrsOp::new(Target::File(&files[1])).len(5),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EBADF as u32);
    assert_eq!(
        fs.attrs(Target::file(&files[0])).unwrap().len().unwrap(),
        19
    );
    for file in &mut files {
        if !file.is_closed() {
            file.try_close().unwrap();
        }
    }
}

pub fn check_foreign<F: Vfsi>(fs: &F, other: &F, path: &str) {
    use vnfs::files::{FileHandle, OpenFlags, OpenOp, Target};
    fs.write_files(&[(path, b"original".as_slice())]).unwrap();
    let mut own = fs
        .vopen(&[OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE)])
        .unwrap()
        .remove(0);
    let mut foreign = other
        .vopen(&[OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE)])
        .unwrap()
        .remove(0);
    let error = fs
        .vsetattrs(&[
            vnfs::directory::SetAttrsOp::new(Target::File(&own)).len(3),
            vnfs::directory::SetAttrsOp::new(Target::File(&foreign)).len(3),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EINVAL as u32);
    assert_eq!(fs.attrs(Target::file(&own)).unwrap().len().unwrap(), 8);
    own.try_close().unwrap();
    foreign.try_close().unwrap();
}

pub fn check_ownership(fs: &impl Vfsi, directory: &str) {
    use vnfs::files::{OpenFlags, OpenOp, Target};
    let paths: Vec<_> = (0..64).map(|i| format!("{directory}/owner-{i}")).collect();
    fs.write_files(
        &paths
            .iter()
            .map(|p| (p, b"ownership".as_slice()))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let options =
        AttrsOptions::new().fields(Attributes::stat() | Attributes::UID | Attributes::GID);
    let attrs = |path: &str, follow: bool| {
        fs.attrs_with_options(path, options.follow_symlinks(follow))
            .unwrap()
    };
    let original = attrs(&paths[0], true);
    let (uid, gid) = (original.uid().unwrap(), original.gid().unwrap());
    let privileged = unsafe { libc::geteuid() } == 0;
    let new_uid = if privileged { 10001 } else { uid };
    let new_gid = if privileged { 10002 } else { gid };
    let updates: Vec<_> = paths
        .iter()
        .map(|p| vnfs::directory::SetAttrsOp::new(p).uid(new_uid))
        .collect();
    fs.vsetattrs(&updates).unwrap();
    for attrs in fs.vgetattrs(&paths, options).unwrap() {
        assert_eq!(attrs.uid(), Some(new_uid));
        assert_eq!(attrs.gid(), Some(gid));
        assert_eq!(attrs.len(), Some(9));
    }
    fs.vsetattrs(
        &paths
            .iter()
            .map(|p| vnfs::directory::SetAttrsOp::new(p).gid(new_gid))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    for attrs in fs.vgetattrs(&paths, options).unwrap() {
        assert_eq!(attrs.uid(), Some(new_uid));
        assert_eq!(attrs.gid(), Some(new_gid));
    }
    let mut files = fs
        .vopen(
            &paths
                .iter()
                .map(|p| OpenOp::new(p, OpenFlags::READ | OpenFlags::WRITE))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let moved = format!("{directory}/owner-moved");
    fs.vrename(
        &[(&paths[0], &moved)],
        vnfs::directory::RenameOptions::Replace,
    )
    .unwrap();
    fs.write_files(&[(&paths[0], b"replacement".as_slice())])
        .unwrap();
    let updates: Vec<_> = files
        .iter()
        .map(|file| {
            vnfs::directory::SetAttrsOp::new(Target::File(file))
                .uid(uid)
                .gid(gid)
                .permissions(Permissions::from_mode(0o640))
                .follow_symlinks(false)
        })
        .collect();
    fs.vsetattrs(&updates).unwrap();
    let targets: Vec<_> = files.iter().map(Target::file).collect();
    for attrs in fs.vgetattrs(&targets, options).unwrap() {
        assert_eq!(attrs.uid(), Some(uid));
        assert_eq!(attrs.gid(), Some(gid));
        assert_eq!(attrs.permissions().unwrap().mode() & 0o7777, 0o640);
    }
    fs.chown(Target::File(&files[0]), Some(new_uid), None)
        .unwrap();
    assert_eq!(
        fs.attrs_with_options(Target::file(&files[0]), options)
            .unwrap()
            .uid(),
        Some(new_uid)
    );
    assert_eq!(
        fs.attrs_with_options(Target::file(&files[0]), options)
            .unwrap()
            .gid(),
        Some(gid)
    );
    fs.chown(&paths[0], None, Some(new_gid)).unwrap();
    assert_eq!(attrs(&paths[0], true).uid(), Some(uid));
    assert_eq!(attrs(&paths[0], true).gid(), Some(new_gid));
    let link = format!("{directory}/owner-link");
    fs.symlink("owner-1", &link).unwrap();
    fs.vsetattrs(&[vnfs::directory::SetAttrsOp::new(&link)
        .uid(new_uid)
        .gid(new_gid)
        .follow_symlinks(false)])
        .unwrap();
    assert_eq!(attrs(&link, false).uid(), Some(new_uid));
    assert_eq!(attrs(&paths[1], true).uid(), Some(uid));
    // Following chown must change the target without changing link ownership.
    fs.chown(&link, Some(uid), Some(gid)).unwrap();
    assert_eq!(attrs(&link, false).uid(), Some(new_uid));
    let error = fs
        .vsetattrs(&[
            vnfs::directory::SetAttrsOp::new(Target::File(&files[0])).len(2),
            vnfs::directory::SetAttrsOp::new(Target::Path(std::path::Path::new(&paths[0])))
                .uid(u32::MAX),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EINVAL as u32);
    assert_eq!(fs.attrs(Target::file(&files[0])).unwrap().len().unwrap(), 9);
    let missing = format!("{directory}/owner-missing");
    let error = fs
        .vsetattrs(&[
            vnfs::directory::SetAttrsOp::new(&paths[0]).uid(uid),
            vnfs::directory::SetAttrsOp::new(&missing).gid(gid),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::ENOENT as u32);
    if !privileged {
        let error = fs.chown(&paths[0], Some(0), None).unwrap_err();
        assert_eq!(error.index(), Some(0));
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
    fs.vclose(&mut files).unwrap();
}
