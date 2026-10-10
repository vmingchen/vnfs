#![cfg(all(feature = "auto", target_os = "linux"))]
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};
use vnfs::{
    helpers::{PathMapper, ResolvePath, copy_to_writer},
    *,
};

#[test]
fn mapping_preserves_final_links_and_rejects_escapes_and_destructive_dot_paths() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("file"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.path().join("file"), root.path().join("link")).unwrap();
    let session = PathMapper::new(root.path()).unwrap();
    assert_eq!(
        session
            .map(root.path().join("new"), ResolvePath::NoFollow)
            .unwrap(),
        Path::new("/new")
    );
    assert_eq!(
        session
            .map(root.path().join("link"), ResolvePath::NoFollow)
            .unwrap(),
        Path::new("/link")
    );
    assert_eq!(
        session
            .map(root.path().join("link"), ResolvePath::Follow)
            .unwrap_err()
            .err_no(),
        libc::EXDEV as u32
    );
    for suffix in ["/.", "/..", "/"] {
        let spelling = PathBuf::from(format!("{}{suffix}", root.path().display()));
        assert_eq!(
            session
                .map(spelling, ResolvePath::NoFollow)
                .unwrap_err()
                .err_no(),
            libc::EINVAL as u32
        );
    }
    assert_eq!(session.local_path("/new").unwrap(), root.path().join("new"));
    assert!(session.local_path("/../outside").is_err());
}

#[test]
fn writer_bridge_handles_short_writes_and_never_replays_failed_output() {
    struct Writer {
        bytes: Vec<u8>,
        fail_after: usize,
        calls: usize,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.bytes.len() == self.fail_after {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            let n = bytes.len().min(2).min(self.fail_after - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("helper must not flush caller's writer")
        }
    }
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path()).unwrap();
    fs.write("/source", b"0123456789").unwrap();
    let mut ok = Writer {
        bytes: vec![],
        fail_after: usize::MAX,
        calls: 0,
    };
    assert_eq!(
        copy_to_writer(
            &fs,
            "/source",
            &mut ok,
            StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(3).unwrap())
        )
        .unwrap(),
        10
    );
    assert_eq!(ok.bytes, b"0123456789");
    let mut bad = Writer {
        bytes: vec![],
        fail_after: 4,
        calls: 0,
    };
    let err = copy_to_writer(
        &fs,
        "/source",
        &mut bad,
        StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(3).unwrap()),
    )
    .unwrap_err();
    assert_eq!(err.err_no(), libc::ENOSPC as u32);
    assert_eq!(bad.bytes, b"0123");
    assert_eq!(bad.calls, 4);
}

#[test]
fn ordered_walk_preserves_sorting_prunes_before_io_and_callbacks_are_unlocked() {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path()).unwrap();
    for p in ["/a", "/b", "/prune", "/empty"] {
        fs.create_dir(p).unwrap();
    }
    fs.write("/a/file", b"x").unwrap();
    fs.symlink("/a", "/link").unwrap();
    let mut paths = Vec::new();
    let completion = fs
        .visit_dirs_ordered(
            "/",
            ListDirOptions::new()
                .recursive(true)
                .fields(Attributes::MODE),
            |a, b| b.path().cmp(a.path()),
            |entry| {
                if entry.path() == Path::new("/prune") {
                    // A prefetch would fail if it tried to list this now-missing child.
                    std::fs::remove_dir(root.path().join("prune")).unwrap();
                    false
                } else {
                    true
                }
            },
            |listing, depth| {
                assert!(fs.attrs(&listing.path).unwrap().is_dir());
                assert_eq!(depth, usize::from(listing.path != Path::new("/")));
                paths.push(listing.path);
                Ok(WalkControl::Continue)
            },
        )
        .unwrap();
    assert_eq!(completion, TraversalCompletion::Complete);
    assert_eq!(paths, ["/", "/empty", "/b", "/a"].map(PathBuf::from));
}

#[test]
fn ordered_walk_stop_skip_and_limits_are_not_silent_truncation() {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path()).unwrap();
    fs.create_dir("/child").unwrap();
    fs.write("/child/file", b"x").unwrap();
    fs.symlink("/child", "/link").unwrap();
    assert!(
        fs.visit_dirs_ordered(
            "/link",
            ListDirOptions::new()
                .recursive(true)
                .fields(Attributes::MODE),
            |a, b| a.path().cmp(b.path()),
            |_| true,
            |_, _| panic!("symlink root must not be followed")
        )
        .is_err()
    );
    fs.remove_file("/link").unwrap();
    for control in [WalkControl::Stop, WalkControl::SkipSubtree] {
        let mut calls = 0;
        let status = fs
            .visit_dirs_ordered(
                "/",
                ListDirOptions::new()
                    .recursive(true)
                    .fields(Attributes::MODE),
                |a, b| a.path().cmp(b.path()),
                |_| true,
                |listing, _| {
                    calls += 1;
                    assert_eq!(listing.path, Path::new("/"));
                    std::fs::remove_file(root.path().join("child/file")).ok();
                    std::fs::remove_dir(root.path().join("child")).unwrap();
                    Ok(control)
                },
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            status,
            if control == WalkControl::Stop {
                TraversalCompletion::Stopped
            } else {
                TraversalCompletion::Complete
            }
        );
        fs.create_dir("/child").unwrap();
    }
    let mut calls = 0;
    assert_eq!(
        fs.visit_dirs_ordered(
            "/",
            ListDirOptions::new()
                .recursive(true)
                .max_entries(1)
                .fields(Attributes::MODE),
            |a, b| a.path().cmp(b.path()),
            |_| false,
            |_, _| {
                calls += 1;
                Ok(WalkControl::Continue)
            }
        )
        .unwrap_err()
        .err_no(),
        libc::EFBIG as u32
    );
    assert_eq!(calls, 0);
    assert_eq!(
        fs.visit_dirs_ordered(
            "/",
            ListDirOptions::new()
                .recursive(true)
                .max_path_bytes(1)
                .fields(Attributes::MODE),
            |a, b| a.path().cmp(b.path()),
            |_| false,
            |_, _| panic!("over-budget snapshot must not be delivered")
        )
        .unwrap_err()
        .err_no(),
        libc::EFBIG as u32
    );
}
