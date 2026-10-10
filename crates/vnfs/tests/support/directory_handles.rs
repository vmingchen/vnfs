use vnfs::files::{Vfsi, VfsiExt};

pub fn check<C: Vfsi>(fs: &C, other: &C, namespace: &impl Vfsi, verify: &impl Vfsi) {
    use vnfs::directory::{DirHandle, RemoveOptions};
    assert!(fs.vopen_dirs::<&str>(&[]).unwrap().is_empty());
    fs.vremove_dir_contents(&[], RemoveOptions::default())
        .unwrap();
    namespace
        .vmkdir(&[
            vnfs::directory::MkDirOp::new("/a", 0o755),
            vnfs::directory::MkDirOp::new("/b", 0o755),
            vnfs::directory::MkDirOp::new("/outside", 0o755),
        ])
        .unwrap();
    namespace.write("/a/file", b"original").unwrap();
    namespace.write("/b/file", b"second").unwrap();
    namespace.write("/outside/keep", b"outside").unwrap();
    namespace.symlink("../outside", "/a/link").unwrap();
    namespace.symlink("/b", "/dir_link").unwrap();
    assert!(fs.open_dir_handle("/dir_link").is_err());
    let mut dirs = fs.vopen_dirs(&["/a", "/b"]).unwrap();
    namespace.rename("/a", "/moved").unwrap();
    namespace.create_dir("/a").unwrap();
    namespace.write("/a/keep", b"replacement").unwrap();
    assert_eq!(dirs[0].path(), std::path::Path::new("/a"));
    let foreign = other.open_dir_handle("/b").unwrap();
    for options in [
        RemoveOptions::default(),
        RemoveOptions::new().continue_on_error(true),
    ] {
        let error = fs
            .vremove_dir_contents(&[&dirs[0], &foreign], options)
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(verify.read("/moved/file").unwrap(), b"original");
    }
    dirs[1].try_close().unwrap();
    let error = fs
        .vremove_dir_contents(&[&dirs[0], &dirs[1]], RemoveOptions::default())
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.err_no(), libc::EBADF as u32);
    assert_eq!(verify.read("/moved/file").unwrap(), b"original");
    let second = fs.open_dir_handle("/b").unwrap();
    fs.vremove_dir_contents(&[&dirs[0], &second], RemoveOptions::new().batch(1))
        .unwrap();
    assert!(verify.read_dir("/moved").unwrap().is_empty());
    assert!(verify.read_dir("/b").unwrap().is_empty());
    assert_eq!(verify.read("/a/keep").unwrap(), b"replacement");
    assert_eq!(verify.read("/outside/keep").unwrap(), b"outside");
    namespace.write("/b/new", b"scalar").unwrap();
    fs.remove_dir_contents_handle(&second).unwrap();
    assert!(verify.read_dir("/b").unwrap().is_empty());
    for dir in dirs {
        dir.close().unwrap();
    }
    second.close().unwrap();
    foreign.close().unwrap();
}
