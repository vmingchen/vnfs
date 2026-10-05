use vnfs::{Attributes, AttrsOptions, MetadataUpdate, Permissions, Vfsi, VfsiExt};

pub fn check_many(fs: &impl Vfsi, directory: &str) {
    fs.vsetattrs::<&str>(&[], true).unwrap();
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
            (
                path,
                MetadataUpdate::new()
                    .permissions(Permissions::from_mode(0o600 | ((i % 8) as u32)))
                    .len(if i % 2 == 0 { 3 } else { 100 + i as u64 })
                    .modified(modified),
            )
        })
        .collect();
    fs.vsetattrs(&updates, true).unwrap();
    let options =
        AttrsOptions::new().fields(Attributes::MODE | Attributes::SIZE | Attributes::MTIME);
    let metadata = fs.vgetattrs(&paths, options).unwrap();
    assert_eq!(metadata.len(), paths.len());
    for (i, item) in metadata.iter().enumerate() {
        assert_eq!(item.permissions().mode() & 0o7777, 0o600 | ((i % 8) as u32));
        assert_eq!(item.len(), if i % 2 == 0 { 3 } else { 100 + i as u64 });
        assert_eq!(item.modified(), Some(modified));
    }
    // Size-only mutations preserve permissions and retain zero as a valid size.
    let sizes: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| (path, MetadataUpdate::new().len(i as u64)))
        .collect();
    fs.vsetattrs(&sizes, false).unwrap();
    let metadata = fs.vgetattrs(&paths, options).unwrap();
    for (i, item) in metadata.iter().enumerate() {
        assert_eq!(item.len(), i as u64);
        assert_eq!(item.permissions().mode() & 0o7777, 0o600 | ((i % 8) as u32));
    }
    let missing = format!("{directory}/missing");
    let error = fs
        .vsetattrs(
            &[
                (&paths[0], MetadataUpdate::new().len(7)),
                (&missing, MetadataUpdate::new().len(7)),
                (&paths[1], MetadataUpdate::new().len(7)),
            ],
            true,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::ENOENT as u32);
}

/// Open objects remain targets even when their original names are reused.
pub fn check_handles(fs: &impl Vfsi, directory: &str) {
    use vnfs::{FileHandle, MetadataTarget, OpenFlags, OpenOp};
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
        vnfs::RenameOptions::Replace,
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
            (
                MetadataTarget::File(file),
                MetadataUpdate::new()
                    .len(i as u64)
                    .permissions(Permissions::from_mode(0o640)),
            )
        })
        .collect();
    // no-follow must still address the opened objects.
    fs.vsetattrs(&updates, false).unwrap();
    for (i, file) in files.iter().enumerate() {
        let attrs = file.attrs().unwrap();
        assert_eq!(attrs.len(), i as u64);
        assert_eq!(attrs.permissions().mode() & 0o7777, 0o640);
        assert_eq!(fs.attrs(&paths[i]).unwrap().len(), 11);
    }
    fs.vsetattrs(
        &[
            (
                MetadataTarget::File(&files[0]),
                MetadataUpdate::new().len(13),
            ),
            (
                MetadataTarget::Path(std::path::Path::new(&paths[0])),
                MetadataUpdate::new().len(17),
            ),
        ],
        true,
    )
    .unwrap();
    assert_eq!(files[0].attrs().unwrap().len(), 13);
    assert_eq!(fs.attrs(&paths[0]).unwrap().len(), 17);
    fs.truncate(MetadataTarget::File(&files[0]), 19).unwrap();
    fs.chmod(
        MetadataTarget::File(&files[0]),
        Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(files[0].attrs().unwrap().len(), 19);
    assert_eq!(
        files[0].attrs().unwrap().permissions().mode() & 0o7777,
        0o600
    );
    files[1].try_close().unwrap();
    let error = fs
        .vsetattrs(
            &[
                (
                    MetadataTarget::File(&files[0]),
                    MetadataUpdate::new().len(5),
                ),
                (
                    MetadataTarget::File(&files[1]),
                    MetadataUpdate::new().len(5),
                ),
            ],
            true,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EBADF as u32);
    assert_eq!(files[0].attrs().unwrap().len(), 19);
    for file in &mut files {
        if !file.is_closed() {
            file.try_close().unwrap();
        }
    }
}

pub fn check_foreign<F: Vfsi>(fs: &F, other: &F, path: &str) {
    use vnfs::{FileHandle, MetadataTarget, OpenFlags, OpenOp};
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
        .vsetattrs(
            &[
                (MetadataTarget::File(&own), MetadataUpdate::new().len(3)),
                (MetadataTarget::File(&foreign), MetadataUpdate::new().len(3)),
            ],
            true,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EINVAL as u32);
    assert_eq!(own.attrs().unwrap().len(), 8);
    own.try_close().unwrap();
    foreign.try_close().unwrap();
}

pub fn check_ownership(fs: &impl Vfsi, directory: &str) {
    use vnfs::{FileHandle, MetadataTarget, OpenFlags, OpenOp};
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
        .map(|p| (p, MetadataUpdate::new().uid(new_uid)))
        .collect();
    fs.vsetattrs(&updates, true).unwrap();
    for attrs in fs.vgetattrs(&paths, options).unwrap() {
        assert_eq!(attrs.uid(), Some(new_uid));
        assert_eq!(attrs.gid(), Some(gid));
        assert_eq!(attrs.len(), 9);
    }
    fs.vsetattrs(
        &paths
            .iter()
            .map(|p| (p, MetadataUpdate::new().gid(new_gid)))
            .collect::<Vec<_>>(),
        true,
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
    fs.vrename(&[(&paths[0], &moved)], vnfs::RenameOptions::Replace)
        .unwrap();
    fs.write_files(&[(&paths[0], b"replacement".as_slice())])
        .unwrap();
    let updates: Vec<_> = files
        .iter()
        .map(|file| {
            (
                MetadataTarget::File(file),
                MetadataUpdate::new()
                    .uid(uid)
                    .gid(gid)
                    .permissions(Permissions::from_mode(0o640)),
            )
        })
        .collect();
    fs.vsetattrs(&updates, false).unwrap();
    for file in &files {
        let attrs = file.attrs().unwrap();
        assert_eq!(attrs.uid(), Some(uid));
        assert_eq!(attrs.gid(), Some(gid));
        assert_eq!(attrs.permissions().mode() & 0o7777, 0o640);
    }
    fs.chown(MetadataTarget::File(&files[0]), Some(new_uid), None)
        .unwrap();
    assert_eq!(files[0].attrs().unwrap().uid(), Some(new_uid));
    assert_eq!(files[0].attrs().unwrap().gid(), Some(gid));
    fs.chown(&paths[0], None, Some(new_gid)).unwrap();
    assert_eq!(attrs(&paths[0], true).uid(), Some(uid));
    assert_eq!(attrs(&paths[0], true).gid(), Some(new_gid));
    let link = format!("{directory}/owner-link");
    fs.symlink("owner-1", &link).unwrap();
    fs.vsetattrs(
        &[(&link, MetadataUpdate::new().uid(new_uid).gid(new_gid))],
        false,
    )
    .unwrap();
    assert_eq!(attrs(&link, false).uid(), Some(new_uid));
    assert_eq!(attrs(&paths[1], true).uid(), Some(uid));
    // Following chown must change the target without changing link ownership.
    fs.chown(&link, Some(uid), Some(gid)).unwrap();
    assert_eq!(attrs(&link, false).uid(), Some(new_uid));
    let error = fs
        .vsetattrs(
            &[
                (
                    MetadataTarget::File(&files[0]),
                    MetadataUpdate::new().len(2),
                ),
                (
                    MetadataTarget::Path(std::path::Path::new(&paths[0])),
                    MetadataUpdate::new().uid(u32::MAX),
                ),
            ],
            true,
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EINVAL as u32);
    assert_eq!(files[0].attrs().unwrap().len(), 9);
    let missing = format!("{directory}/owner-missing");
    let error = fs
        .vsetattrs(
            &[
                (&paths[0], MetadataUpdate::new().uid(uid)),
                (&missing, MetadataUpdate::new().gid(gid)),
            ],
            true,
        )
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
