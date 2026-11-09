#![doc = "Shared backend contract assertions for VFSI integration tests."]

//! A capability-aware shared test suite run against every [`VectorBackend`]
//! implementation. Portable behavior must match on all backends; optional
//! Unix semantics are asserted only when advertised.

use crate::backend::VectorBackend;
use crate::*;
use std::path::{Path, PathBuf};

mod stream_results;
pub use stream_results::public_stream_results;

mod directory_results;
pub use directory_results::public_directory_results;

mod copy_experiment;
pub use copy_experiment::copy_new_files_batched;

/// Borrow fixture storage for one vector dispatch without copying payloads.
/// Only available with `test-support`; this adds no production backend hook.
pub fn borrow_writes(writes: &[WriteOp]) -> Vec<WriteOp<&FileOperand, &[u8]>> {
    writes.iter().map(WriteOp::borrowed).collect()
}

/// Run a potentially deadlocking test in an isolated test-harness process.
/// Return `true` in the supervising parent and `false` in the child, which
/// must execute the actual test body. Killing a timed-out child avoids leaving
/// a stuck thread behind in the parent harness.
pub fn supervise_with_deadline(test: &str) -> bool {
    const CHILD: &str = "VFSI_DEADLINE_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    let executable = std::env::current_exe().unwrap();
    let inventory = std::process::Command::new(&executable)
        .args(["--exact", test, "--list"])
        .output()
        .expect("validate isolated test name");
    let output = String::from_utf8(inventory.stdout).unwrap();
    assert!(
        inventory.status.success() && output.lines().any(|line| line == format!("{test}: test")),
        "isolated test {test} was not found: {output}"
    );
    let mut child = std::process::Command::new(executable)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .spawn()
        .expect("spawn isolated regression test");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("poll regression test") {
            assert!(status.success(), "isolated test {test} failed: {status}");
            return true;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("test {test} exceeded its 30-second deadline (possible deadlock)");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Run a broad set of vectorized-filesystem assertions against `fs`, using
/// paths under `base` (which must be unique per caller).
pub fn run_suite(fs: &mut impl VectorBackend, base: &str) {
    let capabilities = fs.capabilities();
    let posix_metadata = capabilities.contains(Capabilities::POSIX_METADATA);
    let symlinks = capabilities.contains(Capabilities::SYMLINKS);
    let hardlinks = capabilities.contains(Capabilities::HARDLINKS);
    // SMB reports synthetic modes and delegates conventional creation modes
    // to the share ACL, so keep its shared conformance fixtures on defaults.
    let directory_mode = if posix_metadata { 0o755 } else { 0o777 };
    let file_mode = if posix_metadata { 0o644 } else { 0o666 };
    let subdirectory_mode = if posix_metadata { 0o750 } else { 0o777 };
    let dir = format!("{}/suite", base);
    fs.ensure_dir_impl(Path::new(&dir), directory_mode)
        .expect("ensure_dir");
    check_entry_removal_results(fs, &dir, directory_mode);
    let f = format!("{}/f.txt", dir);

    // vwrite_impl / vread_impl via paths.
    let payload = b"the quick brown fox jumps over the lazy dog".to_vec();
    let mut w = WriteOp::from_path(&f, VfOffset::At(0), payload.clone());
    w = w.with_creation();
    let wr = &fs
        .vwrite_impl(&borrow_writes(&[w]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .expect("writev")[0];
    assert_eq!(wr.bytes_written, payload.len());
    let error = fs
        .ensure_dir_impl(Path::new(&f), directory_mode)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);

    let r = &fs
        .vread_impl(&[ReadOp::from_path(&f, VfOffset::At(0), payload.len())])
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .expect("readv")[0];
    assert_eq!(r.data, payload);
    // eof is the backend's EOF signal: NFS reports it for an exact-to-EOF
    // read, the std::fs backend only for short reads. Either way no more data
    // is available, so only the data itself is asserted here.

    // Whole-file vectors are bounded by one aggregate caller-selected limit.
    let whole_a = format!("{}/whole-a", dir);
    let whole_b = format!("{}/whole-b", dir);
    fs.vwrite_impl(&borrow_writes(&[
        WriteOp::at(FileOperand::from_path(&whole_a), 0, b"abc".to_vec()).with_creation(),
        WriteOp::at(FileOperand::from_path(&whole_b), 0, b"def".to_vec()).with_creation(),
    ]))
    .and_then(|outcomes| {
        let count = outcomes.len();
        crate::internal::collect_vec_result(Ok(outcomes), count)
    })
    .unwrap();
    let whole_files = [
        FileOperand::from_path(&whole_a),
        FileOperand::from_path(&whole_b),
    ];
    // Exercise a real cross-file vector, not a loop of singleton reads.
    let ranges = fs
        .vread_impl(&[
            ReadOp::at(whole_files[1].clone(), 0, 3),
            ReadOp::at(whole_files[0].clone(), 0, 3),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(ranges.len(), 2);
    for (result, (file, data)) in ranges
        .iter()
        .zip([(&whole_files[1], b"def"), (&whole_files[0], b"abc")])
    {
        assert_eq!(&result.file, file);
        assert_eq!(result.offset, 0);
        assert_eq!(&result.data, data);
    }
    assert_eq!(
        fs.vread_all_with_options_impl(&whole_files, ReadAllOptions::new().with_max_total_bytes(6))
            .and_then(|outcomes| {
                let count = outcomes.len();
                crate::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap(),
        [b"abc".to_vec(), b"def".to_vec()]
    );
    let error = fs
        .vread_all_with_options_impl(&whole_files, ReadAllOptions::new().with_max_total_bytes(5))
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!(
        (error.index(), error.to_errno() as u32),
        (Some(1), libc::EFBIG as u32)
    );
    assert_eq!(
        ReadAllOptions::default().max_total_bytes(),
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES
    );
    assert_eq!(DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, 16 * 1024 * 1024);

    // stat / exists / file_type.
    let st = fs.stat_impl(Path::new(&f)).expect("stat");
    assert_eq!(st.size, payload.len() as u64);
    if posix_metadata {
        assert!(st.fileid != 0);
        assert!(st.nlink >= 1, "nlink");
        assert_eq!(st.rdev, 0, "rdev of a regular file");
        assert_eq!(fs.lstat_impl(Path::new(&f)).unwrap().fileid, st.fileid);
    }
    // Time attributes are populated (reported in `returned`) on both
    // backends; exact values depend on the filesystem clock semantics.
    if posix_metadata {
        let mut t = VfAttrs {
            file: FileOperand::from_path(&f),
            masks: AttrMask::ATIME | AttrMask::MTIME | AttrMask::CTIME,
            ..VfAttrs::default()
        };
        vfsi_core::internal::collect_vec_result(fs.vgetattrs_impl(std::slice::from_mut(&mut t)), 1)
            .unwrap();
        assert_eq!(
            t.returned,
            AttrMask::ATIME | AttrMask::MTIME | AttrMask::CTIME,
            "time attributes returned"
        );
    }
    // A 4 KiB file occupies at least one 512-byte block on both backends
    // (tmpfs reports 0 blocks for tiny files).
    if posix_metadata {
        let big = format!("{}/big.bin", dir);
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path(&big),
            0,
            vec![b'x'; 4096],
        )
        .with_creation()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                crate::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let mut b = VfAttrs {
            file: FileOperand::from_path(&big),
            masks: AttrMask::BLOCKS,
            ..VfAttrs::default()
        };
        vfsi_core::internal::collect_vec_result(fs.vgetattrs_impl(std::slice::from_mut(&mut b)), 1)
            .unwrap();
        assert!(b.returned.contains(AttrMask::BLOCKS), "blocks returned");
        assert!(b.blocks > 0, "blocks in 512-byte units");
    }
    assert!(fs.exists_impl(Path::new(&f)).unwrap());
    assert_eq!(fs.file_type_impl(Path::new(&f)).unwrap(), VfType::Regular);

    // setattrs: truncate to 5 bytes, then mode.
    {
        let batch_updates = &[VfAttrs {
            file: FileOperand::from_path(&f),
            masks: AttrMask::SIZE,
            size: 5,
            ..VfAttrs::default()
        }];
        vfsi_core::internal::collect_vec_result(
            fs.vsetattrs_raw_impl(batch_updates),
            batch_updates.len(),
        )
        .map(|_| ())
    }
    .expect("setattrsv size");
    assert_eq!(fs.stat_impl(Path::new(&f)).unwrap().size, 5);

    // mkdir.
    let sub = format!("{}/sub", dir);
    fs.mkdir_raw_impl(Path::new(&sub), subdirectory_mode)
        .expect("mkdir");
    assert_eq!(
        fs.stat_impl(Path::new(&sub)).unwrap().ftype,
        VfType::Directory
    );
    if posix_metadata {
        assert_eq!(fs.stat_impl(Path::new(&sub)).unwrap().mode & 0o777, 0o750);
    }

    // open / descriptor write / fseek / descriptor read.
    let tf = fs
        .open_raw_impl(Path::new(&f), libc::O_RDWR, 0)
        .expect("open");
    assert_eq!(fs.fstat_impl(&tf).unwrap().size, 5);
    fs.vwrite_impl(&borrow_writes(&[WriteOp::from_fd(
        tf.fd().unwrap(),
        VfOffset::At(0),
        b"hello".to_vec(),
    )]))
    .and_then(|outcomes| {
        let count = outcomes.len();
        crate::internal::collect_vec_result(Ok(outcomes), count)
    })
    .expect("writev fd");
    assert_eq!(fs.seek_raw_impl(&tf, 0, SeekFrom::Set).unwrap(), 0);
    let r = &fs
        .vread_impl(&[ReadOp::from_fd(tf.fd().unwrap(), VfOffset::Cur, 5)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .expect("readv fd")[0];
    assert_eq!(r.data, b"hello");
    fs.close_impl(&tf).expect("close");

    if symlinks {
        // symlink / readlink.
        let link = format!("{}/ln", dir);
        fs.symlink_raw_impl(Path::new(&f), Path::new(&link))
            .expect("symlink");
        assert_eq!(
            fs.readlink_raw_impl(Path::new(&link)).unwrap(),
            f.as_bytes()
        );

        // stat follows symlinks; lstat does not. (Relative target so both the
        // NFS and std::fs backends can resolve it.)
        let sbase = format!("{}/lnstat", dir);
        let rel = std::path::Path::new(&f)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        fs.symlink_raw_impl(Path::new(&rel), Path::new(&sbase))
            .expect("symlink lnstat");
        assert_eq!(
            fs.stat_impl(Path::new(&sbase)).unwrap().ftype,
            VfType::Regular,
            "stat follows"
        );
        assert_eq!(
            fs.lstat_impl(Path::new(&sbase)).unwrap().ftype,
            VfType::Symlink,
            "lstat stays"
        );
        assert_eq!(
            fs.stat_impl(Path::new(&sbase)).unwrap().size,
            fs.stat_impl(Path::new(&f)).unwrap().size,
            "stat resolves to the target"
        );
    }

    // hardlink.
    if hardlinks {
        let hard = format!("{}/hard", dir);
        fs.vhardlink_impl(&[Path::new(&f)], &[Path::new(&hard)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
            })
            .expect("hardlinkv");
        assert_eq!(
            fs.stat_impl(Path::new(&f)).unwrap().fileid,
            fs.stat_impl(Path::new(&hard)).unwrap().fileid
        );
    }

    // rename.
    let renamed = format!("{}/renamed.txt", dir);
    {
        let rename_pairs = &[(
            FileOperand::from_os_path(Path::new(&f)),
            FileOperand::from_os_path(Path::new(&renamed)),
        )];
        vfsi_core::internal::collect_vec_result(fs.vrename_impl(rename_pairs), rename_pairs.len())
            .map(|_| ())
    }
    .expect("renamev");
    assert!(fs.exists_impl(Path::new(&renamed)).unwrap());
    assert!(!fs.exists_impl(Path::new(&f)).unwrap());

    // Strict vopen_raw_impl / vclose_impl.
    let more = format!("{}/more", dir);
    fs.ensure_dir_impl(Path::new(&more), directory_mode)
        .unwrap();
    let paths = [
        format!("{}/a", more),
        format!("{}/b", more),
        format!("{}/c", more),
    ];
    let refs: Vec<&Path> = paths.iter().map(Path::new).collect();
    let files = crate::backend::helpers::open_raw_batch(
        fs,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[file_mode; 3],
    )
    .expect("openv");
    assert!(
        fs.vclose_impl(&files)
            .expect("closev")
            .into_iter()
            .all(|result| result.is_ok())
    );

    // Independent native OPEN outcomes retain both successful owners.
    let siblings = [
        format!("{more}/first"),
        format!("{more}/absent/entry"),
        format!("{more}/last"),
    ];
    let refs: Vec<_> = siblings.iter().map(Path::new).collect();
    let outcomes = fs
        .vopen_raw_impl(
            &refs,
            &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 3],
            &[file_mode; 3],
        )
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    let mut opened = Vec::new();
    for (index, outcome) in outcomes.into_iter().enumerate() {
        if index == 1 {
            let error = outcome.unwrap_err();
            assert_eq!(error.state(), vfsi_core::FailureState::Failed);
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        } else {
            let file = outcome.unwrap();
            assert_eq!(fs.write_raw_impl(&file, 0, b"owned").unwrap(), 5);
            assert_eq!(fs.read_raw_impl(&file, 0, 5).unwrap(), b"owned");
            opened.push(file);
        }
    }
    assert!(
        fs.vclose_impl(&opened)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );

    // listdir (non-recursive) finds entries.
    let entries = fs
        .listdir_impl(Path::new(&dir), AttrMask::default(), 0, false)
        .expect("listdir");
    assert!(
        entries
            .iter()
            .any(|e| e.file.path().unwrap().ends_with("renamed.txt"))
    );
    assert!(
        entries
            .iter()
            .any(|e| e.file.path().unwrap().ends_with("sub"))
    );

    // vlistdirs_impl callback.
    let mut seen = 0usize;
    let mut cb = |_: &VfAttrs, _: &Path| {
        seen += 1;
        true
    };
    fs.vlistdirs_impl(&[Path::new(&dir)], AttrMask::default(), 0, false, &mut cb)
        .expect("listdirv");
    assert!(seen >= 2);

    // dupv extent copy (whole file).
    let copy = format!("{}/copy.txt", dir);
    {
        let copy_pairs = &[ExtentPair::new(&renamed, 0, &copy, 0, None)];
        vfsi_core::internal::collect_vec_result(fs.vcopy_data_impl(copy_pairs), copy_pairs.len())
            .map(|_| ())
    }
    .expect("dupv");
    assert_eq!(
        fs.stat_impl(Path::new(&copy)).unwrap().size,
        fs.stat_impl(Path::new(&renamed)).unwrap().size
    );

    // dupv over a longer existing destination truncates the stale tail.
    let short = format!("{}/short.txt", dir);
    let long = format!("{}/long.txt", dir);
    fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
        FileOperand::from_path(&short),
        0,
        b"ab".to_vec(),
    )
    .with_creation()]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
        FileOperand::from_path(&long),
        0,
        b"abcdef".to_vec(),
    )
    .with_creation()]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    {
        let copy_pairs = &[ExtentPair::new(&short, 0, &long, 0, None)];
        vfsi_core::internal::collect_vec_result(fs.vcopy_data_impl(copy_pairs), copy_pairs.len())
            .map(|_| ())
    }
    .unwrap();
    let st = fs.stat_impl(Path::new(&long)).unwrap();
    assert_eq!(st.size, 2, "dupv truncates the destination");
    assert_eq!(
        fs.read_raw_impl(&FileOperand::from_path(&long), 0, 8)
            .unwrap(),
        b"ab"
    );

    // O_CREAT mode is ignored when the file already exists.
    let mode_f = format!("{}/mode.txt", dir);
    let mfd = fs
        .open_raw_impl(
            Path::new(&mode_f),
            libc::O_CREAT | libc::O_RDWR,
            if posix_metadata { 0o600 } else { 0o666 },
        )
        .unwrap();
    fs.close_impl(&mfd).unwrap();
    let mfd = fs
        .open_raw_impl(Path::new(&mode_f), libc::O_CREAT | libc::O_RDWR, file_mode)
        .unwrap();
    fs.close_impl(&mfd).unwrap();
    if posix_metadata {
        assert_eq!(
            fs.stat_impl(Path::new(&mode_f)).unwrap().mode & 0o7777,
            0o600,
            "O_CREAT must not chmod an existing file"
        );
    }

    // O_CREAT | O_EXCL fails on an existing file; O_TRUNC empties it.
    assert_eq!(
        fs.open_raw_impl(
            Path::new(&mode_f),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            file_mode
        )
        .unwrap_err()
        .to_errno() as u32,
        ERR_EXIST
    );
    let tfd = fs
        .open_raw_impl(Path::new(&mode_f), libc::O_RDWR | libc::O_TRUNC, 0)
        .unwrap();
    fs.close_impl(&tfd).unwrap();
    assert_eq!(
        fs.stat_impl(Path::new(&mode_f)).unwrap().size,
        0,
        "O_TRUNC empties the file"
    );

    // "Current position" offsets are invalid for path-based operations.
    assert_eq!(
        fs.vread_impl(&[ReadOp::new(
            FileOperand::from_path(&renamed),
            VfOffset::Cur,
            1
        )])
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err()
        .to_errno() as u32,
        ERR_INVAL
    );

    // Mixed descriptor/path batches work and echo the original files.
    let rfd = fs
        .open_raw_impl(Path::new(&renamed), libc::O_RDONLY, 0)
        .unwrap();
    let mixed = fs
        .vread_impl(&[
            ReadOp::new(rfd.clone(), VfOffset::At(0), 2),
            ReadOp::at(FileOperand::from_path(&copy), 0, 2),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(mixed[0].data.len(), 2);
    assert_eq!(mixed[1].data.len(), 2);
    assert!(mixed[0].file.is_fd(), "result echoes the fd op");
    assert!(mixed[1].file.path().is_some(), "result echoes the path op");
    fs.close_impl(&rfd).unwrap();

    // A closed descriptor inside a batch fails at its own index with EBADF.
    let bad = fs
        .open_raw_impl(Path::new(&renamed), libc::O_RDONLY, 0)
        .unwrap();
    fs.close_impl(&bad).unwrap();
    let e = fs
        .vread_impl(&[
            ReadOp::at(FileOperand::from_path(&copy), 0, 1),
            ReadOp::new(bad, VfOffset::At(0), 1),
        ])
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap_err();
    assert_eq!((e.index(), e.to_errno() as u32), (Some(1), ERR_EBADF));

    // readlink/hardlink error paths.
    assert!(
        fs.readlink_raw_impl(Path::new(&renamed)).is_err(),
        "readlink of a regular file"
    );
    assert!(
        fs.vhardlink_impl(
            &[Path::new("/no/such/source")],
            &[Path::new(&format!("{}/h", dir))]
        )
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .is_err(),
        "hardlink of a missing source"
    );

    // ensure_dir on an existing directory is a no-op.
    fs.ensure_dir_impl(Path::new(&sub), directory_mode)
        .expect("ensure_dir existing");

    // Non-recursive rm of a non-empty directory fails.
    let nonempty = format!("{}/nonempty", dir);
    fs.ensure_dir_impl(Path::new(&nonempty), directory_mode)
        .unwrap();
    fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
        FileOperand::from_os_path(Path::new(&format!("{}/x", nonempty))),
        0,
        b"x".to_vec(),
    )
    .with_creation()]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert!(
        {
            let remove_roots: &[&std::path::Path] = &[Path::new(&nonempty)];
            vfsi_core::internal::collect_vec_result(
                fs.remove_paths_impl(remove_roots, false),
                remove_roots.len(),
            )
            .map(|_| ())
        }
        .is_err()
    );

    // write_adb: two blocks with ADBN at block offset 0.
    let adbf = format!("{}/adb.bin", dir);
    let mut a = Adb::blocknum_only(&adbf, 0, 1024, 2, 0, 100);
    a.adb_reloff_pattern = Some(8);
    a.adb_pattern_data = b"PAT".to_vec();
    fs.vwrite_adb_impl(std::slice::from_mut(&mut a))
        .expect("write_adb");
    assert_eq!(a.adb_block_count, 2);
    let adb = fs
        .read_raw_impl(&FileOperand::from_path(&adbf), 0, 2048)
        .unwrap();
    assert_eq!(&adb[0..8], &100u64.to_be_bytes(), "ADBN block 0");
    assert_eq!(&adb[1024..1032], &101u64.to_be_bytes(), "ADBN block 1");
    assert_eq!(&adb[8..11], b"PAT", "pattern block 0");
    assert_eq!(&adb[1032..1035], b"PAT", "pattern block 1");

    // cp_recursive copies the tree.
    let cpsrc = format!("{}/cpsrc", dir);
    let cpdst = format!("{}/cpdst", dir);
    fs.ensure_dir_impl(Path::new(&format!("{}/inner", cpsrc)), directory_mode)
        .unwrap();
    let srcfile = format!("{}/inner/data.txt", cpsrc);
    let mut w = WriteOp::from_path(&srcfile, VfOffset::At(0), b"xyz".to_vec());
    w = w.with_creation();
    fs.vwrite_impl(&borrow_writes(&[w]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    if symlinks {
        fs.symlink_raw_impl(
            Path::new("data.txt"),
            Path::new(&format!("{}/inner/link", cpsrc)),
        )
        .unwrap();
    }
    fs.copy_tree_impl(Path::new(&cpsrc), Path::new(&cpdst), true, false)
        .expect("cp_recursive");
    assert!(
        fs.exists_impl(Path::new(&format!("{}/inner/data.txt", cpdst)))
            .unwrap()
    );
    if symlinks {
        assert_eq!(
            fs.lstat_impl(Path::new(&format!("{}/inner/link", cpdst)))
                .unwrap()
                .ftype,
            VfType::Symlink,
            "cp_recursive(symlinks=true) recreates the link"
        );
        let cpflat = format!("{}/cpflat", dir);
        fs.copy_tree_impl(Path::new(&cpsrc), Path::new(&cpflat), false, false)
            .expect("cp_recursive no symlinks");
        assert_eq!(
            fs.lstat_impl(Path::new(&format!("{}/inner/link", cpflat)))
                .unwrap()
                .ftype,
            VfType::Regular,
            "cp_recursive(symlinks=false) copies through the link"
        );
        assert_eq!(
            fs.read_raw_impl(
                &FileOperand::from_os_path(Path::new(&format!("{}/inner/link", cpflat))),
                0,
                3
            )
            .unwrap(),
            b"xyz"
        );
    }

    // "." and ".." path components resolve identically in both backends.
    let base_name = std::path::Path::new(&dir)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        fs.stat_impl(Path::new(&format!("{}/./renamed.txt", dir)))
            .unwrap()
            .size,
        fs.stat_impl(Path::new(&renamed)).unwrap().size,
        "dot component"
    );
    assert_eq!(
        fs.stat_impl(Path::new(&format!("{}/../{}/renamed.txt", dir, base_name)))
            .unwrap()
            .size,
        fs.stat_impl(Path::new(&renamed)).unwrap().size,
        "dotdot component stays inside the tree"
    );

    // chdir onto a regular file is refused (NOTDIR) by both backends.
    assert_eq!(
        fs.chdir(Path::new(&renamed)).unwrap_err().to_errno() as u32,
        ERR_NOTDIR,
        "chdir to a file"
    );

    // O_APPEND writes always go to the end of the file.
    let app = format!("{}/append.txt", dir);
    let afd = fs
        .open_raw_impl(
            Path::new(&app),
            libc::O_CREAT | libc::O_RDWR | libc::O_APPEND,
            file_mode,
        )
        .expect("open append");
    fs.vwrite_impl(&borrow_writes(&[WriteOp::new(
        afd.clone(),
        VfOffset::At(0),
        b"ab".to_vec(),
    )]))
    .and_then(|outcomes| {
        let count = outcomes.len();
        crate::internal::collect_vec_result(Ok(outcomes), count)
    })
    .unwrap();
    fs.vwrite_impl(&borrow_writes(&[WriteOp::new(
        afd.clone(),
        VfOffset::At(0),
        b"cd".to_vec(),
    )]))
    .and_then(|outcomes| {
        let count = outcomes.len();
        crate::internal::collect_vec_result(Ok(outcomes), count)
    })
    .unwrap();
    let got = fs
        .read_raw_impl(&FileOperand::from_path(&app), 0, 8)
        .unwrap();
    assert_eq!(got, b"abcd", "O_APPEND appends regardless of offset");
    // Cursor-based append reports and advances to the real append position,
    // independently of the initial cursor value.
    let w0 = fs
        .vwrite_impl(&borrow_writes(&[WriteOp::new(
            afd.clone(),
            VfOffset::Cur,
            b"e".to_vec(),
        )]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            crate::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(w0[0].offset, 4, "append write reports the real offset");
    assert_eq!(fs.seek_raw_impl(&afd, 0, SeekFrom::Cur).unwrap(), 5);
    fs.close_impl(&afd).unwrap();
    assert_eq!(
        fs.read_raw_impl(&FileOperand::from_path(&app), 0, 8)
            .unwrap(),
        b"abcde"
    );

    // dupv follows a symlink source and copies the target's data.
    if symlinks {
        let dup_link = format!("{}/dup_link", dir);
        let dup_copy = format!("{}/dup_copy", dir);
        let dup_target = format!("{}/dup_target", dir);
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_path(&dup_target),
            0,
            b"linkdata".to_vec(),
        )
        .with_creation()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                crate::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let rel_name = std::path::Path::new(&dup_target)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        fs.symlink_raw_impl(Path::new(&rel_name), Path::new(&dup_link))
            .unwrap();
        {
            let copy_pairs = &[ExtentPair::new(&dup_link, 0, &dup_copy, 0, None)];
            vfsi_core::internal::collect_vec_result(
                fs.vcopy_data_impl(copy_pairs),
                copy_pairs.len(),
            )
            .map(|_| ())
        }
        .unwrap();
        let st = fs.stat_impl(Path::new(&dup_copy)).unwrap();
        assert_eq!(st.ftype, VfType::Regular, "dupv copies target data");
        assert_eq!(st.size, 8);
        assert_eq!(
            fs.read_raw_impl(&FileOperand::from_path(&dup_copy), 0, 8)
                .unwrap(),
            b"linkdata"
        );

        // lsetattrsv refuses symlinks instead of pretending to set them.
        let lsa = VfAttrs {
            file: FileOperand::from_path(&dup_link),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..VfAttrs::default()
        };
        assert_eq!(
            {
                let batch_updates = &[lsa];
                vfsi_core::internal::collect_vec_result(
                    fs.vsetattrs_raw_nofollow_impl(batch_updates),
                    batch_updates.len(),
                )
                .map(|_| ())
            }
            .unwrap_err()
            .to_errno() as u32,
            libc::EOPNOTSUPP as u32,
            "lsetattrsv on a symlink"
        );
    }

    // Closing an already-closed descriptor reports EBADF on both backends.
    assert_eq!(
        fs.close_impl(&tf).unwrap_err().to_errno() as u32,
        ERR_EBADF,
        "close of a closed descriptor"
    );

    // open_path_impl with an Abs base and a relative-looking path is
    // root-relative in both backends.
    let abs_rel = format!("{}/absrel.txt", base.trim_start_matches('/'));
    let f2 = fs
        .open_path_impl(
            VfPathBase::Abs,
            Path::new(&abs_rel),
            libc::O_CREAT | libc::O_RDWR,
            file_mode,
        )
        .expect("open_by_path Abs relative");
    fs.vwrite_impl(&borrow_writes(&[WriteOp::new(
        f2.clone(),
        VfOffset::At(0),
        b"ar".to_vec(),
    )]))
    .and_then(|outcomes| {
        let count = outcomes.len();
        crate::internal::collect_vec_result(Ok(outcomes), count)
    })
    .unwrap();
    fs.close_impl(&f2).unwrap();
    assert_eq!(
        fs.read_raw_impl(
            &FileOperand::from_os_path(Path::new(&format!("/{}", abs_rel))),
            0,
            2
        )
        .unwrap(),
        b"ar"
    );

    // Writing through `Current(None)` (the cwd itself) is not a file op.
    assert_eq!(
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::cwd(),
            0,
            b"x".to_vec()
        )
        .with_creation()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                crate::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap_err()
            .to_errno() as u32,
        ERR_ISDIR,
        "write to Current(None)"
    );

    // walk returns pre-order with the sort callback applied to every
    // directory (subdirectories visited in the order the caller lists them).
    let wroot = format!("/{}/wroot", dir.trim_start_matches('/'));
    fs.ensure_dir_impl(Path::new(&wroot), directory_mode)
        .unwrap();
    for (sub, file) in [("b", "f1"), ("a", "f2"), ("a", "f3")] {
        let subp = format!("{}/{}", wroot, sub);
        fs.ensure_dir_impl(Path::new(&subp), directory_mode)
            .unwrap();
        fs.vwrite_impl(&borrow_writes(&[WriteOp::at(
            FileOperand::from_os_path(Path::new(&format!("{}/{}", subp, file))),
            0,
            b"x".to_vec(),
        )
        .with_creation()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                crate::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
    }
    let mut sort = |_dir: &Path, attrs: &mut Vec<VfAttrs>| {
        attrs.sort_by(|a, b| {
            a.file
                .path()
                .unwrap()
                .file_name()
                .cmp(&b.file.path().unwrap().file_name())
        });
    };
    let tree = fs
        .walk_impl(Path::new(&wroot), AttrMask::stat(), &mut sort)
        .unwrap();
    let order: Vec<PathBuf> = tree.iter().map(|w| w.path.clone()).collect();
    assert_eq!(
        order,
        vec![
            PathBuf::from(&wroot),
            PathBuf::from(format!("{}/a", wroot)),
            PathBuf::from(format!("{}/b", wroot)),
        ],
        "walk pre-order follows the sorted subdirectory order"
    );
    for w in &tree {
        let names: Vec<_> = w
            .entries
            .iter()
            .map(|e| e.file.path().unwrap().file_name().unwrap().to_owned())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "entries are sorted in {}", w.path.display());
    }

    // chdir / getcwd.
    fs.chdir(Path::new(&sub)).expect("chdir");
    assert_eq!(fs.getcwd().file_name(), Some(Path::new("sub").as_os_str()));

    // Generic recursive removal requires no-follow metadata. Backends without
    // LSTAT must reject it without changing the tree.
    if capabilities.contains(Capabilities::LSTAT) {
        {
            let remove_roots: &[&std::path::Path] = &[Path::new(&dir)];
            vfsi_core::internal::collect_vec_result(
                fs.remove_paths_impl(remove_roots, true),
                remove_roots.len(),
            )
            .map(|_| ())
        }
        .expect("rm recursive");
        assert!(!fs.exists_impl(Path::new(&dir)).unwrap());
    } else {
        assert_eq!(
            {
                let remove_roots: &[&std::path::Path] = &[Path::new(&dir)];
                vfsi_core::internal::collect_vec_result(
                    fs.remove_paths_impl(remove_roots, true),
                    remove_roots.len(),
                )
                .map(|_| ())
            }
            .unwrap_err()
            .to_errno() as u32,
            libc::EOPNOTSUPP as u32
        );
        assert!(fs.exists_impl(Path::new(&dir)).unwrap());
    }
}

/// Shared page-admission conformance for local, mount-routed and network clients.
/// `fixture_fs` prepares the namespace before traversal: for Auto over NFS,
/// use the kernel mount here rather than assuming kernel/direct cache coherence.
/// SMB's legacy snapshot listing cannot enforce no-follow descendant lookup;
/// `recursive_supported = false` asserts that limitation instead of bypassing it.
pub fn run_pruning_suite(
    fs: &impl Vfsi,
    fixture_fs: &impl Vfsi,
    base: &str,
    recursive_supported: bool,
) {
    use crate::api::{DepthLimit, ListDirControl, MkDirOp};
    use std::collections::HashSet;
    fixture_fs.create_dir(base).unwrap();
    let roots = [format!("{base}/a"), format!("{base}/b")];
    for root in &roots {
        fixture_fs.create_dir(root).unwrap();
    }
    let children: Vec<_> = (0..270)
        .map(|i| PathBuf::from(format!("{}/d{i:03}", roots[0])))
        .collect();
    fixture_fs
        .vmkdir(
            &children
                .iter()
                .map(|path| MkDirOp::new(path, 0o777))
                .collect::<Vec<_>>(),
        )
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap();
    fixture_fs
        .write(format!("{}/file", roots[1]), b"x")
        .unwrap();
    let options = ListDirOptions::new();
    let mut counts = [0; 2];
    let mut pages = 0;
    let done = fs
        .listdirs_all(&roots, options, |index, page| {
            assert_eq!(page.path, Path::new(&roots[index]));
            counts[index] += page.entries.len();
            pages += 1;
            // Callbacks are unlocked; nested metadata must not deadlock.
            assert!(fs.attrs(&page.path)?.is_dir());
            Ok(ListDirControl::Continue)
        })
        .unwrap();
    assert_eq!(done, [TraversalCompletion::Complete; 2]);
    assert_eq!(counts, [270, 1]);
    assert!(pages >= 4, "fixture must cross directory page boundaries");
    for policy in [
        options.with_max_entries(270),
        options.with_max_path_bytes(children.iter().map(|p| p.as_os_str().len()).sum()),
    ] {
        let mut delivered_entries = 0;
        let mut delivered_bytes = 0;
        let error = fs
            .listdirs_all(&roots, policy, |_, page| {
                delivered_entries += page.entries.len();
                delivered_bytes += page
                    .entries
                    .iter()
                    .map(|e| e.path().as_os_str().len())
                    .sum::<usize>();
                Ok(ListDirControl::Continue)
            })
            .unwrap_err();
        // Backends can interleave root pages differently. The error must
        // identify the root of the rejected entry, not an assumed fetch order.
        let owner = error.index().expect("budget error retains root index");
        assert!(owner < roots.len());
        assert!(error.path().unwrap().starts_with(&roots[owner]));
        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
        assert!(delivered_entries <= 270);
        if policy.max_entries().is_some() {
            assert_eq!(delivered_entries, 270);
        }
        if let Some(max) = policy.max_path_bytes() {
            assert!(delivered_bytes <= max);
        }
    }
    for fail in [false, true] {
        let mut calls = 0;
        let result = fs.listdirs_all(
            &roots,
            options.with_recursive(recursive_supported),
            |_, _| {
                calls += 1;
                if fail {
                    Err(VfError::errno(17, libc::ECANCELED as u32))
                } else {
                    Ok(ListDirControl::Stop)
                }
            },
        );
        assert_eq!(
            calls,
            if fail { roots.len() } else { 1 },
            "callback errors isolate roots; explicit stop ends delivery without replay"
        );
        if fail {
            let error = result.unwrap_err();
            assert_eq!(error.index(), Some(0));
            assert_eq!(error.raw_code(), libc::ECANCELED as u32);
        } else {
            assert_eq!(result.unwrap(), [TraversalCompletion::Stopped; 2]);
        }
    }
    if !recursive_supported {
        let error = fs
            .listdirs_all(
                &roots,
                options.with_recursive(true),
                |_, _| -> VfResult<ListDirControl> {
                    panic!("unsupported recursive safety contract must fail before callbacks");
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        return;
    }
    // Pruning on *every* page precedes the depth check and all child I/O,
    // even on backends which cannot safely reopen a child directory.
    fs.listdirs_all(
        &roots[..1],
        options
            .with_recursive(true)
            .with_max_depth(DepthLimit::new(0).unwrap()),
        |_, page| {
            assert_eq!(page.path, Path::new(&roots[0]));
            Ok(ListDirControl::Prune(
                page.entries
                    .iter()
                    .map(|e| e.path().to_path_buf())
                    .collect(),
            ))
        },
    )
    .unwrap();
    let mut admitted = HashSet::new();
    let mut pruned = HashSet::new();
    let mut visited = HashSet::new();
    let result = fs.listdirs_all(&roots[..1], options.with_recursive(true), |_, page| {
        visited.insert(page.path.clone());
        let mut excluded = Vec::new();
        for entry in page.entries {
            if entry
                .file_name()
                .unwrap()
                .as_encoded_bytes()
                .last()
                .unwrap()
                % 2
                == 0
            {
                pruned.insert(entry.path().to_path_buf());
                excluded.push(entry.path().to_path_buf());
            } else {
                admitted.insert(entry.path().to_path_buf());
            }
        }
        Ok(ListDirControl::Prune(excluded))
    });
    assert_eq!(result.unwrap(), [TraversalCompletion::Complete]);
    assert!(!admitted.is_empty() && !pruned.is_empty());
    assert!(pruned.is_disjoint(&visited));
    admitted.insert(PathBuf::from(&roots[0]));
    assert_eq!(
        visited, admitted,
        "visit exactly the admitted directory set"
    );
}

/// Independent native READ outcomes, without a strict collection adapter.
pub fn read_vector_outcomes<F: VectorBackend>(fs: &mut F, directory: &Path) {
    fs.mkdir_raw_impl(directory, 0o777).unwrap();
    let a = directory.join("a");
    let b = directory.join("b");
    let missing = directory.join("missing");
    fs.vwrite_impl(&borrow_writes(&[
        WriteOp::from_os_path(&a, VfOffset::At(0), b"abcd".to_vec())
            .with_creation()
            .with_truncate(),
        WriteOp::from_os_path(&b, VfOffset::At(0), b"efgh".to_vec())
            .with_creation()
            .with_truncate(),
    ]))
    .and_then(|outcomes| {
        let count = outcomes.len();
        crate::internal::collect_vec_result(Ok(outcomes), count)
    })
    .unwrap();
    let outcomes = fs
        .vread_impl(&[
            ReadOp::from_os_path(&a, VfOffset::At(0), 4),
            ReadOp::from_os_path(&missing, VfOffset::At(0), 4),
            ReadOp::from_os_path(&b, VfOffset::At(0), 4),
            ReadOp::from_os_path(&a, VfOffset::At(u64::MAX), 4),
        ])
        .unwrap();
    assert_eq!(outcomes.len(), 4);
    assert_eq!(outcomes[0].as_ref().unwrap().data, b"abcd");
    assert_eq!(outcomes[2].as_ref().unwrap().data, b"efgh");
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().cause().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(outcomes[1].as_ref().unwrap_err().cause().index(), None);
    assert_eq!(
        outcomes[3].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    let mixed = fs
        .vread_impl(&[
            ReadOp::from_os_path(&a, VfOffset::At(0), 4),
            ReadOp::at(FileOperand::from_fd(i32::MAX), 0, 4),
            ReadOp::from_os_path(&b, VfOffset::At(0), 4),
        ])
        .unwrap();
    assert_eq!(mixed.len(), 3);
    assert_eq!(mixed[0].as_ref().unwrap().data, b"abcd");
    assert_eq!(mixed[2].as_ref().unwrap().data, b"efgh");
    assert_eq!(
        mixed[1].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(fs.vread_impl(&[]).unwrap().is_empty());
    let reads = [
        ReadOp::from_os_path(&a, VfOffset::At(0), 4),
        ReadOp::from_os_path(&missing, VfOffset::At(0), 4),
        ReadOp::from_os_path(&b, VfOffset::At(0), 4),
        ReadOp::at(FileOperand::from_fd(i32::MAX), 0, 4),
    ];
    let (mut first, mut absent, mut last, mut invalid) = ([99; 4], [99; 4], [99; 4], [99; 4]);
    let outcomes = fs
        .vread_into_impl(
            &reads,
            &mut [&mut first, &mut absent, &mut last, &mut invalid],
        )
        .unwrap();
    assert_eq!(outcomes.len(), 4);
    assert_eq!(outcomes[0].as_ref().unwrap().bytes_read, 4);
    assert_eq!(outcomes[2].as_ref().unwrap().bytes_read, 4);
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        outcomes[3].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(&first, b"abcd");
    assert_eq!(&last, b"efgh");
    assert_eq!(absent, [99; 4]);
    assert_eq!(invalid, [99; 4]);
    assert!(fs.vread_into_impl(&[], &mut []).unwrap().is_empty());
    let whole = fs
        .vread_all_with_options_impl(
            &[
                FileOperand::from_os_path(&a),
                FileOperand::from_os_path(&missing),
                FileOperand::from_os_path(&b),
                FileOperand::from_fd(i32::MAX),
            ],
            ReadAllOptions::new().with_max_total_bytes(8),
        )
        .unwrap();
    assert_eq!(whole.len(), 4);
    assert_eq!(whole[0].as_ref().unwrap(), b"abcd");
    assert_eq!(whole[2].as_ref().unwrap(), b"efgh");
    assert_eq!(whole[1].as_ref().unwrap_err().state(), FailureState::Failed);
    assert_eq!(
        whole[3].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(fs.vread_all_impl(&[]).unwrap().is_empty());

    {
        let remove_files = &[FileOperand::from_os_path(&a), FileOperand::from_os_path(&b)];
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
    {
        let remove_files = &[FileOperand::from_os_path(directory)];
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
}

/// Independent public read outcomes across mixed forms and invalid handles.
pub fn public_read_outcomes<F: vfsi_core::Vfsi>(fs: &F, other: &F, directory: &str) {
    use vfsi_core::FailureState;
    use vfsi_core::api::{FileHandle, ReadOp, ReadOptions, VfsiExt};
    let one = format!("{directory}/one");
    let two = format!("{directory}/two");
    let missing = format!("{directory}/missing");
    fs.write_files(&[(&one, b"abcd".as_slice()), (&two, b"efgh")])
        .unwrap();
    let live = fs.open(&one).unwrap();
    let mut closed = fs.open(&two).unwrap();
    closed.try_close().unwrap();
    let foreign = other.open(&two).unwrap();
    let mut rejected = [99; 2];
    let mut valid = [99; 2];
    let outcomes = fs
        .vread(
            [
                ReadOp::whole(&one),
                ReadOp::into(&closed, 0, &mut rejected),
                ReadOp::range(&live, 1, 2),
                ReadOp::whole(&missing),
                ReadOp::into(&live, 0, &mut valid),
                ReadOp::range(&foreign, 0, 2),
                ReadOp::whole(&two),
            ],
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(outcomes.len(), 7);
    for (index, data) in [(0, b"abcd".as_slice()), (2, b"bc"), (6, b"efgh")] {
        assert_eq!(outcomes[index].as_ref().unwrap().data().unwrap(), data);
    }
    let buffered = outcomes[4].as_ref().unwrap();
    assert!(buffered.is_buffered());
    assert_eq!((buffered.offset(), buffered.bytes_read()), (0, 2));
    assert_eq!(valid, *b"ab");
    assert_eq!(rejected, [99; 2]);
    for index in [1, 5] {
        let error = outcomes[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    let missing_error = outcomes[3].as_ref().unwrap_err();
    assert_eq!(missing_error.state(), FailureState::Failed);
    assert_eq!(missing_error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(missing_error.cause().path(), Some(Path::new(&missing)));
    assert_eq!(missing_error.cause().index(), None);
    let mut rejected = [99; 2];
    let error = fs
        .read_batch(
            [
                ReadOp::whole(&missing),
                ReadOp::into(&foreign, 0, &mut rejected),
            ],
            ReadOptions::new(),
        )
        .unwrap_err();
    assert_eq!(
        error.index(),
        Some(0),
        "select by input order after every phase finishes"
    );
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(rejected, [99; 2]);
    let mut untouched = [99; 2];
    let error = fs
        .vread(
            [
                ReadOp::whole(&one),
                ReadOp::into(&live, 0, &mut untouched),
                ReadOp::range(&live, 0, 3),
            ],
            ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(4).unwrap()),
        )
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    assert_eq!(error.index(), Some(2));
    assert_eq!(untouched, [99; 2]);
    assert!(fs.vread([], ReadOptions::new()).unwrap().is_empty());
    live.close().unwrap();
    foreign.close().unwrap();
}

/// Independent public synchronization outcomes and untouched invalid handles.
pub fn public_sync_outcomes<F: vfsi_core::Vfsi>(fs: &F, other: &F, directory: &str) {
    public_owned_open_outcomes(fs, directory);
    use vfsi_core::{
        FailureState,
        api::{FileHandle, SyncMode, VfsiExt},
    };
    let paths = [
        format!("{directory}/sync-one"),
        format!("{directory}/sync-two"),
    ];
    fs.write_files(&[(&paths[0], b"one".as_slice()), (&paths[1], b"two")])
        .unwrap();
    let files = fs
        .open_options()
        .read(true)
        .write(true)
        .vopen(&paths)
        .unwrap();
    let mut closed = fs.open(&paths[0]).unwrap();
    closed.try_close().unwrap();
    let foreign = other
        .open_options()
        .read(true)
        .write(true)
        .open(&paths[1])
        .unwrap();
    for mode in [SyncMode::Data, SyncMode::All] {
        let outcomes = fs
            .vfsync(&[&files[0], &closed, &files[1], &foreign], mode)
            .unwrap();
        assert_eq!(outcomes.len(), 4);
        assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
        for index in [1, 3] {
            let error = outcomes[index].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        }
        let error = fs.sync_batch(&[&foreign, &files[0]], mode).unwrap_err();
        assert_eq!(error.index(), Some(0));
        assert!(fs.vfsync(&[], mode).unwrap().is_empty());
    }
    fs.close_files(files).unwrap();
    foreign.close().unwrap();
}

/// Confirmed CLOSEs disarm independently; invalid owners remain untouched.
pub fn public_close_outcomes<F: vfsi_core::Vfsi>(fs: &F, other: &F, directory: &str) {
    use vfsi_core::{
        FailureState,
        api::{FileHandle, VfsiExt},
    };
    let path = format!("{directory}/close");
    fs.write(&path, b"close").unwrap();
    let mut closed = fs.open(&path).unwrap();
    closed.try_close().unwrap();
    let mut files = [
        fs.open(&path).unwrap(),
        other.open(&path).unwrap(),
        closed,
        fs.open(&path).unwrap(),
    ];
    let outcomes = fs.vclose(&mut files).unwrap();
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok() && outcomes[3].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::NotAttempted);
    assert_eq!(error.cause().index(), None);
    assert!(error.progress().is_none());
    assert_eq!(
        files
            .iter()
            .map(|file| file.is_closed())
            .collect::<Vec<_>>(),
        [true, false, true, true]
    );
    let error = fs.close_batch(&mut files).unwrap_err();
    assert_eq!(error.index(), Some(1));
    files[1].try_close().unwrap();
    // Closed foreign handles still fail ownership validation.
    let outcomes = fs.vclose(&mut files).unwrap();
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok() && outcomes[3].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(other.vclose(std::slice::from_mut(&mut files[1])).unwrap()[0].is_ok());
    assert!(fs.vclose(&mut []).unwrap().is_empty());
}

fn public_owned_open_outcomes<F: vfsi_core::Vfsi>(fs: &F, directory: &str) {
    use vfsi_core::{
        FailureState,
        api::{FileHandle, OpenFlags, OpenOp, ReadOp, ReadOptions, VfsiExt},
    };
    let paths = [
        format!("{directory}/open-first"),
        format!("{directory}/absent/entry"),
        format!("{directory}/open-last"),
    ];
    fs.write_files(&[(&paths[0], b"first".as_slice()), (&paths[2], b"last")])
        .unwrap();
    let requests: Vec<_> = paths
        .iter()
        .map(|path| OpenOp::new(path, OpenFlags::READ))
        .collect();
    let outcomes = fs.vopen(&requests).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    let error = outcomes[1].as_ref().err().expect("missing input fails");
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().path(), Some(std::path::Path::new(&paths[1])));
    let files: Vec<_> = outcomes.into_iter().filter_map(Result::ok).collect();
    assert_eq!(files[0].path(), std::path::Path::new(&paths[0]));
    assert_eq!(files[1].path(), std::path::Path::new(&paths[2]));
    let reads = fs
        .read_batch(
            files.iter().map(|file| ReadOp::range(file, 0, 5)),
            ReadOptions::new(),
        )
        .unwrap();
    assert_eq!(reads[0].data().unwrap(), b"first");
    assert_eq!(reads[1].data().unwrap(), b"last");
    fs.close_files(files).unwrap();
    let error = fs
        .open_batch(&requests)
        .err()
        .expect("strict open must fail");
    assert_eq!(error.index(), Some(1));
    assert!(fs.vopen(&[]).unwrap().is_empty());
}

/// Direct native WRITE outcomes: path errors, rejected descriptors, actual
/// read-only-handle rejection, and healthy siblings in later executor waves.
/// Backends tracking access rights may reject before dispatch; others learn the
/// failure from the executor/server. Assert the backend's precise expected state.
pub fn write_vector_outcomes<F: VectorBackend>(
    fs: &mut F,
    directory: &Path,
    read_only_state: FailureState,
) {
    fs.mkdir_raw_impl(directory, 0o777).unwrap();
    let a = directory.join("first");
    let b = directory.join("last");
    let missing = directory.join("missing/child");
    let overflow = directory.join("overflow");
    let readonly = directory.join("readonly");
    let results = fs
        .vwrite_impl(&borrow_writes(&[
            WriteOp::from_os_path(&a, VfOffset::At(0), b"first".to_vec()).with_creation(),
            WriteOp::from_os_path(&missing, VfOffset::At(0), b"bad".to_vec()).with_creation(),
            WriteOp::from_os_path(&b, VfOffset::At(0), b"last".to_vec()).with_creation(),
            WriteOp::from_os_path(&overflow, VfOffset::At(u64::MAX), b"bad".to_vec())
                .with_creation()
                .with_truncate(),
        ]))
        .unwrap();
    assert_eq!(results.len(), 4);
    for (index, path, count) in [(0, &a, 5), (2, &b, 4)] {
        let result = results[index].as_ref().unwrap();
        assert_eq!(result.file, FileOperand::from_os_path(path));
        assert_eq!(result.offset, 0);
        assert_eq!(result.bytes_written, count);
    }
    let absent = results[1].as_ref().unwrap_err();
    assert_eq!(absent.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(absent.cause().index(), None);
    assert_eq!(
        results[3].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(
        !fs.exists_impl(&overflow).unwrap(),
        "rejected CREATE must not run"
    );
    let first = fs.open_raw_impl(&a, libc::O_RDWR, 0).unwrap();
    let last = fs.open_raw_impl(&b, libc::O_RDWR, 0).unwrap();
    let read_only = fs
        .open_raw_impl(&readonly, libc::O_CREAT | libc::O_RDONLY, 0o666)
        .unwrap();
    let invalid = FileOperand::from_fd(i32::MAX);
    let rejected = fs
        .vwrite_impl(&[
            WriteOp::new(&first, VfOffset::At(1), b"A".as_slice()),
            WriteOp::new(&invalid, VfOffset::At(0), b"bad".as_slice()),
            WriteOp::new(&last, VfOffset::At(1), b"Z".as_slice()),
        ])
        .unwrap();
    assert_eq!(rejected.len(), 3);
    assert_eq!(rejected[0].as_ref().unwrap().bytes_written, 1);
    assert_eq!(rejected[2].as_ref().unwrap().bytes_written, 1);
    let error = rejected[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::NotAttempted);
    assert_eq!(error.cause().index(), None);
    assert!(error.progress().is_none());

    let completed = fs
        .vwrite_impl(&[
            WriteOp::new(&first, VfOffset::At(0), b"1".as_slice()),
            WriteOp::new(&read_only, VfOffset::At(0), b"bad".as_slice()),
            WriteOp::new(&last, VfOffset::At(0), b"3".as_slice()),
        ])
        .unwrap();
    assert_eq!(completed.len(), 3);
    assert_eq!(completed[0].as_ref().unwrap().bytes_written, 1);
    assert_eq!(completed[2].as_ref().unwrap().bytes_written, 1);
    let error = completed[1].as_ref().unwrap_err();
    assert_eq!(error.state(), read_only_state);
    assert_eq!(error.cause().index(), None);
    assert!(error.progress().is_none());
    assert_eq!(fs.read_raw_impl(&first, 0, 8).unwrap(), b"1Arst");
    assert_eq!(fs.read_raw_impl(&last, 0, 8).unwrap(), b"3Zst");
    assert!(fs.vwrite_impl(&[]).unwrap().is_empty());
    assert!(
        fs.vclose_impl(&[first, last, read_only])
            .unwrap()
            .iter()
            .all(Result::is_ok)
    );
    {
        let remove_files = &[
            FileOperand::from_os_path(&a),
            FileOperand::from_os_path(&b),
            FileOperand::from_os_path(&readonly),
        ];
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
    {
        let remove_files = &[FileOperand::from_os_path(directory)];
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
}

/// Portable WRITE results across local rejection, actual write failures, strict
/// convenience collection and both partial/complete execution policies.
pub fn public_write_outcomes<F: vfsi_core::Vfsi>(
    fs: &F,
    other: &F,
    directory: &str,
    read_only_state: FailureState,
) {
    use vfsi_core::api::{FileHandle, OpenFlags, OpenOp, VfsiExt, WriteOp, WriteOptions};
    let one = format!("{directory}/one");
    let two = format!("{directory}/two");
    let protected = format!("{directory}/readonly");
    fs.write_files(&[
        (&one, b"abcd".as_slice()),
        (&two, b"efgh"),
        (&protected, b"safe"),
    ])
    .unwrap();
    let files = fs
        .open_batch(&[
            OpenOp::new(&one, OpenFlags::READ | OpenFlags::WRITE),
            OpenOp::new(&two, OpenFlags::READ | OpenFlags::WRITE),
        ])
        .unwrap();
    let readonly = fs.open(&protected).unwrap();
    let mut closed = fs.open(&one).unwrap();
    closed.try_close().unwrap();
    let foreign = other.open(&two).unwrap();
    for complete in [false, true] {
        let results = fs
            .vwrite(
                &[
                    WriteOp::at(&files[0], 1, b"AB"),
                    WriteOp::at(&closed, 0, b"closed"),
                    WriteOp::at(&files[1], 2, b"CD"),
                    WriteOp::at(&foreign, 0, b"foreign"),
                    WriteOp::at(&readonly, 0, b"bad"),
                    WriteOp::at(&files[1], u64::MAX, b"overflow"),
                    WriteOp::at(&files[1], 4, b"EF"),
                ],
                WriteOptions::new().with_write_all(complete),
            )
            .unwrap();
        assert_eq!(results.len(), 7);
        for index in [0, 2, 6] {
            assert_eq!(results[index].as_ref().unwrap().bytes_written, 2);
        }
        for index in [1, 3, 5] {
            let error = results[index].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        }
        let error = results[4].as_ref().unwrap_err();
        assert_eq!(error.cause().path(), Some(Path::new(&protected)));
        assert_eq!(error.cause().operation(), Some("vwrite_native"));
        assert_eq!(error.state(), read_only_state);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
        assert!(
            fs.vwrite(&[], WriteOptions::new().with_write_all(complete))
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        fs.read_files(&[&one, &two, &protected]).unwrap(),
        [b"aABd".to_vec(), b"efCDEF".to_vec(), b"safe".to_vec()]
    );
    let error = fs
        .write_batch(
            &[
                WriteOp::at(&files[0], 10, b"x"),
                WriteOp::at(&readonly, 0, b"!"),
                WriteOp::at(&files[1], 10, b"z"),
            ],
            WriteOptions::new().with_write_all(true),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let data = fs.read_files(&[&one, &two]).unwrap();
    assert_eq!(
        data[0][10], b'x',
        "strict collection does not undo healthy inputs"
    );
    assert_eq!(
        data[1][10], b'z',
        "strict collection does not suppress a later healthy input"
    );
    assert_eq!(fs.read(&protected).unwrap(), b"safe");
    fs.close_files(files).unwrap();
    readonly.close().unwrap();
    foreign.close().unwrap();
}

fn check_entry_removal_results(fs: &mut impl VectorBackend, directory: &str, mode: u32) {
    assert!(fs.vremove_impl(&[]).unwrap().is_empty());
    let first = PathBuf::from(format!("{directory}/remove-first"));
    let last = PathBuf::from(format!("{directory}/remove-last"));
    let missing = PathBuf::from(format!("{directory}/remove-missing"));
    let empty = PathBuf::from(format!("{directory}/remove-empty-dir"));
    for path in [&first, &last] {
        let results = fs.vwrite_impl(&borrow_writes(&[WriteOp::from_os_path(
            path,
            VfOffset::At(0),
            b"safe".to_vec(),
        )
        .with_creation()]));
        crate::internal::collect_vec_result(results, 1).unwrap();
    }
    fs.ensure_dir_impl(&empty, mode).unwrap();
    let files = [
        FileOperand::from_os_path(&first),
        FileOperand::from_os_path(&missing),
        FileOperand::from_os_path(&last),
        FileOperand::from_fd(-1),
        FileOperand::from_os_path(&empty),
    ];
    let results = fs.vremove_impl(&files).unwrap();
    assert_eq!(results.len(), files.len());
    for index in [0, 2, 4] {
        assert!(results[index].is_ok(), "{index}: {:?}", results[index]);
    }
    let error = results[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(error.cause().index(), None);
    assert!(error.progress().is_none());
    assert_eq!(
        results[3].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    for path in [&first, &last, &empty] {
        assert!(!fs.exists_impl(path).unwrap());
    }
    let error = crate::internal::collect_vec_result(Ok(results), files.len()).unwrap_err();
    assert_eq!(error.index(), Some(1));
}
