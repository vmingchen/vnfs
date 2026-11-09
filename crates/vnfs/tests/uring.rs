#![cfg(all(feature = "uring", target_os = "linux"))]
use vnfs::files::{OpenFlags, OpenOp, ReadOp, VfsiExt, WriteOp};
use vnfs::uring::Uring;
// This target runs with only the uring feature, so NFS cannot hide its guide.
#[allow(unused_imports)]
#[test]
fn opaque_facade_batches_and_retains_successes_around_foreign_handles() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        std::fs::write(root.path().join(name), b"hello").unwrap();
    }
    let (fs, telemetry) = Uring::with_telemetry(root.path(), Default::default()).unwrap();
    let other = Uring::new(root.path()).unwrap();
    let mut files = fs
        .open_batch(&[
            OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE),
            OpenOp::new("/b", OpenFlags::READ | OpenFlags::WRITE),
        ])
        .unwrap();
    let foreign = other
        .open_batch(&[OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE)])
        .unwrap();
    let error = fs
        .write_batch(
            &[
                WriteOp::at(&files[0], 0, b"bad"),
                WriteOp::at(&foreign[0], 0, b"bad"),
            ],
            Default::default(),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(telemetry.snapshot().submissions, 1);
    let results = fs
        .read_batch(
            files.iter().map(|f| ReadOp::range(f, 0, 5)),
            Default::default(),
        )
        .unwrap();
    assert_eq!(results[0].data(), Some(b"badlo".as_slice()));
    assert_eq!(results[1].data(), Some(b"hello".as_slice()));
    assert_eq!(telemetry.snapshot().waves, 2);
    assert_eq!(telemetry.snapshot().completions, 3);
    fs.close_batch(&mut files).unwrap();
    assert!(files.iter().all(|f| f.is_closed()));
    assert!(
        fs.read_batch([ReadOp::range(&files[0], 0, 1)], Default::default())
            .is_err()
    );
    other.close_files(foreign).unwrap();
}

#[path = "support/read_results.rs"]
mod read_results;

#[test]
fn public_uring_mixed_read_outcomes_preserve_siblings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("reads")).unwrap();
    read_results::check(
        &Uring::new(root.path()).unwrap(),
        &Uring::new(root.path()).unwrap(),
        "/reads",
    );
}

#[test]
fn public_uring_sync_outcomes_preserve_valid_siblings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("syncs")).unwrap();
    vfsi_sync::test_support::public_sync_outcomes(
        &Uring::new(root.path()).unwrap(),
        &Uring::new(root.path()).unwrap(),
        "/syncs",
    );
}

#[test]
fn public_uring_close_outcomes_preserve_valid_siblings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("closes")).unwrap();
    vfsi_sync::test_support::public_close_outcomes(
        &Uring::new(root.path()).unwrap(),
        &Uring::new(root.path()).unwrap(),
        "/closes",
    );
}

#[path = "support/write_results.rs"]
mod write_results;

#[test]
fn public_uring_write_outcomes_preserve_siblings() {
    let root = tempfile::tempdir().unwrap();
    let fs = Uring::new(root.path()).unwrap();
    let other = Uring::new(root.path()).unwrap();
    fs.create_dir("/writes").unwrap();
    write_results::check(&fs, &other, "/writes", vfsi_core::FailureState::Failed);
}

#[path = "support/links.rs"]
mod links_support;

#[test]
fn public_uring_readlink_outcomes_preserve_siblings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("links")).unwrap();
    links_support::check_links_and_modes(&Uring::new(root.path()).unwrap(), "/links");
}

#[path = "support/removal_results.rs"]
mod removal_results;

#[test]
fn public_removal_results_preserve_independent_roots() {
    let root = tempfile::tempdir().unwrap();
    let fs = Uring::new(root.path()).unwrap();
    fs.create_dir("/removal").unwrap();
    removal_results::check(&fs, "/removal");
}

#[test]
fn public_uring_directory_outcomes_preserve_siblings_and_cursors() {
    let root = tempfile::tempdir().unwrap();
    let fs = Uring::new(root.path()).unwrap();
    let other = Uring::new(root.path()).unwrap();
    vfsi_sync::test_support::public_directory_results(&fs, &other, "/directory_results", true);
}

#[test]
fn public_uring_stream_outcomes_preserve_siblings_and_delivery_progress() {
    let root = tempfile::tempdir().unwrap();
    let fs = Uring::new(root.path()).unwrap();
    vfsi_sync::test_support::public_stream_results(&fs, "/uring_streams");
}
