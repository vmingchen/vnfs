#![cfg(all(feature = "auto", target_os = "linux"))]
use std::ops::ControlFlow;
use std::path::Path;
use vnfs::{Mounted, TraversalCompletion, Vfsi, VfsiExt, VisitOptions};

fn collect(
    fs: &impl Vfsi,
    roots: &[&str],
    recursive: bool,
) -> vnfs::Result<Vec<Vec<vnfs::DirectoryListing>>> {
    fs.read_dirs_with_options(roots, VisitOptions::new().recursive(recursive))
}

#[test]
fn blanket_collection_preserves_empty_roots_children_duplicates_and_multi_page_entries() {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path()).unwrap();
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
    let fs = Mounted::new(root.path()).unwrap();
    fs.create_dir("/empty").unwrap();
    assert_eq!(
        fs.vlistdirs(
            &["/empty", "/missing"],
            VisitOptions::new(),
            |index, page| {
                assert_eq!(index, 0);
                assert!(page.entries.is_empty());
                assert!(fs.metadata(&page.path).unwrap().is_dir());
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
    fs.vlistdirs(&["/empty"], VisitOptions::new(), |_, page| {
        sizes.push(page.entries.len());
        assert_eq!(page.path, Path::new("/empty"));
        assert!(fs.metadata(&page.path).unwrap().is_dir());
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
    let fs = Mounted::new(root.path()).unwrap();
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
    let fs = Mounted::new(root.path()).unwrap();
    fs.create_dir("/a").unwrap();
    fs.create_dir("/b").unwrap();
    fs.write("/a/f", b"x").unwrap();
    fs.write("/b/f", b"x").unwrap();
    assert_eq!(
        fs.read_dirs_with_options(&["/a", "/b"], VisitOptions::new().max_entries(1))
            .unwrap_err()
            .index(),
        Some(1)
    );
    assert_eq!(
        fs.read_dirs_with_options(&["/a", "/a"], VisitOptions::new().max_path_bytes(7))
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
fn single_directory_visitor_honors_recursion_and_preserves_convenience_defaults() {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path()).unwrap();
    fs.create_dir_all("/tree/sub").unwrap();
    fs.write("/tree/top", b"a").unwrap();
    fs.write("/tree/sub/leaf", b"b").unwrap();
    fs.symlink("sub", "/tree/link").unwrap();
    let collect = |options| {
        let mut paths = Vec::new();
        assert_eq!(
            fs.visit_dir_with_options("/tree", options, |entry| {
                paths.push(entry.path().to_path_buf());
                Ok(ControlFlow::Continue(()))
            })
            .unwrap(),
            TraversalCompletion::Complete
        );
        paths.sort();
        paths
    };
    let shallow = collect(VisitOptions::new());
    assert_eq!(shallow.len(), 3);
    assert!(!shallow.contains(&"/tree/sub/leaf".into()));
    let recursive = collect(VisitOptions::new().recursive(true));
    assert_eq!(recursive.len(), 4);
    assert!(recursive.contains(&"/tree/sub/leaf".into()));
    assert!(!recursive.contains(&"/tree/link/leaf".into()));
    for (walk, expected) in [(false, shallow), (true, recursive)] {
        let mut paths = Vec::new();
        let callback = |entry: &vnfs::DirEntry| {
            paths.push(entry.path().to_path_buf());
            Ok(ControlFlow::Continue(()))
        };
        let completion = if walk {
            fs.visit_walk("/tree", callback)
        } else {
            fs.visit_dir("/tree", callback)
        }
        .unwrap();
        assert_eq!(completion, TraversalCompletion::Complete);
        paths.sort();
        assert_eq!(paths, expected);
    }
}

#[test]
fn consolidated_visitor_preserves_limits_cancellation_and_callback_errors() {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path()).unwrap();
    fs.create_dir_all("/tree/sub").unwrap();
    fs.write("/tree/sub/leaf", b"x").unwrap();
    for recursive in [false, true] {
        let options = VisitOptions::new().recursive(recursive);
        let mut calls = 0;
        assert_eq!(
            fs.visit_dir_with_options("/tree", options, |entry| {
                calls += 1;
                fs.symlink_metadata(entry.path())?;
                Ok(ControlFlow::Break(()))
            })
            .unwrap(),
            TraversalCompletion::Stopped
        );
        assert_eq!(calls, 1);
        let error = fs
            .visit_dir_with_options("/tree", options, |_| {
                Err(vnfs::Error::transport(None, "callback failure"))
            })
            .unwrap_err();
        assert!(error.to_string().contains("callback failure"));
        let error = fs
            .visit_dir_with_options("/tree", options.max_entries(0), |_| {
                panic!("entry budget must be enforced before delivery")
            })
            .unwrap_err();
        assert_eq!(error.kind(), vnfs::ErrorKind::FileTooLarge);
    }
    let recursive = VisitOptions::new().recursive(true).max_depth(0);
    assert_eq!(
        fs.visit_dir_with_options("/tree", recursive, |_| { Ok(ControlFlow::Continue(())) })
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
    let mut paths = Vec::new();
    fs.visit_dir_with_options("/tree", recursive.truncate_at_max_depth(true), |entry| {
        paths.push(entry.path().to_path_buf());
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    assert_eq!(paths, [Path::new("/tree/sub")]);
}

#[test]
fn scalar_collectors_select_named_scope_and_respect_visit_budgets() {
    let root = tempfile::tempdir().unwrap();
    let fs = Mounted::new(root.path())
        .unwrap()
        .with_limits(vnfs::ResourceLimits {
            max_directory_entries: 1,
            ..Default::default()
        });
    fs.create_dir_all("/tree/sub").unwrap();
    fs.write("/tree/top", b"a").unwrap();
    fs.write("/tree/sub/leaf", b"b").unwrap();
    assert_eq!(
        fs.read_dir_with_options("/tree", VisitOptions::new())
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
    assert_eq!(
        fs.walk_with_options("/tree", VisitOptions::new())
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
    let options = VisitOptions::new()
        .max_entries(3)
        .fields(vnfs::MetadataFields::MODE);
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
        .walk_with_options("/tree", options.recursive(false))
        .unwrap();
    assert_eq!(tree.len(), 2);
    assert!(
        tree.iter()
            .any(|listing| listing.path == Path::new("/tree/sub"))
    );
    assert_eq!(
        fs.walk_with_options("/tree", options.max_depth(0))
            .unwrap_err()
            .kind(),
        vnfs::ErrorKind::FileTooLarge
    );
}
