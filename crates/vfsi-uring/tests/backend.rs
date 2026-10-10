#![cfg(target_os = "linux")]
use std::num::{NonZeroU32, NonZeroUsize};
use vfsi_core::api::{ReadOp, ReadOptions, SyncMode, WriteOp, WriteOptions};
use vfsi_core::{OpenFlags, OpenOp, Vfsi, VfsiExt};
use vfsi_uring::{Options, connect, connect_with_telemetry};

#[test]
fn read_write_sync_vectors_are_bounded_and_match_local() {
    let root = tempfile::tempdir().unwrap();
    let paths: Vec<_> = (0..7).map(|i| format!("/file-{i}")).collect();
    for (i, path) in paths.iter().enumerate() {
        std::fs::write(root.path().join(&path[1..]), vec![i as u8; 100 + i]).unwrap();
    }
    let options = Options::default()
        .queue_depth(NonZeroU32::new(3).unwrap())
        .max_batch_bytes(NonZeroUsize::new(80).unwrap());
    let (fs, telemetry) = connect_with_telemetry(root.path(), options).unwrap();
    let local = vfsi_sync::FsClient::new(
        vfsi_local::DummyVecFs::try_new(root.path().to_path_buf()).unwrap(),
    );
    let opens: Vec<_> = paths
        .iter()
        .map(|p| OpenOp::new(p, OpenFlags::READ | OpenFlags::WRITE))
        .collect();
    let files = fs.vopen(&opens).unwrap();
    let oracle = local.vopen(&opens).unwrap();
    let actual = fs
        .vread(
            files.iter().map(|file| ReadOp::range(file, 3, 200)),
            ReadOptions::new(),
        )
        .unwrap();
    let expected = local
        .vread(
            oracle.iter().map(|file| ReadOp::range(file, 3, 200)),
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(actual, expected);
    let data: Vec<_> = (0..7).map(|i| vec![20 + i; 133]).collect();
    let writes: Vec<_> = files
        .iter()
        .zip(&data)
        .map(|(f, b)| WriteOp::at(f, 0, b))
        .collect();
    assert!(
        fs.vwrite(&writes, WriteOptions::new().write_all(true))
            .unwrap()
            .iter()
            .all(|r| r.written == 133)
    );
    fs.vfsync(&files.iter().collect::<Vec<_>>(), SyncMode::Data)
        .unwrap();
    fs.vfsync(&files.iter().collect::<Vec<_>>(), SyncMode::All)
        .unwrap();
    let actual = fs
        .vread(
            files.iter().map(|file| ReadOp::range(file, 0, 200)),
            ReadOptions::new(),
        )
        .unwrap();
    for (result, data) in actual.iter().zip(&data) {
        assert_eq!(result.data(), Some(data.as_slice()));
    }
    fs.close_files(files).unwrap();
    local.close_files(oracle).unwrap();
    let stats = telemetry.snapshot();
    assert_eq!(stats.submissions, stats.completions);
    assert!(stats.peak_bytes <= 80);
    assert!(stats.submissions > stats.waves, "syncs must be batched");
}

#[test]
fn single_file_ranges_and_caller_buffers_are_batched_without_gaps() {
    let root = tempfile::tempdir().unwrap();
    let bytes: Vec<_> = (0..4096).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.path().join("file"), &bytes).unwrap();
    let (fs, telemetry) = connect_with_telemetry(root.path(), Options::default()).unwrap();
    let files = fs.vopen(&[OpenOp::new("/file", OpenFlags::READ)]).unwrap();
    let mut buffers = vec![vec![0; 256]; 16];
    let results = fs
        .vread(
            buffers
                .iter_mut()
                .enumerate()
                .map(|(i, b)| ReadOp::into(&files[0], (i * 256) as u64, b)),
            ReadOptions::new(),
        )
        .unwrap();
    assert!(
        results
            .iter()
            .all(|r| r.read() == 256 && r.data().is_none())
    );
    assert_eq!(buffers.concat(), bytes);
    assert_eq!(telemetry.snapshot().waves, 1);
    assert_eq!(telemetry.snapshot().submissions, 16);
    let results = fs
        .vread(
            [
                ReadOp::range(&files[0], 4096, 10),
                ReadOp::range(&files[0], 0, 0),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert!(results[0].eof());
    assert_eq!(results[0].read(), 0);
    assert_eq!(results[1].read(), 0);
    fs.close_files(files).unwrap();
}

#[test]
fn indexed_cqe_failure_drains_other_requests_and_ring_remains_usable() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(root.path().join(name), b"old").unwrap();
    }
    let (fs, telemetry) = connect_with_telemetry(root.path(), Options::default()).unwrap();
    let files = fs
        .vopen(&[
            OpenOp::new("/a", OpenFlags::WRITE),
            OpenOp::new("/b", OpenFlags::READ),
            OpenOp::new("/c", OpenFlags::WRITE),
        ])
        .unwrap();
    let writes: Vec<_> = files
        .iter()
        .map(|file| WriteOp::at(file, 0, b"new"))
        .collect();
    let error = fs
        .vwrite(&writes, WriteOptions::new().write_all(true))
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let stats = telemetry.snapshot();
    assert_eq!(stats.submissions, 3);
    assert_eq!(stats.completions, 3);
    assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"new");
    assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"old");
    assert_eq!(std::fs::read(root.path().join("c")).unwrap(), b"new");
    assert_eq!(
        fs.vwrite(&[writes[0]], Default::default()).unwrap()[0].written,
        3
    );
    fs.close_files(files).unwrap();
}

#[test]
fn known_failure_stops_unsent_waves_for_queue_and_byte_limits() {
    for (depth, budget) in [(1, 1024), (2, 1024), (256, 6)] {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(root.path().join(name), b"old").unwrap();
        }
        let (fs, telemetry) = connect_with_telemetry(
            root.path(),
            Options::default()
                .queue_depth(NonZeroU32::new(depth).unwrap())
                .max_batch_bytes(NonZeroUsize::new(budget).unwrap()),
        )
        .unwrap();
        let files = fs
            .vopen(&[
                OpenOp::new("/a", OpenFlags::WRITE),
                OpenOp::new("/b", OpenFlags::READ),
                OpenOp::new("/c", OpenFlags::WRITE),
            ])
            .unwrap();
        let writes: Vec<_> = files
            .iter()
            .map(|file| WriteOp::at(file, 0, b"new"))
            .collect();
        let error = fs
            .vwrite(&writes, WriteOptions::new().write_all(true))
            .unwrap_err();
        assert_eq!(error.index(), Some(1));
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"new");
        assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"old");
        assert_eq!(
            std::fs::read(root.path().join("c")).unwrap(),
            b"old",
            "depth={depth}, budget={budget}: an unsent write ran after failure"
        );
        let stats = telemetry.snapshot();
        assert_eq!((stats.submissions, stats.completions), (2, 2));
        assert_eq!(
            fs.vwrite(&[writes[0]], Default::default()).unwrap()[0].written,
            3
        );
        fs.close_files(files).unwrap();
    }
}

#[test]
fn overlapping_alias_writes_keep_order_instead_of_racing() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), b"000000").unwrap();
    std::fs::hard_link(root.path().join("a"), root.path().join("alias")).unwrap();
    let (fs, telemetry) = connect_with_telemetry(root.path(), Default::default()).unwrap();
    let files = fs
        .vopen(&[
            OpenOp::new("/a", OpenFlags::WRITE),
            OpenOp::new("/alias", OpenFlags::WRITE),
        ])
        .unwrap();
    fs.vwrite(
        &[
            WriteOp::at(&files[0], 0, b"AAAA"),
            WriteOp::at(&files[1], 2, b"BBBB"),
        ],
        Default::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"AABBBB");
    assert_eq!(
        telemetry.snapshot().submissions,
        0,
        "dependent writes use ordered fallback"
    );
    fs.close_files(files).unwrap();
}

#[test]
fn namespace_fallback_and_whole_file_limits_preserve_contracts() {
    let root = tempfile::tempdir().unwrap();
    let fs = connect(root.path(), Default::default()).unwrap();
    fs.write_files(&[("/a", b"hello".as_slice()), ("/b", b"world".as_slice())])
        .unwrap();
    let results = fs
        .vread(
            [ReadOp::whole("/a"), ReadOp::whole("/b")],
            Default::default(),
        )
        .unwrap();
    assert_eq!(results[0].data(), Some(b"hello".as_slice()));
    assert_eq!(results[1].data(), Some(b"world".as_slice()));
    let budget = ReadOptions::new().max_total_bytes(NonZeroUsize::new(4));
    assert!(fs.vread([ReadOp::whole("/a")], budget).is_err());
    assert_eq!(fs.read_dir("/").unwrap().len(), 2);
    fs.vrename(&[("/a", "/renamed")], Default::default())
        .unwrap();
    assert_eq!(fs.read_files(&["/renamed"]).unwrap()[0], b"hello");
}

#[test]
fn invalid_queue_depth_is_rejected_before_creating_root() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("not-created");
    assert!(
        connect(
            &path,
            Options::default().queue_depth(NonZeroU32::new(4097).unwrap())
        )
        .is_err()
    );
    assert!(!path.exists());
}

#[test]
fn queue_depth_partitions_vectors_into_exact_submission_waves() {
    let root = tempfile::tempdir().unwrap();
    let paths: Vec<_> = (0..7).map(|i| format!("/file-{i}")).collect();
    for path in &paths {
        std::fs::write(root.path().join(&path[1..]), vec![42; 32]).unwrap();
    }
    let (fs, telemetry) = connect_with_telemetry(
        root.path(),
        Options::default().queue_depth(NonZeroU32::new(3).unwrap()),
    )
    .unwrap();
    let files = fs
        .vopen(
            &paths
                .iter()
                .map(|path| OpenOp::new(path, OpenFlags::READ))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let results = fs
        .vread(
            files.iter().map(|file| ReadOp::range(file, 0, 32)),
            Default::default(),
        )
        .unwrap();
    assert!(
        results
            .iter()
            .all(|result| result.data() == Some(vec![42; 32].as_slice()))
    );
    let stats = telemetry.snapshot();
    assert_eq!(
        (stats.waves, stats.submissions, stats.completions),
        (3, 7, 7)
    );
    fs.close_files(files).unwrap();
}

#[test]
fn disjoint_writes_to_one_file_are_batched_at_their_actual_offsets() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), vec![0; 256]).unwrap();
    let (fs, telemetry) = connect_with_telemetry(root.path(), Default::default()).unwrap();
    let files = fs.vopen(&[OpenOp::new("/file", OpenFlags::WRITE)]).unwrap();
    let blocks: Vec<_> = (1..=4).map(|value| vec![value; 64]).collect();
    let ops: Vec<_> = blocks
        .iter()
        .enumerate()
        .map(|(i, bytes)| WriteOp::at(&files[0], (i * 64) as u64, bytes))
        .collect();
    let results = fs.vwrite(&ops, Default::default()).unwrap();
    assert!(
        results
            .iter()
            .enumerate()
            .all(|(i, result)| result.offset == (i * 64) as u64 && result.written == 64)
    );
    assert_eq!(
        std::fs::read(root.path().join("file")).unwrap(),
        blocks.concat()
    );
    let stats = telemetry.snapshot();
    assert_eq!(
        (stats.waves, stats.submissions, stats.completions),
        (1, 4, 4)
    );
    fs.close_files(files).unwrap();
}
