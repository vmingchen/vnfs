use vnfs::files::{Vfsi, VfsiExt};

pub fn check<C: Vfsi>(fs: &C, other: &C, namespace: &impl Vfsi, verify: &impl Vfsi) {
    use vnfs::directory::{DirHandle, RemoveOptions};
    assert!(fs.open_dirs::<&str>(&[]).unwrap().is_empty());
    fs.remove_contents_all(&[], RemoveOptions::default())
        .unwrap();
    namespace
        .vmkdir(&[
            vnfs::directory::MkDirOp::new("/a", 0o755),
            vnfs::directory::MkDirOp::new("/b", 0o755),
            vnfs::directory::MkDirOp::new("/outside", 0o755),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap();
    namespace.write("/a/file", b"original").unwrap();
    namespace.write("/b/file", b"second").unwrap();
    namespace.write("/outside/keep", b"outside").unwrap();
    namespace.symlink("../outside", "/a/link").unwrap();
    namespace.symlink("/b", "/dir_link").unwrap();
    assert!(fs.open_dirs(&["/dir_link"]).is_err());
    let outcomes = fs
        .vopen_dirs(&["/a", "/missing", "/b", "/dir_link", "/a/file"])
        .unwrap();
    assert_eq!(outcomes.len(), 5);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    for index in [1, 3, 4] {
        let error = outcomes[index].as_ref().err().expect("failed input");
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    let mut dirs: Vec<_> = outcomes.into_iter().filter_map(Result::ok).collect();
    namespace.rename("/a", "/moved").unwrap();
    namespace.create_dir("/a").unwrap();
    namespace.write("/a/keep", b"replacement").unwrap();
    assert_eq!(dirs[0].path(), std::path::Path::new("/a"));
    let foreign = other.open_dirs(&["/b"]).unwrap().remove(0);
    for options in [
        RemoveOptions::default(),
        RemoveOptions::new().with_continue_on_error(true),
    ] {
        namespace.write("/moved/file", b"original").unwrap();
        let outcomes = fs
            .vremove_dir_contents(&[&dirs[0], &foreign], options)
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].is_ok());
        assert_eq!(
            outcomes[1].as_ref().unwrap_err().cause().raw_code(),
            libc::EBADF as u32
        );
        assert!(verify.read_dir("/moved").unwrap().is_empty());
        assert_eq!(verify.read("/a/keep").unwrap(), b"replacement");
    }
    namespace.write("/moved/file", b"original").unwrap();
    dirs[1].try_close().unwrap();
    dirs[1].try_close().unwrap();
    let outcomes = fs
        .vremove_dir_contents(&[&dirs[1], &dirs[0]], RemoveOptions::default())
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    let error = outcomes[0].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
    assert_eq!(error.cause().raw_code(), libc::EBADF as u32);
    assert!(outcomes[1].is_ok());
    assert!(verify.read_dir("/moved").unwrap().is_empty());
    // Successful borrowed owners stay live and can empty the renamed directory again.
    namespace.write("/moved/again", b"second pass").unwrap();
    let second = fs.open_dirs(&["/b"]).unwrap().remove(0);
    fs.remove_contents_all(
        &[&dirs[0], &second],
        RemoveOptions::new().with_batch_size(std::num::NonZeroUsize::new(1)),
    )
    .unwrap();
    assert!(verify.read_dir("/moved").unwrap().is_empty());
    assert!(verify.read_dir("/b").unwrap().is_empty());
    assert_eq!(verify.read("/a/keep").unwrap(), b"replacement");
    assert_eq!(verify.read("/outside/keep").unwrap(), b"outside");
    namespace.write("/b/new", b"scalar").unwrap();
    fs.remove_dir_contents("/b").unwrap();
    assert!(verify.read_dir("/b").unwrap().is_empty());
    for dir in dirs {
        dir.close().unwrap();
    }
    second.close().unwrap();
    foreign.close().unwrap();
}
