#![cfg(all(feature = "uring", target_os = "linux"))]
use vnfs::{OpenFlags, OpenOp, ReadOp, Uring, Vfsi, VfsiExt, WriteOp};
// This target runs with only the uring feature, so NFS cannot hide its guide.
#[allow(unused_imports)]
use vnfs::guides::uring as _;

#[test]
fn opaque_facade_batches_and_rejects_foreign_handles_before_io() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        std::fs::write(root.path().join(name), b"hello").unwrap();
    }
    let (fs, telemetry) = Uring::with_telemetry(root.path(), Default::default()).unwrap();
    let other = Uring::new(root.path()).unwrap();
    let mut files = fs
        .vopen(&[
            OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE),
            OpenOp::new("/b", OpenFlags::READ | OpenFlags::WRITE),
        ])
        .unwrap();
    let foreign = other
        .vopen(&[OpenOp::new("/a", OpenFlags::READ | OpenFlags::WRITE)])
        .unwrap();
    let error = fs
        .vwrite(
            &[
                WriteOp::at(&files[0], 0, b"bad"),
                WriteOp::at(&foreign[0], 0, b"bad"),
            ],
            Default::default(),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(telemetry.snapshot().submissions, 0);
    let results = fs
        .vread(
            files.iter().map(|f| ReadOp::range(f, 0, 5)),
            Default::default(),
        )
        .unwrap();
    assert!(
        results
            .iter()
            .all(|r| r.data() == Some(b"hello".as_slice()))
    );
    assert_eq!(telemetry.snapshot().waves, 1);
    assert_eq!(telemetry.snapshot().completions, 2);
    fs.vclose(&mut files).unwrap();
    assert!(files.iter().all(|f| f.is_closed()));
    assert!(
        fs.vread([ReadOp::range(&files[0], 0, 1)], Default::default())
            .is_err()
    );
    other.close_files(foreign).unwrap();
}
