#![cfg(target_os = "linux")]
use std::num::{NonZeroU32, NonZeroUsize};
use vfsi_core::api::{ReadOp, ReadOptions, SyncMode, WriteOp, WriteOptions};
use vfsi_core::{OpenFlags, OpenOp, Vfsi, VfsiExt};
use vfsi_uring::{Options, connect, connect_with_telemetry};

#[test]
fn whole_file_chunks_grow_after_short_siblings_finish() {
    const MIB: usize = 1024 * 1024;
    for sibling_bytes in [0, 4096] {
        for large_index in [0, 255] {
            let root = tempfile::tempdir().unwrap();
            let paths: Vec<_> = (0..256).map(|i| format!("/file-{i}")).collect();
            let large = vec![0x5a; 16 * MIB];
            let small = vec![0xa5; sibling_bytes];
            for (index, path) in paths.iter().enumerate() {
                let data = if index == large_index { &large } else { &small };
                std::fs::write(root.path().join(&path[1..]), data).unwrap();
            }
            let (fs, telemetry) = connect_with_telemetry(root.path(), Options::default()).unwrap();
            let results = fs
                .vread(
                    paths.iter().map(ReadOp::whole),
                    ReadOptions::new().with_max_total_bytes(
                        NonZeroUsize::new(large.len() + 255 * small.len()).unwrap(),
                    ),
                )
                .unwrap();
            assert_eq!(results.len(), paths.len());
            for (index, result) in results.iter().enumerate() {
                let expected = if index == large_index { &large } else { &small };
                assert_eq!(result.data(), Some(expected.as_slice()));
                assert!(result.eof());
            }
            let stats = telemetry.snapshot();
            // One 64 KiB request per sibling initially; nonempty short reads
            // need another round to establish EOF. The remaining large file
            // must then grow to 1 MiB, rather than keeping 64 KiB forever.
            let (submissions, waves) = if sibling_bytes == 0 {
                (273, 25)
            } else {
                (529, 33)
            };
            assert_eq!(
                stats.submissions, submissions,
                "sibling bytes={sibling_bytes}, large index={large_index}"
            );
            assert_eq!(stats.waves, waves);
            assert_eq!(stats.submissions, stats.completions);
            assert!(stats.peak_bytes <= 2 * MIB as u64);
        }
    }
}

#[test]
fn whole_file_paths_share_the_budget_and_submit_bounded_read_vectors() {
    for cached in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..8).map(|i| format!("/file-{i}")).collect();
        for (i, path) in paths.iter().enumerate() {
            std::fs::write(root.path().join(&path[1..]), vec![i as u8; 4096]).unwrap();
        }
        let (fs, telemetry) = connect_with_telemetry(
            root.path(),
            Options::default()
                .with_queue_depth(NonZeroU32::new(4).unwrap())
                .with_max_batch_bytes(NonZeroUsize::new(16384).unwrap())
                .with_cached_reads(cached),
        )
        .unwrap();
        let results = fs
            .vread(
                paths.iter().map(ReadOp::whole),
                ReadOptions::new().with_max_total_bytes(NonZeroUsize::new(32768).unwrap()),
            )
            .unwrap();
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.data(), Some(vec![i as u8; 4096].as_slice()));
            assert!(result.eof());
        }
        let stats = telemetry.snapshot();
        assert!(stats.peak_bytes <= 16384);
        assert_eq!(stats.submissions, stats.completions);
        if !cached {
            assert!(
                stats.submissions > stats.waves,
                "path contents must use actual vector submissions"
            );
            assert_eq!(
                stats.copied_read_bytes, 16384,
                "the first cohort uses scratch; the remaining budget lets the second use direct owned reads"
            );
        }
    }
}

#[test]
fn buffered_writes_report_unstable_until_explicit_sync() {
    for hybrid in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (fs, telemetry) =
            connect_with_telemetry(root.path(), Options::default().with_syscall_writes(hybrid))
                .unwrap();
        let files = fs
            .vopen(&[OpenOp::new(
                "/file",
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
            )])
            .unwrap();
        let results = fs
            .vwrite(
                &[WriteOp::at(&files[0], 0, b"payload")],
                WriteOptions::new(),
            )
            .unwrap();
        assert_eq!(results[0].bytes_written, 7);
        assert!(
            !results[0].stable,
            "buffered writes do not guarantee durability"
        );
        assert_eq!(
            telemetry.snapshot().submissions,
            u64::from(!hybrid),
            "write must not submit an implicit fsync"
        );
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"payload");
        for mode in [SyncMode::Data, SyncMode::All] {
            let before = telemetry.snapshot().submissions;
            fs.vfsync(&[&files[0]], mode).unwrap();
            assert_eq!(telemetry.snapshot().submissions, before + 1);
        }
        assert!(
            !results[0].stable,
            "later sync does not change the write's guarantee"
        );
        fs.close_files(files).unwrap();
        assert_eq!(
            telemetry.snapshot().submissions,
            telemetry.snapshot().completions
        );
    }
}

#[test]
fn read_write_sync_vectors_are_bounded_and_match_local() {
    for adaptive in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..7).map(|i| format!("/file-{i}")).collect();
        for (i, path) in paths.iter().enumerate() {
            std::fs::write(root.path().join(&path[1..]), vec![i as u8; 100 + i]).unwrap();
        }
        let options = Options::default()
            .with_cached_reads(adaptive)
            .with_syscall_writes(adaptive)
            .with_queue_depth(NonZeroU32::new(3).unwrap())
            .with_max_batch_bytes(NonZeroUsize::new(80).unwrap());
        let (fs, telemetry) = connect_with_telemetry(root.path(), options).unwrap();
        let local = vfsi_posix::connect(root.path()).unwrap();
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
            fs.vwrite(&writes, WriteOptions::new().with_write_all(true))
                .unwrap()
                .iter()
                .all(|r| r.bytes_written == 133)
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
            .all(|r| r.bytes_read() == 256 && r.data().is_none())
    );
    assert_eq!(buffers.concat(), bytes);
    assert_eq!(telemetry.snapshot().waves, 1);
    assert_eq!(telemetry.snapshot().submissions, 16);
    assert_eq!(telemetry.snapshot().copied_read_bytes, 4096);
    let owned = fs
        .vread([ReadOp::range(&files[0], 0, 4096)], ReadOptions::new())
        .unwrap();
    assert_eq!(owned[0].data(), Some(bytes.as_slice()));
    assert_eq!(
        telemetry.snapshot().copied_read_bytes,
        4096,
        "bounded owned reads must not copy through scratch"
    );
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
    assert_eq!(results[0].bytes_read(), 0);
    assert_eq!(results[1].bytes_read(), 0);
    let mut caller = [99; 4];
    let mixed = fs
        .vread(
            [
                ReadOp::range(&files[0], 0, 4),
                ReadOp::into(&files[0], 4, &mut caller),
                ReadOp::range(&files[0], 8, 4),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(mixed[0].data(), Some(&bytes[..4]));
    assert!(mixed[1].data().is_none());
    assert_eq!(caller, bytes[4..8]);
    assert_eq!(mixed[2].data(), Some(&bytes[8..12]));
    assert_eq!(
        telemetry.snapshot().copied_read_bytes,
        4100,
        "only the borrowed destination copies"
    );
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
        .vwrite(&writes, WriteOptions::new().with_write_all(true))
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let stats = telemetry.snapshot();
    assert_eq!(stats.submissions, 3);
    assert_eq!(stats.completions, 3);
    assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"new");
    assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"old");
    assert_eq!(std::fs::read(root.path().join("c")).unwrap(), b"new");
    assert_eq!(
        fs.vwrite(&[writes[0]], Default::default()).unwrap()[0].bytes_written,
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
                .with_queue_depth(NonZeroU32::new(depth).unwrap())
                .with_max_batch_bytes(NonZeroUsize::new(budget).unwrap()),
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
            .vwrite(&writes, WriteOptions::new().with_write_all(true))
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
            fs.vwrite(&[writes[0]], Default::default()).unwrap()[0].bytes_written,
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
    let budget = ReadOptions::new().with_max_total_bytes(NonZeroUsize::new(4).unwrap());
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
            Options::default().with_queue_depth(NonZeroU32::new(4097).unwrap())
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
        Options::default().with_queue_depth(NonZeroU32::new(3).unwrap()),
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
            .all(|(i, result)| result.offset == (i * 64) as u64 && result.bytes_written == 64)
    );
    assert_eq!(
        std::fs::read(root.path().join("file")).unwrap(),
        blocks.concat()
    );
    let stats = telemetry.snapshot();
    assert_eq!(
        (stats.waves, stats.submissions, stats.completions),
        (1, 1, 1),
        "adjacent writes to one descriptor must share one kernel transfer"
    );
    fs.close_files(files).unwrap();
}

#[test]
fn write_coalescing_respects_gaps_order_descriptors_and_wave_limits() {
    for (offsets, owners, depth, budget, waves, submissions) in [
        ([0, 3, 6], [0, 0, 0], 256, 32, 1, 1),
        ([0, 4, 8], [0, 0, 0], 256, 32, 1, 3),
        ([6, 3, 0], [0, 0, 0], 256, 32, 1, 3),
        ([0, 3, 6], [0, 1, 0], 256, 32, 1, 3),
        ([0, 3, 6], [0, 0, 0], 2, 32, 2, 2),
        ([0, 3, 6], [0, 0, 0], 256, 5, 2, 2),
        ([0, 3, 6], [0, 0, 0], 256, 1, 9, 9),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, [0; 16]).unwrap();
        let (fs, telemetry) = connect_with_telemetry(
            root.path(),
            Options::default()
                .with_queue_depth(NonZeroU32::new(depth).unwrap())
                .with_max_batch_bytes(NonZeroUsize::new(budget).unwrap()),
        )
        .unwrap();
        // Two opens of the same inode must not be merged across descriptors.
        let files = fs
            .vopen(&[
                OpenOp::new("/file", OpenFlags::WRITE),
                OpenOp::new("/file", OpenFlags::WRITE),
            ])
            .unwrap();
        let blocks = [*b"abc", *b"DEF", *b"ghi"];
        let writes: Vec<_> = (0..3)
            .map(|i| WriteOp::at(&files[owners[i]], offsets[i], &blocks[i]))
            .collect();
        let results = fs.vwrite(&writes, Default::default()).unwrap();
        let mut expected = [0; 16];
        for i in 0..3 {
            assert_eq!(
                (results[i].offset, results[i].bytes_written),
                (offsets[i], 3)
            );
            let start = offsets[i] as usize;
            expected[start..start + 3].copy_from_slice(&blocks[i]);
        }
        assert_eq!(std::fs::read(path).unwrap(), expected);
        let stats = telemetry.snapshot();
        assert_eq!(
            (stats.waves, stats.submissions, stats.completions),
            (waves, submissions, submissions)
        );
        assert!(stats.peak_bytes <= budget as u64);
        fs.close_files(files).unwrap();
    }
}
