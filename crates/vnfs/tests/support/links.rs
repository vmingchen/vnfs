use std::path::{Path, PathBuf};
use vnfs::{Attributes, AttrsOptions, Capabilities, Vfsi, VfsiExt};

pub fn check_links_and_modes(fs: &impl Vfsi, directory: &str) {
    assert!(
        fs.capabilities().unwrap().contains(
            Capabilities::SYMLINKS | Capabilities::HARDLINKS | Capabilities::POSIX_METADATA
        )
    );
    fs.vmkdir::<&str>(&[]).unwrap();
    fs.vsymlink::<&str, &str>(&[]).unwrap();
    fs.vhardlink::<&str, &str>(&[]).unwrap();
    assert!(fs.vreadlink::<&str>(&[]).unwrap().is_empty());

    let directories: Vec<_> = (0..64)
        .map(|i| vnfs::MkDirOp::new(format!("{directory}/dir-{i}"), 0o700 | (i % 8)))
        .collect();
    fs.vmkdir(&directories).unwrap();
    let paths: Vec<_> = directories.iter().map(|op| op.path()).collect();
    let attrs = fs
        .vgetattrs(&paths, AttrsOptions::new().fields(Attributes::MODE))
        .unwrap();
    assert_eq!(attrs.len(), directories.len());
    for (attrs, op) in attrs.iter().zip(&directories) {
        assert!(attrs.is_dir());
        assert_eq!(attrs.permissions().mode() & 0o7777, op.mode());
    }
    fs.create_dir_with_mode(format!("{directory}/scalar-dir"), 0o751)
        .unwrap();
    assert_eq!(
        fs.attrs(format!("{directory}/scalar-dir"))
            .unwrap()
            .permissions()
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
    fs.vsymlink(&requests).unwrap();
    let read = fs.vreadlink(&links).unwrap();
    assert_eq!(read, targets.iter().map(PathBuf::from).collect::<Vec<_>>());
    for attrs in fs
        .vgetattrs(&links, AttrsOptions::new().follow_symlinks(false))
        .unwrap()
    {
        assert!(attrs.is_symlink());
    }

    let hardlinks: Vec<_> = (0..64).map(|i| format!("{directory}/hard-{i}")).collect();
    let requests: Vec<_> = files.iter().zip(&hardlinks).collect();
    fs.vhardlink(&requests).unwrap();
    let attrs = fs
        .vgetattrs(
            &files,
            AttrsOptions::new().fields(Attributes::FILEID | Attributes::NLINK),
        )
        .unwrap();
    let linked = fs
        .vgetattrs(
            &hardlinks,
            AttrsOptions::new().fields(Attributes::FILEID | Attributes::NLINK),
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
    fs.vsymlink(&[
        (&raw, &raw_link),
        (&PathBuf::from("/missing/../target"), &absolute_link),
    ])
    .unwrap();
    assert_eq!(
        fs.vreadlink(&[&raw_link, &absolute_link]).unwrap(),
        [raw, PathBuf::from("/missing/../target")]
    );

    // Native errors identify the original vector item, without replaying.
    let fresh = format!("{directory}/fresh-symbolic");
    let error = fs
        .vsymlink(&[("file-0", &fresh), ("file-1", &links[1])])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let fresh_hard = format!("{directory}/fresh-hard");
    let error = fs
        .vhardlink(&[(&files[0], &fresh_hard), (&files[1], &hardlinks[1])])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let error = fs.vreadlink(&[&links[0], &files[0]]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    let new_dir = format!("{directory}/fresh-dir");
    let error = fs
        .vmkdir(&[
            vnfs::MkDirOp::new(std::path::Path::new(&new_dir), 0o711),
            vnfs::MkDirOp::new(directories[0].path(), 0o755),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        fs.attrs(&new_dir).unwrap().permissions().mode() & 0o7777,
        0o711
    );
    let duplicate = format!("{directory}/duplicate-dir");
    let error = fs
        .vmkdir(&[
            vnfs::MkDirOp::new(&duplicate, 0o700),
            vnfs::MkDirOp::new(&duplicate, 0o755),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(fs.attrs(&duplicate).is_err());
}
