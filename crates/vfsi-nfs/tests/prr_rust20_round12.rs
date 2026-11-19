//! Server COPY preserves dependencies between requests without disabling batching.
#![cfg(feature = "server-copy")]

use std::path::Path;
use vfsi_nfs::NfsVecFs;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{CopyOptions, ExtentPair, FileOperand};

fn connect(name: &str) -> Option<(NfsVecFs, String)> {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    let mut fs = NfsVecFs::connect_minor(&server, 2).expect("connect NFSv4.2 fixture");
    let root = format!("/prr-copy-dependency-{}-{name}", std::process::id());
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    Some((fs, root))
}
fn put(fs: &mut NfsVecFs, path: &str, data: &[u8]) {
    let file = fs
        .open_raw_impl(
            Path::new(path),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
        .unwrap();
    assert_eq!(fs.write_raw_impl(&file, 0, data).unwrap(), data.len());
    fs.close_impl(&file).unwrap();
}
fn get(fs: &mut NfsVecFs, path: &str) -> Vec<u8> {
    fs.read_raw_impl(&FileOperand::from_path(path), 0, 100)
        .unwrap()
}
fn cleanup(fs: &mut NfsVecFs, root: &str) {
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
}
fn chained(hardlink: bool, missing_intermediate: bool) {
    let Some((mut fs, root)) = connect(if hardlink { "hardlink" } else { "chain" }) else {
        return;
    };
    let (a, b, c, alias) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
        format!("{root}/alias"),
    );
    put(&mut fs, &a, b"fresh");
    if !missing_intermediate {
        put(&mut fs, &b, b"stale-tail");
    }
    put(&mut fs, &c, b"old");
    if hardlink {
        fs.vhardlink_impl(&[Path::new(&b)], &[Path::new(&alias)])
            .map_err(vfsi_core::IndexedError::unattributed)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
            })
            .unwrap();
    }
    let d = format!("{root}/d");
    let source = if hardlink { &alias } else { &b };
    let result = {
        let mut copy_pairs = vec![
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(source, 0, &c, 0, None),
        ];
        if missing_intermediate {
            // C already exists, so its initial OPEN succeeds. Its copy
            // still has to observe the write from the re-admitted pair.
            copy_pairs.push(ExtentPair::new(&c, 0, &d, 0, None));
        }
        vfsi_core::internal::collect_vec_result(
            fs.vcopy_impl(&copy_pairs, CopyOptions::default()),
            copy_pairs.len(),
        )
        .map(|_| ())
    };
    let actual_b = get(&mut fs, &b);
    let actual_c = get(&mut fs, &c);
    let actual_d = missing_intermediate.then(|| get(&mut fs, &d));
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.expect("chained COPY requests must execute in caller order");
    assert_eq!(actual_b, b"fresh");
    assert_eq!(actual_c, b"fresh");
    if let Some(actual_d) = actual_d {
        assert_eq!(actual_d, b"fresh");
    }
    assert!(
        enabled,
        "a dependency must not disable server COPY for future calls"
    );
}
#[test]
fn chained_copy_reads_the_previous_destination() {
    chained(false, false);
    chained(false, true);
}
#[test]
fn chained_copy_detects_hardlink_alias_dependencies() {
    chained(true, false);
}
#[cfg(feature = "test-faults")]
#[test]
fn failed_copy_does_not_publish_a_previously_missing_source() {
    use std::sync::Arc;
    use vfsi_core::{
        FailureState, TransportKind,
        internal::{
            errors::transport,
            faults::{FaultScript, OpenFaultPoint},
        },
    };

    for (point, cause, expected) in [
        (
            OpenFaultPoint::AfterCopyCompoundReply { first: 0 },
            transport(TransportKind::Other, "lost predecessor COPY reply"),
            FailureState::Unknown,
        ),
        (
            OpenFaultPoint::BeforeCopyCompound { first: 0 },
            std::io::Error::from_raw_os_error(libc::EIO),
            FailureState::Failed,
        ),
    ] {
        let Some((mut fs, root)) = connect("failed-chain") else {
            return;
        };
        let source = format!("{root}/a");
        let intermediate = format!("{root}/b");
        let destination = format!("{root}/c");
        let independent = format!("{root}/d");
        put(&mut fs, &source, b"first");
        put(&mut fs, &destination, b"keep");
        let fault = Arc::new(FaultScript::one(point, cause));
        fs.set_fault_injector(fault.clone());
        let results = fs
            .vcopy_impl(
                &[
                    ExtentPair::new(&source, 0, &intermediate, 0, None),
                    ExtentPair::new(&intermediate, 0, &destination, 0, None),
                    ExtentPair::new(&source, 0, &independent, 0, None),
                ],
                CopyOptions::default(),
            )
            .unwrap();
        let intermediate_exists = fs.exists_impl(Path::new(&intermediate)).unwrap();
        let preserved = get(&mut fs, &destination);
        let independent_data =
            (expected == FailureState::Failed).then(|| get(&mut fs, &independent));
        cleanup(&mut fs, &root);

        assert!(fault.is_consumed());
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap_err().state(), expected);
        assert_eq!(
            results[1].as_ref().unwrap_err().state(),
            FailureState::NotAttempted
        );
        assert!(intermediate_exists);
        assert_eq!(preserved, b"keep");
        if let Some(data) = independent_data {
            assert!(results[2].is_ok());
            assert_eq!(data, b"first");
        } else {
            assert_eq!(
                results[2].as_ref().unwrap_err().state(),
                FailureState::NotAttempted
            );
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn chained_copy_does_not_propagate_a_failed_destination() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultInjector, OpenFaultPoint},
    };

    struct FailSecond(OpenFaultPoint, AtomicUsize);
    impl FaultInjector for FailSecond {
        fn check(&self, point: &OpenFaultPoint) -> std::io::Result<()> {
            if *point == self.0 && self.1.fetch_add(1, Ordering::Relaxed) == 1 {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(())
        }
    }

    for (point, fail_open) in [
        (OpenFaultPoint::BeforeRegister { index: 0 }, true),
        (OpenFaultPoint::BeforeSetattrCompound { first: 0 }, false),
    ] {
        let Some((mut fs, root)) = connect("failed-destination") else {
            return;
        };
        let a = format!("{root}/a");
        let b = format!("{root}/b");
        let c = format!("{root}/c");
        let d = format!("{root}/d");
        let e = format!("{root}/e");
        put(&mut fs, &a, b"new");
        put(&mut fs, &c, b"old tail");
        put(&mut fs, &d, b"keep");
        let fault = Arc::new(FailSecond(point, AtomicUsize::new(0)));
        fs.set_fault_injector(fault.clone());
        let pairs = [
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(&b, 0, &c, 0, None),
            ExtentPair::new(&c, 0, &d, 0, None),
            ExtentPair::new(&a, 0, &e, 0, None),
        ];
        let outcomes = fs.vcopy_impl(&pairs, CopyOptions::default()).unwrap();
        let contents = [&b, &c, &d, &e].map(|path| get(&mut fs, path));
        cleanup(&mut fs, &root);

        assert!(fault.1.load(Ordering::Relaxed) >= 2);
        let failed = if fail_open { 0 } else { 1 };
        assert_eq!(
            outcomes[failed].as_ref().unwrap_err().state(),
            FailureState::Failed
        );
        assert_eq!(
            outcomes[failed + 1].as_ref().unwrap_err().state(),
            FailureState::NotAttempted
        );
        assert!(outcomes[3].is_ok());
        let [b, c, d, e] = contents;
        if fail_open {
            assert!(outcomes[2].is_ok());
            assert_eq!(
                (b.as_slice(), c.as_slice(), d.as_slice()),
                (&b""[..], &b"old tail"[..], &b"old tail"[..])
            );
        } else {
            assert!(outcomes[0].is_ok());
            assert_eq!(
                (b.as_slice(), c.as_slice(), d.as_slice()),
                (&b"new"[..], &b"new tail"[..], &b"keep"[..])
            );
        }
        assert_eq!(e, b"new");
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn unrelated_failure_does_not_block_a_successful_copy_chain() {
    use std::sync::Arc;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};

    let Some((mut fs, root)) = connect("independent-failure") else {
        return;
    };
    let a = format!("{root}/a");
    let b = format!("{root}/b");
    let y = format!("{root}/y");
    let z = format!("{root}/z");
    put(&mut fs, &a, b"new");
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeSetattrCompound { first: 0 },
        std::io::Error::from_raw_os_error(libc::EIO),
    ));
    fs.set_fault_injector(fault.clone());
    let pairs = [
        ExtentPair::new(&a, 0, &b, 0, None),
        ExtentPair::new(&a, 0, &y, 0, None),
        ExtentPair::new(&y, 0, &z, 0, None),
    ];
    let outcomes = fs.vcopy_impl(&pairs, CopyOptions::default()).unwrap();
    let data = fs.read_raw_impl(&FileOperand::from_path(&z), 0, 100);
    cleanup(&mut fs, &root);

    assert!(fault.is_consumed());
    assert_eq!(
        vfsi_core::native_status(outcomes[0].as_ref().unwrap_err().cause()),
        Some(vfsi_core::StatusCode::Errno(libc::EIO as u32))
    );
    assert!(outcomes[1].is_ok(), "{:?}", outcomes[1]);
    assert!(outcomes[2].is_ok(), "{:?}", outcomes[2]);
    assert_eq!(data.unwrap(), b"new");
}

#[test]
fn unrelated_symlink_loop_does_not_block_a_successful_copy_chain() {
    let Some((mut fs, root)) = connect("unrelated-symlink-loop") else {
        return;
    };
    let source = format!("{root}/source");
    let link = format!("{root}/loop");
    let intermediate = format!("{root}/intermediate");
    let destination = format!("{root}/destination");
    put(&mut fs, &source, b"input");
    fs.vsymlink_impl(&[Path::new("loop")], &[Path::new(&link)])
        .unwrap()
        .remove(0)
        .unwrap();
    let pairs = [
        ExtentPair::new(&source, 0, &link, 0, None),
        ExtentPair::new(&source, 0, &intermediate, 0, None),
        ExtentPair::new(&intermediate, 0, &destination, 0, None),
    ];
    let outcomes = fs.vcopy_impl(&pairs, CopyOptions::default()).unwrap();
    let data = fs.read_raw_impl(&FileOperand::from_path(&destination), 0, 100);
    cleanup(&mut fs, &root);

    assert_eq!(
        vfsi_core::native_status(outcomes[0].as_ref().unwrap_err().cause()),
        Some(vfsi_core::StatusCode::Errno(libc::ELOOP as u32))
    );
    assert!(outcomes[1].is_ok(), "{:?}", outcomes[1]);
    assert!(outcomes[2].is_ok(), "{:?}", outcomes[2]);
    assert_eq!(data.unwrap(), b"input");
}

#[test]
fn failed_destination_open_does_not_replace_an_unrelated_missing_source_error() {
    let Some((mut fs, root)) = connect("missing-source-after-open-failure") else {
        return;
    };
    let source = format!("{root}/source");
    let missing = format!("{root}/missing");
    let destination = format!("{root}/destination");
    put(&mut fs, &source, b"input");
    let outcomes = fs
        .vcopy_impl(
            &[
                ExtentPair::new(&source, 0, &root, 0, None),
                ExtentPair::new(&missing, 0, &destination, 0, None),
            ],
            CopyOptions::default(),
        )
        .unwrap();
    cleanup(&mut fs, &root);

    assert!(outcomes[0].is_err());
    let missing = outcomes[1].as_ref().unwrap_err();
    assert_eq!(missing.state(), vfsi_core::FailureState::Failed);
    assert_eq!(missing.cause().kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn later_destination_does_not_invalidate_an_earlier_source() {
    let Some((mut fs, root)) = connect("anti") else {
        return;
    };
    let (a, b, c) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
    );
    put(&mut fs, &a, b"first");
    put(&mut fs, &b, b"stale");
    put(&mut fs, &c, b"second");
    let result = {
        let copy_pairs = &[
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(&c, 0, &a, 0, None),
        ];
        vfsi_core::internal::collect_vec_result(
            fs.vcopy_impl(copy_pairs, CopyOptions::default()),
            copy_pairs.len(),
        )
        .map(|_| ())
    };
    let actual_b = get(&mut fs, &b);
    let actual_a = get(&mut fs, &a);
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual_b, b"first");
    assert_eq!(actual_a, b"second");
    assert!(enabled);
}
#[test]
fn repeated_destination_preserves_final_size_without_disabling_server_copy() {
    let Some((mut fs, root)) = connect("repeat") else {
        return;
    };
    let (a, b, c) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
    );
    put(&mut fs, &a, b"long-first");
    put(&mut fs, &b, b"x");
    put(&mut fs, &c, b"stale-tail");
    let result = {
        let copy_pairs = &[
            ExtentPair::new(&a, 0, &c, 0, None),
            ExtentPair::new(&b, 0, &c, 0, None),
        ];
        vfsi_core::internal::collect_vec_result(
            fs.vcopy_impl(copy_pairs, CopyOptions::default()),
            copy_pairs.len(),
        )
        .map(|_| ())
    };
    let actual = get(&mut fs, &c);
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual, b"x");
    assert!(enabled);
}
#[test]
fn independent_pairs_keep_one_server_copy_cohort() {
    let Some((mut fs, root)) = connect("independent") else {
        return;
    };
    let (a, b, c, d) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
        format!("{root}/d"),
    );
    put(&mut fs, &a, b"first");
    put(&mut fs, &b, b"second");
    let before = fs.server_copy_stats();
    let result = {
        let copy_pairs = &[
            ExtentPair::new(&a, 0, &c, 0, None),
            ExtentPair::new(&b, 0, &d, 0, None),
        ];
        vfsi_core::internal::collect_vec_result(
            fs.vcopy_impl(copy_pairs, CopyOptions::default()),
            copy_pairs.len(),
        )
        .map(|_| ())
    };
    let after = fs.server_copy_stats();
    let actual_c = get(&mut fs, &c);
    let actual_d = get(&mut fs, &d);
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual_c, b"first");
    assert_eq!(actual_d, b"second");
    assert_eq!(after.fallbacks, before.fallbacks);
    assert_eq!(after.requests - before.requests, 2);
    assert_eq!(after.operations - before.operations, 4);
}

fn fanout(hardlink: bool) {
    let Some((mut fs, root)) = connect(if hardlink { "fanout-alias" } else { "fanout" }) else {
        return;
    };
    let (a, b, c, alias) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
        format!("{root}/alias"),
    );
    put(&mut fs, &a, b"fresh");
    if hardlink {
        fs.vhardlink_impl(&[Path::new(&a)], &[Path::new(&alias)])
            .map_err(vfsi_core::IndexedError::unattributed)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
            })
            .unwrap();
    }
    let source = if hardlink { &alias } else { &a };
    let result = {
        let copy_pairs = &[
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(source, 0, &c, 0, None),
        ];
        vfsi_core::internal::collect_vec_result(
            fs.vcopy_impl(copy_pairs, CopyOptions::default()),
            copy_pairs.len(),
        )
        .map(|_| ())
    };
    let actual_b = get(&mut fs, &b);
    let actual_c = get(&mut fs, &c);
    let enabled = fs.server_copy_enabled();
    cleanup(&mut fs, &root);
    result.unwrap();
    assert_eq!(actual_b, b"fresh");
    assert_eq!(actual_c, b"fresh");
    assert!(enabled);
}
#[test]
fn repeated_source_fanout_preserves_server_copy_capability() {
    fanout(false)
}
#[test]
fn repeated_source_hardlink_fanout_preserves_server_copy_capability() {
    fanout(true)
}

#[test]
fn failed_dependent_suffix_keeps_original_index_and_does_not_replay_prefix() {
    let Some((mut fs, root)) = connect("error") else {
        return;
    };
    let (a, b, c) = (
        format!("{root}/a"),
        format!("{root}/b"),
        format!("{root}/c"),
    );
    put(&mut fs, &a, b"fresh");
    put(&mut fs, &b, b"stale");
    put(&mut fs, &c, b"old");
    let result = {
        let copy_pairs = &[
            ExtentPair::new(&a, 0, &b, 0, None),
            ExtentPair::new(&b, 0, &c, 0, None),
            ExtentPair::new(&c, 0, &c, 0, None),
        ];
        vfsi_core::internal::collect_vec_result(
            fs.vcopy_impl(copy_pairs, CopyOptions::default()),
            copy_pairs.len(),
        )
        .map(|_| ())
    };
    let actual_b = get(&mut fs, &b);
    let actual_c = get(&mut fs, &c);
    let enabled = fs.server_copy_enabled();
    #[cfg(feature = "test-faults")]
    assert_eq!(fs.test_open_handle_count(), 0);
    cleanup(&mut fs, &root);
    let error = result.expect_err("fixture rejects same-file server COPY");
    assert_eq!(error.index(), Some(2));
    assert_eq!(actual_b, b"fresh");
    assert_eq!(actual_c, b"fresh");
    assert!(enabled);
}
