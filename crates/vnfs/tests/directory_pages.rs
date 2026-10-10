#![cfg(all(feature = "auto", target_os = "linux"))]
use std::ops::ControlFlow;
use std::path::Path;
use vnfs::{ListDirOptions, Posix, TraversalCompletion, Vfsi, VfsiExt, WalkControl, WalkEventKind};

fn collect(
    fs: &impl Vfsi,
    roots: &[&str],
    recursive: bool,
) -> vnfs::Result<Vec<Vec<vnfs::DirectoryListing>>> {
    fs.read_dirs_with_options(roots, ListDirOptions::new().recursive(recursive))
}

#[test]
fn blanket_collection_preserves_empty_roots_children_duplicates_and_multi_page_entries() {
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path()).unwrap();
    fs.create_dir_all("/tree/empty").unwrap();
    fs.create_dir("/empty").unwrap();
    for i in 0..300 {
        fs.write(format!("/tree/f{i:03}"), b"x").unwrap();
    }
    let trees = collect(&fs, &["/tree", "/empty", "/tree"], false).unwrap();
    assert_eq!(trees.len(), 3);
    assert_eq!(trees[0][0].entries.len(), 301);
    assert!(trees[1][0].entries.is_empty());
    assert_eq!(trees[0][0].entries, trees[2][0].entries);
    let trees = collect(&fs, &["/tree", "/empty"], true).unwrap();
    assert_eq!(trees[0].len(), 2);
    assert_eq!(trees[0][1].path, Path::new("/tree/empty"));
    assert!(trees[0][1].entries.is_empty());
    assert!(trees[1][0].entries.is_empty());
}

#[test]
fn page_delivery_is_bounded_reentrant_and_reports_empty_directory_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path()).unwrap();
    fs.create_dir("/empty").unwrap();
    assert_eq!(
        fs.vlistdirs(
            &["/empty", "/missing"],
            ListDirOptions::new(),
            |index, page| {
                assert_eq!(index, 0);
                assert!(page.entries.is_empty());
                assert!(fs.attrs(&page.path).unwrap().is_dir());
                Ok(ControlFlow::Break(()))
            }
        )
        .unwrap(),
        [TraversalCompletion::Stopped]
    );
    for i in 0..300 {
        fs.write(format!("/empty/f{i}"), b"x").unwrap();
    }
    let mut sizes = Vec::new();
    fs.vlistdirs(&["/empty"], ListDirOptions::new(), |_, page| {
        sizes.push(page.entries.len());
        assert_eq!(page.path, Path::new("/empty"));
        assert!(fs.attrs(&page.path).unwrap().is_dir());
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    assert_eq!(sizes[0], 1);
    assert!(sizes.iter().all(|&n| n <= 128));
    assert_eq!(sizes.iter().sum::<usize>(), 300);
}

#[test]
fn recursive_roots_and_entries_never_follow_final_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path()).unwrap();
    fs.create_dir("/target").unwrap();
    fs.write("/target/file", b"x").unwrap();
    fs.symlink("/target", "/link").unwrap();
    let error = collect(&fs, &["/link"], true).unwrap_err();
    assert_eq!(error.err_no(), libc::ENOTDIR as u32);
    assert_eq!(error.index(), Some(0));
    let listings = collect(&fs, &["/"], true).unwrap();
    assert!(
        !listings[0]
            .iter()
            .any(|listing| listing.path == Path::new("/link"))
    );
}

#[test]
fn collection_limits_and_errors_keep_the_input_root_index() {
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path()).unwrap();
    fs.create_dir("/a").unwrap();
    fs.create_dir("/b").unwrap();
    fs.write("/a/f", b"x").unwrap();
    fs.write("/b/f", b"x").unwrap();
    assert_eq!(
        fs.read_dirs_with_options(&["/a", "/b"], ListDirOptions::new().max_entries(1))
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        fs.read_dirs_with_options(&["/a", "/a"], ListDirOptions::new().max_path_bytes(7))
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        collect(&fs, &["/a", "/missing"], false)
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert!(collect(&fs, &[], true).unwrap().is_empty());
}

#[test]
fn listdir_modes_preserve_scope_limits_depth_and_callback_contracts() {
    if vfsi_sync::test_support::supervise_with_deadline(
        "listdir_modes_preserve_scope_limits_depth_and_callback_contracts",
    ) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path()).unwrap();
    fs.create_dir_all("/tree/sub").unwrap();
    fs.create_dir("/tree/empty").unwrap();
    fs.write("/tree/top", b"a").unwrap();
    fs.write("/tree/sub/leaf", b"b").unwrap();
    fs.symlink("sub", "/tree/link").unwrap();
    let limited = Posix::new(root.path())
        .unwrap()
        .with_limits(vnfs::ResourceLimits::new().max_directory_entries(1));
    for (name, recursive, lifecycle, sort) in [
        ("paged", false, false, false),
        ("sorted", false, false, true),
        ("lifecycle", false, true, false),
        ("sorted lifecycle", false, true, true),
        ("recursive paged", true, false, false),
        ("recursive sorted", true, false, true),
        ("recursive lifecycle", true, true, false),
        ("recursive sorted lifecycle", true, true, true),
    ] {
        let options = ListDirOptions::new()
            .recursive(recursive)
            .enter_leave(lifecycle)
            .sort_by_name(sort)
            .max_depth(usize::from(recursive));
        let mut expected: Vec<_> = ["/tree/empty", "/tree/link", "/tree/sub", "/tree/top"]
            .map(std::path::PathBuf::from)
            .into();
        if recursive {
            expected.push("/tree/sub/leaf".into());
        }
        if lifecycle {
            expected.push("/tree".into());
        }
        expected.sort();
        let collect = |options| -> vnfs::Result<Vec<std::path::PathBuf>> {
            let mut paths = Vec::new();
            let completion = fs.listdir("/tree", options, |event| {
                assert_eq!(
                    event.depth,
                    event
                        .entry
                        .path()
                        .strip_prefix("/tree")
                        .unwrap()
                        .components()
                        .count(),
                    "{name}"
                );
                if !lifecycle || (!recursive && event.depth > 0) {
                    assert_eq!(event.kind, WalkEventKind::Entry, "{name}");
                }
                if event.kind != WalkEventKind::Leave {
                    paths.push(event.entry.path().to_owned());
                }
                Ok(WalkControl::Continue)
            })?;
            assert_eq!(completion, TraversalCompletion::Complete, "{name}");
            paths.sort();
            Ok(paths)
        };
        assert_eq!(collect(options).unwrap(), expected, "{name}");
        assert_eq!(
            collect(options.max_entries(usize::MAX).max_path_bytes(usize::MAX)).unwrap(),
            expected,
            "{name}: unlimited budgets"
        );
        assert_eq!(
            collect(options.max_entries(expected.len())).unwrap(),
            expected,
            "{name}: exact entry budget"
        );
        // Expected paths are fixture literals, not another traversal's output.
        let bytes = expected.iter().map(|p| p.as_os_str().len()).sum::<usize>()
            + if recursive && !lifecycle {
                "/tree".len() + "/tree/sub".len() + "/tree/empty".len()
            } else {
                0
            };
        assert_eq!(
            collect(options.max_path_bytes(bytes)).unwrap(),
            expected,
            "{name}"
        );
        for limited_options in [
            options.max_entries(expected.len() - 1),
            options.max_path_bytes(bytes - 1),
            options.max_path_bytes(1),
        ] {
            let error = collect(limited_options).unwrap_err();
            assert_eq!(error.kind(), vnfs::ErrorKind::FileTooLarge, "{name}");
            assert_eq!(error.index(), Some(0), "{name}");
        }
        assert_eq!(
            fs.listdir("/tree", options.max_entries(0), |_| panic!("zero budget"))
                .unwrap_err()
                .kind(),
            vnfs::ErrorKind::FileTooLarge,
            "{name}"
        );
        assert_eq!(
            limited
                .listdir("/tree", options, |_| Ok(WalkControl::Continue))
                .unwrap_err()
                .kind(),
            vnfs::ErrorKind::FileTooLarge,
            "{name}: inherited limits"
        );
        let mut calls = 0;
        assert_eq!(
            fs.listdir("/tree", options, |event| {
                calls += 1;
                fs.symlink_attrs(event.entry.path())?;
                Ok(WalkControl::Stop)
            })
            .unwrap(),
            TraversalCompletion::Stopped,
            "{name}"
        );
        assert_eq!(calls, 1, "{name}");
        for fault in [
            vnfs::Error::client(0, libc::ECANCELED as u32),
            vnfs::Error::transport(None, "callback failure"),
        ] {
            calls = 0;
            let error = fs
                .listdir("/tree", options, |_| {
                    calls += 1;
                    Err(fault.clone())
                })
                .unwrap_err();
            assert_eq!(error.kind(), fault.kind(), "{name}");
            assert_eq!(error.err_no(), fault.err_no(), "{name}");
            assert_eq!(calls, 1, "{name}: callbacks must not replay");
        }
        if recursive {
            assert_eq!(
                collect(options.max_depth(0)).unwrap_err().kind(),
                vnfs::ErrorKind::FileTooLarge,
                "{name}"
            );
            let truncated = collect(options.max_depth(0).truncate_at_max_depth(true)).unwrap();
            let shallow: Vec<_> = expected
                .iter()
                .filter(|p| p.as_path() != Path::new("/tree/sub/leaf"))
                .cloned()
                .collect();
            assert_eq!(truncated, shallow, "{name}");
            if lifecycle {
                fs.listdir("/tree", options.max_depth(0), |event| {
                    assert_ne!(event.entry.path(), Path::new("/tree/sub/leaf"));
                    Ok(if event.depth > 0 && event.kind == WalkEventKind::Enter {
                        WalkControl::SkipSubtree
                    } else {
                        WalkControl::Continue
                    })
                })
                .unwrap();
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn buffered_local_walk_never_follows_replaced_children_or_ancestors() {
    for options in [
        ListDirOptions::new().sort_by_name(true),
        ListDirOptions::new().enter_leave(true),
        ListDirOptions::new().sort_by_name(true).enter_leave(true),
    ] {
        for (name, victim, target, outside) in [
            ("child", "/tree/child", "../outside", "/outside"),
            ("ancestor", "/tree", "outside", "/outside/child"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let fs = Posix::new(root.path()).unwrap();
            fs.create_dir_all("/tree/child").unwrap();
            fs.create_dir_all(outside).unwrap();
            fs.write(format!("{outside}/secret"), b"never traverse this")
                .unwrap();
            let mut changed = false;
            let mut escaped = false;
            let result = fs.listdir("/tree", options.recursive(true), |event| {
                if event.entry.path() == Path::new("/tree/child") && !changed {
                    fs.vrename(&[(victim, "/saved")], Default::default())?;
                    fs.symlink(target, victim)?;
                    changed = true;
                }
                escaped |= event.entry.path().ends_with("secret");
                Ok(vnfs::WalkControl::Continue)
            });
            assert!(changed);
            assert!(!escaped, "{options:?}, {name}");
            let error = result.unwrap_err();
            assert_eq!(error.err_no(), libc::ELOOP as u32);
        }
    }
}

#[test]
fn scalar_collectors_select_named_scope_and_respect_visit_budgets() {
    let root = tempfile::tempdir().unwrap();
    let fs = Posix::new(root.path())
        .unwrap()
        .with_limits(vnfs::ResourceLimits::new().max_directory_entries(1));
    fs.create_dir_all("/tree/sub").unwrap();
    fs.write("/tree/top", b"a").unwrap();
    fs.write("/tree/sub/leaf", b"b").unwrap();
    assert_eq!(
        fs.read_dir_with_options("/tree", ListDirOptions::new())
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
    assert_eq!(
        fs.read_dirs_with_options(&["/tree"], ListDirOptions::new().recursive(true))
            .map(|mut trees| trees.remove(0))
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
    let options = ListDirOptions::new()
        .max_entries(3)
        .fields(vnfs::Attributes::MODE);
    let shallow = fs
        .read_dir_with_options("/tree", options.recursive(true))
        .unwrap();
    assert_eq!(shallow.len(), 2);
    assert!(
        !shallow
            .iter()
            .any(|entry| entry.path() == Path::new("/tree/sub/leaf"))
    );
    let tree = fs
        .read_dirs_with_options(&["/tree"], options.recursive(true))
        .map(|mut trees| trees.remove(0))
        .unwrap();
    assert_eq!(tree.len(), 2);
    assert!(
        tree.iter()
            .any(|listing| listing.path == Path::new("/tree/sub"))
    );
    assert_eq!(
        fs.read_dirs_with_options(&["/tree"], options.max_depth(0).recursive(true))
            .map(|mut trees| trees.remove(0))
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
}
