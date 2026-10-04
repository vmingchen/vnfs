#![cfg(all(feature = "auto", target_os = "linux"))]
use std::ops::ControlFlow;
use std::path::Path;
use vnfs::{Fs, FsExt, Mounted, TraversalCompletion, VisitOptions};

fn collect(
    fs: &impl Fs,
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
        fs.visit_dirs_with_options(
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
    fs.visit_dirs_with_options(&["/empty"], VisitOptions::new(), |_, page| {
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
