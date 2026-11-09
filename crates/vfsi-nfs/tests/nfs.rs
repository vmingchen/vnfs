//! Public-package integration coverage for the NFS backend.

use std::path::{Path, PathBuf};
#[cfg(feature = "test-faults")]
use std::sync::Arc;
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::test_support::borrow_writes;
use vfsi_sync::*;

use vfsi_core::RpcError;
#[cfg(feature = "test-faults")]
use vfsi_core::VfError;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_nfs::NfsVecFs;
#[cfg(feature = "test-faults")]
use vfsi_nfs::client::{FileRef, PathOpenOp, PathWriteOp, WriteOp};
use vfsi_nfs::client::{NfsClient, OpenCreate, ReadOp};
use vfsi_sync::test_support;

fn required() -> bool {
    std::env::var("VFSI_NFS_REQUIRED").as_deref() == Ok("1")
}

fn scalar_descriptor_lifetime(exclusive: bool) {
    let Some(mut fs) = connect() else { return };
    let root = format!("/vfsi-scalar-owner-{}-{exclusive}", std::process::id());
    let path = std::path::PathBuf::from(format!("{root}/file"));
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    if !exclusive {
        fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_os_path(
            &path,
            VfOffset::At(0),
            b"abc".to_vec(),
        )
        .with_creation()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
    }
    let flags = libc::O_RDWR
        | libc::O_APPEND
        | if exclusive {
            libc::O_CREAT | libc::O_EXCL
        } else {
            0
        };
    let first = fs.open_raw_impl(&path, flags, 0o640).unwrap();
    if exclusive {
        fs.write_raw_impl(&first, 0, b"abc").unwrap();
        assert_eq!(fs.fstat_impl(&first).unwrap().mode & 0o777, 0o640);
    }
    let second = fs.open_raw_impl(&path, libc::O_RDWR, 0).unwrap();
    let by_path = FileOperand::from_os_path(&path);
    assert_eq!(fs.read_raw_impl(&by_path, 0, 4).unwrap(), b"abc");
    // The temporary pathname OPEN/CLOSE must not invalidate either descriptor.
    let appended = fs
        .vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::new(
            first.clone(),
            VfOffset::Cur,
            b"d".to_vec(),
        )]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(appended[0].offset, 3);
    assert_eq!(appended[0].bytes_written, 1);
    assert_eq!(fs.read_raw_impl(&second, 0, 4).unwrap(), b"abcd");
    // Closing one caller-owned OPEN must not close its independent sibling.
    fs.close_impl(&first).unwrap();
    fs.write_raw_impl(&second, 0, b"AB").unwrap();
    assert_eq!(fs.read_raw_impl(&by_path, 0, 4).unwrap(), b"ABcd");
    assert_eq!(fs.read_raw_impl(&second, 0, 4).unwrap(), b"ABcd");
    fs.close_impl(&second).unwrap();
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
}

#[test]
fn scalar_opens_keep_independent_state_during_path_io() {
    scalar_descriptor_lifetime(false);
}

#[test]
fn exclusive_scalar_opens_keep_independent_state_during_path_io() {
    scalar_descriptor_lifetime(true);
}

#[cfg(feature = "test-faults")]
#[test]
fn exclusive_open_cleanup_preserves_times_across_mixed_compound_chunks() {
    use std::time::{SystemTime, UNIX_EPOCH};
    use vfsi_nfs::client::{CloseOp, OpenOp};

    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse::<u32>()
        .unwrap();
    let mut client = NfsClient::connect_minor(&server, minor).unwrap();
    let root = client.root().clone();
    client.test_limit_compound_operations(8); // SEQUENCE + two three-op OPENs.
    for mixed in [false, true] {
        let name = format!("vfsi-exclusive-times-{}-{mixed}", std::process::id());
        let dir = client.mkdir(&root, &name).unwrap();
        let names: Vec<Vec<u8>> = (0..if mixed { 4 } else { 1 })
            .map(|index| format!("file-{index}").into_bytes())
            .collect();
        let mut existing_times = Vec::new();
        if mixed {
            for name in &names[..2] {
                let (fh, stateid) = client
                    .open(
                        &dir,
                        name,
                        nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                        OpenCreate::Unchecked,
                    )
                    .unwrap();
                // A known old timestamp also proves cleanup does not modify
                // earlier nonexclusive requests when interpreting later replies.
                client
                    .setattr_values(&fh, None, None, Some((123, 456)), Some((123, 456)))
                    .unwrap();
                existing_times.push(
                    client
                        .getattr(
                            &fh,
                            &[
                                nfsv41_sys::FATTR4_TIME_ACCESS,
                                nfsv41_sys::FATTR4_TIME_MODIFY,
                            ],
                        )
                        .unwrap(),
                );
                client.close(&fh, &stateid).unwrap();
            }
        }
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let ops: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, name)| OpenOp {
                dir: dir.clone(),
                name: name.clone(),
                access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                create: if mixed && index < 2 {
                    OpenCreate::NoCreate
                } else {
                    OpenCreate::Exclusive
                },
            })
            .collect();
        let opened = client.open_many(&ops).unwrap();
        let observed: Vec<_> = opened
            .iter()
            .map(|(fh, _)| {
                client
                    .getattr(
                        fh,
                        &[
                            nfsv41_sys::FATTR4_TIME_ACCESS,
                            nfsv41_sys::FATTR4_TIME_MODIFY,
                        ],
                    )
                    .unwrap()
            })
            .collect();
        let closes: Vec<_> = opened
            .into_iter()
            .map(|(fh, stateid)| CloseOp { fh, stateid })
            .collect();
        client.close_many(&closes).unwrap();
        {
            let remove_files = &names;
            vfsi_core::internal::collect_vec_result(
                client.remove_many(&dir, remove_files),
                remove_files.len(),
            )
            .map(|_| ())
        }
        .unwrap();
        client.remove(&root, &name).unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (index, attrs) in observed.iter().enumerate() {
            if mixed && index < 2 {
                assert_eq!(attrs, &existing_times[index]);
            } else {
                assert_eq!(attrs.len(), 24);
                for offset in [0, 12] {
                    let seconds = i64::from_be_bytes(attrs[offset..offset + 8].try_into().unwrap());
                    assert!(
                        (before..=after).contains(&seconds),
                        "verifier timestamp was not restored: mixed={mixed}, file={index}, seconds={seconds}, expected {before}..={after}"
                    );
                }
            }
        }
    }
}

#[test]
fn session_connect_uses_the_highest_supported_minor_and_initializes_limits() {
    let host = match std::env::var("VFSI_NFS_SERVER") {
        Ok(host) => host,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    use vfsi_nfs::session::Session;
    let expected = match Session::connect_minor(&host, 2) {
        Ok(_) => 2,
        Err(error) => {
            assert_eq!(
                error.status,
                nfsv41_sys::nfsstat4_NFS4ERR_MINOR_VERS_MISMATCH
            );
            1
        }
    };
    let session = Session::connect(&host).expect("automatic session negotiation");
    assert_eq!(session.minorversion, expected);
    assert!(session.max_operations > 0);
    assert!(session.max_requestsize > 1024);
    assert!(session.max_responsesize > 1024);
    assert_ne!(session.sessionid, [0; 16]);
}

#[cfg(feature = "test-faults")]
#[test]
fn recursive_remove_does_not_restart_each_page_after_a_persistent_entry_failure() {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use vfsi_core::internal::faults::FaultInjector;
    struct PersistentFailure {
        attempts: AtomicUsize,
        cookies: Mutex<Vec<u64>>,
    }
    impl FaultInjector for PersistentFailure {
        fn check(&self, point: &OpenFaultPoint) -> vfsi_core::VfResult<()> {
            match point {
                OpenFaultPoint::BeforeRemovePage { cookie } => {
                    self.cookies.lock().unwrap().push(*cookie)
                }
                OpenFaultPoint::BeforeRemoveChunk { first_name } if first_name == b"blocked" => {
                    self.attempts.fetch_add(1, Ordering::Relaxed);
                    return Err(VfError::nfs(0, nfsv41_sys::nfsstat4_NFS4ERR_ACCESS));
                }
                _ => {}
            }
            Ok(())
        }
    }
    let Some(mut fs) = connect() else { return };
    let root = format!("/vfsi-remove-pages-{}", std::process::id());
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    // Enough entries for multiple 32-KiB READDIR pages. One-item REMOVE
    // batches fail only `blocked`, without fabricating a successful prefix.
    let paths: Vec<_> = std::iter::once(format!("{root}/blocked"))
        .chain((0..1500).map(|index| format!("{root}/file-{index:04}")))
        .collect();
    fs.vwrite_impl(&borrow_writes(
        &paths
            .iter()
            .map(|path| {
                vfsi_sync::WriteOp::at(vfsi_sync::FileOperand::from_path(path), 0, b"x".to_vec())
                    .with_creation()
            })
            .collect::<Vec<_>>(),
    ))
    .and_then(|outcomes| {
        let count = outcomes.len();
        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
    })
    .unwrap();
    let failure = Arc::new(PersistentFailure {
        attempts: AtomicUsize::new(0),
        cookies: Mutex::new(Vec::new()),
    });
    fs.set_fault_injector(failure.clone());
    let error = {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_with_options_impl(
                remove_roots,
                vfsi_core::api::RemoveMode::Tree,
                vfsi_sync::RemoveOptions::new()
                    .with_batch_size(std::num::NonZeroUsize::new(1))
                    .with_continue_on_error(true),
            ),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap_err();
    let attempts = failure.attempts.load(Ordering::Relaxed);
    let cookies = failure.cookies.lock().unwrap().clone();
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    // Clean up before assertions, even when the operation's behavior regresses.
    let remaining: Vec<_> = fs
        .listdir_impl(Path::new(&root), vfsi_sync::AttrMask::empty(), 2, false)
        .unwrap()
        .into_iter()
        .map(|attrs| attrs.file.path().unwrap().to_path_buf())
        .collect();
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    assert_eq!(
        error.status(),
        Some(vfsi_core::StatusCode::Nfs(
            nfsv41_sys::nfsstat4_NFS4ERR_ACCESS
        ))
    );
    assert_eq!(error.index(), Some(0));
    assert!(
        cookies.iter().any(|cookie| *cookie != 0),
        "fixture must exercise continuation pages: {cookies:?}"
    );
    assert_eq!(
        attempts, 2,
        "failed entry should be tried once per complete pass, not once per page: {cookies:?}"
    );
    assert_eq!(
        remaining,
        [std::path::PathBuf::from(format!("{root}/blocked"))]
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn recursive_remove_retries_a_transient_entry_only_when_configured() {
    let Some(mut fs) = connect() else { return };
    for (retries, failures) in [(0u32, 1usize), (0, 2), (1, 1), (1, 2)] {
        let root = format!(
            "/vfsi-remove-retry-{}-{retries}-{failures}",
            std::process::id()
        );
        let path = format!("{root}/transient");
        fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
        fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::at(
            vfsi_sync::FileOperand::from_path(&path),
            0,
            b"x".to_vec(),
        )
        .with_creation()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let point = OpenFaultPoint::BeforeRemoveChunk {
            first_name: b"transient".to_vec(),
        };
        let script = Arc::new(FaultScript::new((0..failures).map(|_| {
            (
                point.clone(),
                VfError::nfs(0, nfsv41_sys::nfsstat4_NFS4ERR_DELAY),
            )
        })));
        fs.set_fault_injector(script.clone());
        let result = {
            let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
            vfsi_core::internal::collect_vec_result(
                fs.remove_paths_with_options_impl(
                    remove_roots,
                    vfsi_core::api::RemoveMode::Tree,
                    vfsi_sync::RemoveOptions::new()
                        .with_batch_size(std::num::NonZeroUsize::new(1))
                        .with_retries(retries),
                ),
                remove_roots.len(),
            )
            .map(|_| ())
        };
        let exists = fs.exists_impl(Path::new(&path)).unwrap();
        fs.set_fault_injector(Arc::new(FaultScript::new([])));
        if exists {
            {
                let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
                vfsi_core::internal::collect_vec_result(
                    fs.remove_paths_impl(remove_roots, true),
                    remove_roots.len(),
                )
                .map(|_| ())
            }
            .unwrap();
        }
        let attempts = retries as usize + 1;
        assert_eq!(script.remaining().len(), failures.saturating_sub(attempts));
        assert_eq!(
            script
                .visited()
                .iter()
                .filter(|visited| **visited == point)
                .count(),
            attempts,
        );
        if failures > retries as usize {
            assert!(exists);
            assert_eq!(
                result.unwrap_err().status(),
                Some(vfsi_core::StatusCode::Nfs(
                    nfsv41_sys::nfsstat4_NFS4ERR_DELAY
                ))
            );
        } else {
            result.unwrap();
            assert!(!exists);
        }
    }
}

#[cfg(feature = "test-faults")]
fn assert_same_file_scatter_is_batched(write_all: bool, short: bool) {
    let Some(mut backend) = connect() else { return };
    backend.set_max_compound_bytes(64 * 1024);
    if short {
        backend.test_short_write_once(7);
    }
    let fs = vfsi_sync::FsClient::new(backend);
    let path = format!("/vfsi-scatter-{}-{write_all}-{short}", std::process::id());
    let file = fs.create(&path).unwrap();
    // Nonmonotonic offsets exercise overlap checks in both directions, with
    // holes between ranges to distinguish positional writes from appends.
    let indices = [7, 0, 15, 3, 11, 1, 9, 4, 14, 2, 10, 5, 13, 6, 12, 8];
    let pieces: Vec<_> = indices
        .iter()
        .map(|&index| vec![index as u8 + 1; 32])
        .collect();
    let requests: Vec<_> = indices
        .iter()
        .zip(&pieces)
        .map(|(&index, data)| file.write_request_at(index * 64, data))
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let results = if write_all {
        fs.vwrite_all_native(&requests)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap()
    } else {
        fs.vwrite_native(&requests)
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap()
    };
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    // Probe the backend descriptor after sampling the scatter RPC count.
    // An independent std_io cursor cannot detect accidental cursor advances.
    let cursor = fs
        .vwrite_native(&[FsWrite::new(&file, VfOffset::Cur, &[])])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap()[0]
        .offset;
    file.close().unwrap();
    let observed = fs.read(&path).unwrap();
    fs.remove_file(&path).unwrap();
    let mut expected = vec![0; 15 * 64 + 32];
    for (&index, data) in indices.iter().zip(&pieces) {
        let start = index as usize * 64;
        expected[start..start + data.len()].copy_from_slice(data);
    }
    assert_eq!(
        compounds,
        if short { 2 } else { 1 },
        "write_all={write_all}, short={short}"
    );
    assert_eq!(cursor, 0, "positional scatter writes changed the cursor");
    assert!(results.iter().all(|result| result.bytes_written == 32));
    assert_eq!(observed, expected);
}

#[cfg(feature = "test-faults")]
#[test]
fn same_file_scatter_writev_uses_one_compound() {
    assert_same_file_scatter_is_batched(false, false);
}

#[cfg(feature = "test-faults")]
#[test]
fn same_file_scatter_write_allv_uses_one_compound() {
    assert_same_file_scatter_is_batched(true, false);
}

#[cfg(feature = "test-faults")]
#[test]
fn same_file_scatter_short_write_repairs_only_the_missing_suffix() {
    for write_all in [false, true] {
        assert_same_file_scatter_is_batched(write_all, true);
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn same_file_dependent_writes_remain_serialized() {
    use vfsi_sync::VfOffset::{At, Cur, End};
    for kind in 0..4 {
        for short in [false, true] {
            let Some(mut fs) = connect() else { return };
            let path = format!("/vfsi-dependent-{}-{kind}-{short}", std::process::id());
            let append = kind == 3;
            let flags = libc::O_CREAT
                | libc::O_RDWR
                | libc::O_TRUNC
                | if append { libc::O_APPEND } else { 0 };
            let files = fs
                .vopen_raw_simple_impl(&[Path::new(&path)], flags, 0o600)
                .unwrap();
            let (first, second) = match kind {
                0 => (At(0), At(2)),
                1 => (Cur, Cur),
                2 => (End, End),
                _ => (At(0), At(4096)),
            };
            let script = Arc::new(FaultScript::new([]));
            fs.set_fault_injector(script.clone());
            if short {
                fs.test_short_write_once(2);
            }
            let _ = vfsi_nfs::compound::thread_compound_stats();
            let results = fs
                .vwrite_impl(&borrow_writes(&[
                    vfsi_sync::WriteOp::new(files[0].clone(), first, b"abcd".to_vec()),
                    vfsi_sync::WriteOp::new(files[0].clone(), second, b"XY".to_vec()),
                ]))
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap();
            let compounds = vfsi_nfs::compound::thread_compound_stats().0;
            let waves = script
                .visited()
                .iter()
                .filter(|point| matches!(point, OpenFaultPoint::AfterWriteChunk { .. }))
                .count();
            let cursor = fs
                .seek_raw_impl(&files[0], 0, vfsi_sync::SeekFrom::Cur)
                .unwrap();
            let contents = fs
                .vread_impl(&[vfsi_sync::ReadOp::at(files[0].clone(), 0, 16)])
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap();
            assert!(
                fs.vclose_impl(&files)
                    .unwrap()
                    .into_iter()
                    .all(|result| result.is_ok())
            );
            {
                let remove_files = &[vfsi_sync::FileOperand::from_path(&path)];
                vfsi_core::internal::collect_vec_result(
                    fs.vremove_impl(remove_files),
                    remove_files.len(),
                )
                .map(|_| ())
            }
            .unwrap();
            // End and append also perform the two eager size validations and
            // refresh the second request's position after the first finishes.
            assert_eq!(compounds, if kind < 2 { 2 } else { 5 } + u64::from(short));
            assert_eq!(waves, if short { 3 } else { 2 });
            assert_eq!(results[1].offset, if kind == 0 { 2 } else { 4 });
            assert_eq!(cursor, if kind == 1 { 6 } else { 0 });
            assert_eq!(
                contents[0].data,
                if kind == 0 {
                    b"abXY".as_slice()
                } else {
                    b"abcdXY".as_slice()
                }
            );
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn short_read_recovery_preserves_every_cursor_and_range() {
    for into in [false, true] {
        for recover in [false, true] {
            let Some(mut fs) = connect() else { return };
            fs.set_auto_reconnect(recover);
            let paths = [
                format!(
                    "/vfsi-repair-recovery-{}-{into}-{recover}-first",
                    std::process::id()
                ),
                format!(
                    "/vfsi-repair-recovery-{}-{into}-{recover}-second",
                    std::process::id()
                ),
            ];
            for (path, data) in paths.iter().zip([b"abcdefgh", b"ijklmnop"]) {
                fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
                    path,
                    vfsi_sync::VfOffset::At(0),
                    data.to_vec(),
                )
                .with_creation()
                .with_truncate()]))
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })
                    .unwrap();
            }
            let files = fs
                .vopen_raw_simple_impl(
                    &[Path::new(&paths[0]), Path::new(&paths[1])],
                    libc::O_RDONLY,
                    0,
                )
                .unwrap();
            let script = Arc::new(FaultScript::one(
                OpenFaultPoint::BeforeReadRepair { index: 1 },
                VfError::transport(None, "injected failure repairing the second READ"),
            ));
            fs.set_fault_injector(script.clone());
            fs.test_short_read_for_request_once(1, 2);
            let reads: Vec<_> = files
                .iter()
                .map(|file| vfsi_sync::ReadOp::new(file.clone(), vfsi_sync::VfOffset::Cur, 4))
                .collect();
            let outcome = if into {
                let mut first = [0; 4];
                let mut second = [0; 4];
                fs.vread_into_impl(&reads, &mut [&mut first, &mut second])
                    .and_then(|outcomes| {
                        if !recover {
                            assert_eq!(outcomes[0].as_ref().unwrap().bytes_read, 4);
                            let error = outcomes[1].as_ref().unwrap_err();
                            assert_eq!(error.state(), vfsi_core::FailureState::Failed);
                            let Some(vfsi_core::IoProgress::Read(prefix)) = error.progress() else {
                                panic!("missing buffered prefix");
                            };
                            assert!(prefix.is_buffered());
                            assert_eq!((prefix.offset(), prefix.bytes_read()), (0, 2));
                            assert_eq!(&first, b"abcd");
                            assert_eq!(&second[..2], b"ij");
                        }
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })
                    .map(|result| {
                        (
                            vec![first.to_vec(), second.to_vec()],
                            result.iter().map(|item| item.offset).collect::<Vec<_>>(),
                        )
                    })
            } else {
                fs.vread_impl(&reads)
                    .and_then(|outcomes| {
                        if !recover {
                            assert_eq!(outcomes[0].as_ref().unwrap().data, b"abcd");
                            let error = outcomes[1].as_ref().unwrap_err();
                            assert_eq!(error.state(), vfsi_core::FailureState::Failed);
                            let Some(vfsi_core::IoProgress::Read(prefix)) = error.progress() else {
                                panic!("missing prefix")
                            };
                            assert_eq!(prefix.data().unwrap(), b"ij");
                            assert_eq!(prefix.offset(), 0);
                        }
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })
                    .map(|result| {
                        (
                            result
                                .iter()
                                .map(|item| item.data.clone())
                                .collect::<Vec<_>>(),
                            result.iter().map(|item| item.offset).collect::<Vec<_>>(),
                        )
                    })
            };
            let cursors: Vec<_> = files
                .iter()
                .map(|file| fs.seek_raw_impl(file, 0, vfsi_sync::SeekFrom::Cur).unwrap())
                .collect();
            assert!(
                fs.vclose_impl(&files)
                    .unwrap()
                    .into_iter()
                    .all(|result| result.is_ok())
            );
            {
                let remove_files = &paths
                    .iter()
                    .map(|path| vfsi_sync::FileOperand::from_path(path))
                    .collect::<Vec<_>>();
                vfsi_core::internal::collect_vec_result(
                    fs.vremove_impl(remove_files),
                    remove_files.len(),
                )
                .map(|_| ())
            }
            .unwrap();
            assert!(script.is_consumed());
            if recover {
                let (observed, offsets) = outcome.unwrap();
                assert_eq!(
                    observed,
                    vec![b"abcd".to_vec(), b"ijkl".to_vec()],
                    "into={into}"
                );
                assert_eq!(offsets, vec![0, 0], "into={into}");
                assert_eq!(cursors, vec![4, 4], "into={into}");
            } else {
                assert!(outcome.unwrap_err().is_transport());
                assert_eq!(
                    cursors,
                    vec![4, 2],
                    "cursor publication must match the selected native contract: into={into}"
                );
            }
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn short_write_failure_does_not_dispatch_a_later_file() {
    for transport in [false, true] {
        let Some(mut fs) = connect() else { return };
        fs.set_max_compound_bytes(64 * 1024);
        let paths = [
            format!("/vfsi-wave-order-{}-{transport}-first", std::process::id()),
            format!("/vfsi-wave-order-{}-{transport}-second", std::process::id()),
        ];
        let files = fs
            .vopen_raw_simple_impl(
                &[Path::new(&paths[0]), Path::new(&paths[1])],
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o600,
            )
            .unwrap();
        let per = fs.test_io_chunk_bytes();
        let payload = vec![b'a'; per * 2 + 17];
        // A half-completed first chunk leaves room for the second file in the
        // repair wave, but not for the next full chunk of the first request.
        fs.test_short_write_once(per / 2);
        let script = Arc::new(FaultScript::one(
            OpenFaultPoint::AfterWriteChunk { chunk: 1 },
            if transport {
                VfError::transport(None, "injected lost reply after the repair wave")
            } else {
                VfError::errno(0, libc::ENOSPC as u32)
            },
        ));
        fs.set_fault_injector(script.clone());
        let error = fs
            .vwrite_impl(&borrow_writes(&[
                vfsi_sync::WriteOp::new(files[0].clone(), vfsi_sync::VfOffset::Cur, payload),
                vfsi_sync::WriteOp::at(files[1].clone(), 0, b"must not execute".to_vec()),
            ]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap_err();
        let first_cursor = fs
            .seek_raw_impl(&files[0], 0, vfsi_sync::SeekFrom::Cur)
            .unwrap();
        let later = fs
            .vread_impl(&[vfsi_sync::ReadOp::at(files[1].clone(), 0, 32)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        assert!(
            fs.vclose_impl(&files)
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
        {
            let remove_files = &paths
                .iter()
                .map(|path| vfsi_sync::FileOperand::from_path(path))
                .collect::<Vec<_>>();
            vfsi_core::internal::collect_vec_result(
                fs.vremove_impl(remove_files),
                remove_files.len(),
            )
            .map(|_| ())
        }
        .unwrap();
        assert!(script.is_consumed());
        if transport {
            assert!(error.is_transport());
            assert_eq!(error.index(), Some(0));
        } else {
            assert_eq!(error.raw_code(), libc::ENOSPC as u32);
            assert_eq!(error.index(), Some(0));
        }
        assert_eq!(first_cursor, per as i64);
        assert!(
            later[0].data.is_empty(),
            "a later file was modified before the earlier request finished"
        );
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn ordered_write_waves_still_batch_independent_small_files() {
    let Some(mut fs) = connect() else { return };
    fs.set_max_compound_bytes(64 * 1024);
    let paths: Vec<_> = (0..16)
        .map(|index| format!("/vfsi-small-wave-{}-{index}", std::process::id()))
        .collect();
    let refs: Vec<_> = paths.iter().map(Path::new).collect();
    let files = fs
        .vopen_raw_simple_impl(&refs, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
        .unwrap();
    let script = Arc::new(FaultScript::new([]));
    fs.set_fault_injector(script.clone());
    let writes: Vec<_> = files
        .iter()
        .map(|file| vfsi_sync::WriteOp::at(file.clone(), 0, b"batched".to_vec()))
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let results = fs
        .vwrite_impl(&borrow_writes(&writes))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    let waves = script
        .visited()
        .iter()
        .filter(|point| matches!(point, OpenFaultPoint::AfterWriteChunk { .. }))
        .count();
    let reads: Vec<_> = files
        .iter()
        .map(|file| vfsi_sync::ReadOp::at(file.clone(), 0, 7))
        .collect();
    let contents = fs
        .vread_impl(&reads)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    {
        let remove_files = &paths
            .iter()
            .map(|path| vfsi_sync::FileOperand::from_path(path))
            .collect::<Vec<_>>();
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
    assert_eq!(waves, 1, "small independent writes must remain vectorized");
    assert_eq!(
        compounds, 1,
        "small independent writes must use one real compound"
    );
    assert!(results.iter().all(|result| result.bytes_written == 7));
    assert!(contents.iter().all(|result| result.data == b"batched"));
}

#[cfg(feature = "test-faults")]
#[test]
fn descriptor_short_reads_preserve_contiguous_contents() {
    for into in [false, true] {
        let Some(mut fs) = connect() else { return };
        let path = format!("/vfsi-short-read-{}-{into}", std::process::id());
        let size = fs.test_io_chunk_bytes() * 2 + 17;
        let payload: Vec<_> = (0..size).map(|index| (index % 251) as u8).collect();
        fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
            &path,
            vfsi_sync::VfOffset::At(0),
            payload.clone(),
        )
        .with_creation()
        .with_truncate()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let files = fs
            .vopen_raw_simple_impl(&[Path::new(&path)], libc::O_RDONLY, 0)
            .unwrap();
        fs.test_short_read_once(7);
        let request = vfsi_sync::ReadOp::at(files[0].clone(), 0, size);
        let observed = if into {
            let mut buffer = vec![0; size];
            let result = fs
                .vread_into_impl(&[request], &mut [&mut buffer])
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap();
            buffer.truncate(result[0].bytes_read);
            buffer
        } else {
            fs.vread_impl(&[request])
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap()
                .remove(0)
                .data
        };
        assert!(
            fs.vclose_impl(&files)
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
        {
            let remove_files = &[vfsi_sync::FileOperand::from_path(&path)];
            vfsi_core::internal::collect_vec_result(
                fs.vremove_impl(remove_files),
                remove_files.len(),
            )
            .map(|_| ())
        }
        .unwrap();
        assert_eq!(
            observed.len(),
            payload.len(),
            "short reply must be filled: into={into}"
        );
        assert!(
            observed == payload,
            "split READ must not skip a gap: into={into}"
        );
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn descriptor_short_writes_preserve_contiguous_contents() {
    let Some(mut backend) = connect() else { return };
    let path = format!("/vfsi-short-write-{}", std::process::id());
    let size = backend.test_io_chunk_bytes() * 2 + 17;
    let payload: Vec<_> = (0..size).map(|index| (index % 251) as u8).collect();
    backend.test_short_write_once(7);
    let fs = vfsi_sync::FsClient::new(backend);
    let file = fs.create(&path).unwrap();
    let results = fs
        .vwrite_all_native(&[file.write_request_at(0, &payload)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(results[0].bytes_written, size);
    file.close().unwrap();
    let observed = fs.read(&path).unwrap();
    fs.remove_file(&path).unwrap();
    assert!(
        observed == payload,
        "successful vwrite_all_native must leave no unwritten gaps"
    );
}

#[test]
fn descriptor_positional_io_preserves_sequential_cursor() {
    let Some(mut backend) = connect() else { return };
    let path = format!("/vfsi-positional-cursor-{}", std::process::id());
    let file = backend
        .open_impl(&vfsi_sync::OpenOp::new(
            &path,
            vfsi_sync::OpenFlags::READ
                | vfsi_sync::OpenFlags::WRITE
                | vfsi_sync::OpenFlags::CREATE
                | vfsi_sync::OpenFlags::TRUNCATE,
        ))
        .unwrap();
    backend
        .write_impl(vfsi_sync::WriteOp::new(&file, VfOffset::At(0), b"abcdefgh"))
        .unwrap();
    for kind in 0..5 {
        backend
            .seek_impl(&file, std::io::SeekFrom::Start(0))
            .unwrap();
        let mut buffer = [0; 2];
        let read = vfsi_sync::ReadOp::at(file.clone(), 4, 2);
        let write = vfsi_sync::WriteOp::new(&file, VfOffset::At(4), &b"XY"[..]);
        match kind {
            0 => {
                backend.read_into_impl(&read, &mut buffer).unwrap();
            }
            1 => {
                backend
                    .vread_impl(&[read])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })
                    .unwrap();
            }
            2 => {
                backend
                    .vread_into_impl(&[read], &mut [&mut buffer])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })
                    .unwrap();
            }
            3 => {
                backend.write_impl(write).unwrap();
            }
            _ => {
                backend
                    .vwrite_impl(&[write])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })
                    .unwrap();
            }
        }
        let cursor = backend
            .seek_impl(&file, std::io::SeekFrom::Current(0))
            .unwrap();
        assert_eq!(cursor, 0, "positional operation {kind} changed cursor");
        let result = backend
            .read_impl(&vfsi_sync::ReadOp::new(file.clone(), VfOffset::Cur, 2))
            .unwrap();
        assert_eq!(result.data, b"ab");
    }
    backend.close_impl(&file).unwrap();
    backend.unlink_impl(Path::new(&path)).unwrap();
}

#[test]
fn reconnect_keeps_renamed_object_when_original_path_is_replaced() {
    let Some(mut fs) = connect() else { return };
    let path = format!("/vfsi-reconnect-identity-{}", std::process::id());
    let saved = format!("{path}-original");
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        &path,
        vfsi_sync::VfOffset::At(0),
        b"original".to_vec(),
    )
    .with_creation()
    .with_truncate()]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    let files = fs
        .vopen_raw_simple_impl(&[Path::new(&path)], libc::O_RDWR, 0)
        .unwrap();
    // Also exercise the successful unchanged-identity recovery path.
    fs.reconnect().unwrap();
    {
        let rename_pairs = &[(
            vfsi_sync::FileOperand::from_path(&path),
            vfsi_sync::FileOperand::from_path(&saved),
        )];
        vfsi_core::internal::collect_vec_result(fs.vrename_impl(rename_pairs), rename_pairs.len())
            .map(|_| ())
    }
    .unwrap();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        &path,
        vfsi_sync::VfOffset::At(0),
        b"replaced".to_vec(),
    )
    .with_creation()
    .with_truncate()]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    let result = fs.reconnect();
    let old_contents = fs
        .vread_impl(&[vfsi_sync::ReadOp::at(files[0].clone(), 0, 8)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    {
        let remove_files = &[
            vfsi_sync::FileOperand::from_path(&path),
            vfsi_sync::FileOperand::from_path(&saved),
        ];
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
    result.unwrap();
    assert_eq!(
        old_contents[0].data, b"original",
        "recovery must reopen the original object, not its replacement path"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn short_writes_do_not_reorder_overlapping_or_cursor_requests() {
    for cursor in [false, true] {
        let Some(mut fs) = connect() else { return };
        fs.set_max_compound_bytes(64 * 1024);
        let path = format!("/vfsi-short-order-{}-{cursor}", std::process::id());
        let length = fs.test_io_chunk_bytes() * 2 + 17;
        let first = vec![b'a'; length];
        let second = vec![b'b'; length / 2];
        let files = fs
            .vopen_raw_simple_impl(
                &[Path::new(&path)],
                libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
                0o600,
            )
            .unwrap();
        fs.test_short_write_once(7);
        let offset = if cursor {
            vfsi_sync::VfOffset::Cur
        } else {
            vfsi_sync::VfOffset::At(0)
        };
        let results = fs
            .vwrite_impl(&borrow_writes(&[
                vfsi_sync::WriteOp::new(files[0].clone(), offset, first.clone()),
                vfsi_sync::WriteOp::new(files[0].clone(), offset, second.clone()),
            ]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        assert_eq!(results[0].bytes_written, first.len());
        assert_eq!(results[1].bytes_written, second.len());
        assert_eq!(results[1].offset, if cursor { length as u64 } else { 0 });
        let position = fs
            .seek_raw_impl(&files[0], 0, vfsi_sync::SeekFrom::Cur)
            .unwrap();
        assert_eq!(
            position,
            if cursor {
                (length + second.len()) as i64
            } else {
                0
            }
        );
        assert!(
            fs.vclose_impl(&files)
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
        let observed = fs
            .vread_all_impl(&[vfsi_sync::FileOperand::from_path(&path)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let expected = if cursor {
            [first, second].concat()
        } else {
            [second.clone(), first[second.len()..].to_vec()].concat()
        };
        {
            let remove_files = &[vfsi_sync::FileOperand::from_path(&path)];
            vfsi_core::internal::collect_vec_result(
                fs.vremove_impl(remove_files),
                remove_files.len(),
            )
            .map(|_| ())
        }
        .unwrap();
        assert!(
            observed[0] == expected,
            "short writes reordered a later request: cursor={cursor}"
        );
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn descriptor_zero_progress_is_an_error_not_a_retry_loop() {
    for read in [false, true] {
        for into in [false, true] {
            let Some(mut fs) = connect() else { return };
            let path = format!("/vfsi-no-progress-{}-{read}-{into}", std::process::id());
            fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
                &path,
                vfsi_sync::VfOffset::At(0),
                b"contents".to_vec(),
            )
            .with_creation()
            .with_truncate()]))
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap();
            let files = fs
                .vopen_raw_simple_impl(&[Path::new(&path)], libc::O_RDWR, 0)
                .unwrap();
            let result = if read {
                fs.test_short_read_once(0);
                let request = vfsi_sync::ReadOp::new(files[0].clone(), vfsi_sync::VfOffset::Cur, 4);
                if into {
                    fs.vread_into_impl(&[request], &mut [&mut [0; 4]])
                        .and_then(|outcomes| {
                            let count = outcomes.len();
                            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                        })
                        .map(|_| ())
                } else {
                    fs.vread_impl(&[request])
                        .and_then(|outcomes| {
                            let count = outcomes.len();
                            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                        })
                        .map(|_| ())
                }
            } else {
                fs.test_short_write_once(0);
                fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::new(
                    files[0].clone(),
                    vfsi_sync::VfOffset::Cur,
                    b"new".to_vec(),
                )]))
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .map(|_| ())
            };
            assert_eq!(result.unwrap_err().raw_code(), libc::EIO as u32);
            assert_eq!(
                fs.seek_raw_impl(&files[0], 0, vfsi_sync::SeekFrom::Cur)
                    .unwrap(),
                0
            );
            assert!(
                fs.vclose_impl(&files)
                    .unwrap()
                    .into_iter()
                    .all(|result| result.is_ok())
            );
            assert_eq!(
                fs.vread_all_impl(&[vfsi_sync::FileOperand::from_path(&path)])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })
                    .unwrap()[0],
                b"contents"
            );
            {
                let remove_files = &[vfsi_sync::FileOperand::from_path(&path)];
                vfsi_core::internal::collect_vec_result(
                    fs.vremove_impl(remove_files),
                    remove_files.len(),
                )
                .map(|_| ())
            }
            .unwrap();
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn short_reads_handle_eof_and_requests_beyond_the_file() {
    for (into, small) in [(false, false), (true, false), (false, true), (true, true)] {
        let Some(mut fs) = connect() else { return };
        fs.set_max_compound_bytes(64 * 1024);
        let path = format!("/vfsi-short-eof-{}-{into}-{small}", std::process::id());
        let size = if small {
            6
        } else {
            fs.test_io_chunk_bytes() + 17
        };
        let payload = vec![b'x'; size];
        fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
            &path,
            vfsi_sync::VfOffset::At(0),
            payload.clone(),
        )
        .with_creation()
        .with_truncate()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let files = fs
            .vopen_raw_simple_impl(&[Path::new(&path)], libc::O_RDONLY, 0)
            .unwrap();
        fs.test_short_read_once(if small { 2 } else { 7 });
        let request = vfsi_sync::ReadOp::at(files[0].clone(), 0, size * 2);
        let (observed, eof) = if into {
            let mut buffer = vec![0; size * 2];
            let result = fs
                .vread_into_impl(&[request], &mut [&mut buffer])
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap();
            buffer.truncate(result[0].bytes_read);
            (buffer, result[0].eof)
        } else {
            let result = fs
                .vread_impl(&[request])
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap()
                .remove(0);
            (result.data, result.eof)
        };
        assert!(
            fs.vclose_impl(&files)
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
        {
            let remove_files = &[vfsi_sync::FileOperand::from_path(&path)];
            vfsi_core::internal::collect_vec_result(
                fs.vremove_impl(remove_files),
                remove_files.len(),
            )
            .map(|_| ())
        }
        .unwrap();
        assert!(observed == payload);
        assert!(eof);
    }
}

fn connect() -> Option<NfsVecFs> {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(
                !required(),
                "VFSI_NFS_SERVER is required in this integration job"
            );
            return None;
        }
    };
    let client = match std::env::var("VFSI_NFS_MINOR").as_deref() {
        Ok("1") => NfsVecFs::connect_minor(&server, 1),
        Ok("2") => NfsVecFs::connect_minor(&server, 2),
        Ok(value) => panic!("unsupported VFSI_NFS_MINOR={value}"),
        Err(_) => NfsVecFs::connect(&server),
    };
    Some(client.expect("connect to configured NFS server"))
}

#[test]
fn symlinked_directories_work_for_mkdir_and_batched_listdir() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping NFS integration test: VFSI_NFS_SERVER not set");
        return;
    };
    let root = format!("/vfsi-followed-directory-{}", std::process::id());
    let target = format!("{root}/target");
    let link = format!("{root}/link");
    let child = format!("{link}/created");
    let _ = {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    };

    let result: vfsi_core::VfResult<Vec<String>> = (|| {
        fs.vmkdir_impl(&[VfAttrs {
            file: FileOperand::from_path(&root),
            mode: 0o755,
            ..VfAttrs::default()
        }])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })?;
        fs.vmkdir_impl(&[VfAttrs {
            file: FileOperand::from_path(&target),
            mode: 0o755,
            ..VfAttrs::default()
        }])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })?;
        fs.vsymlink_impl(&[Path::new("target")], &[Path::new(&link)])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
            })?;
        fs.vmkdir_impl(&[VfAttrs {
            file: FileOperand::from_path(&child),
            mode: 0o755,
            ..VfAttrs::default()
        }])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })?;

        let dirs = [Path::new(&link), Path::new(&target)];
        let mut listed = Vec::new();
        fs.vlistdirs_impl(&dirs, AttrMask::empty(), 0, false, &mut |entry, _dir| {
            listed.push(entry.file.path().unwrap().to_string_lossy().into_owned());
            true
        })?;
        Ok(listed)
    })();
    let cleanup = {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    };

    let listed = result.expect("operations through followed directory symlink");
    cleanup.expect("remove symlink test fixture");
    assert!(listed.contains(&child));
    assert!(listed.contains(&format!("{target}/created")));
}

#[cfg(feature = "test-faults")]
#[test]
fn batched_symlink_resolution_preserves_transport_errors() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/vfsi-symlink-transport-{}", std::process::id());
    let target = format!("{root}/target");
    let link = format!("{root}/link");
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    fs.ensure_dir_impl(Path::new(&target), 0o755).unwrap();
    fs.vsymlink_impl(&[Path::new("target")], &[Path::new(&link)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap();
    fs.set_auto_reconnect(false);
    let dirs = [Path::new(&target), Path::new(&link)];
    let mut outcomes = Vec::new();
    for kind in [
        vfsi_core::TransportKind::Timeout,
        vfsi_core::TransportKind::Connection,
        vfsi_core::TransportKind::Auth,
        vfsi_core::TransportKind::InvalidReply,
    ] {
        for index in [None, Some(0)] {
            let injected =
                VfError::transport_with_kind(index, kind, "symlink target lookup failed")
                    .with_context("resolve symlink", &link);
            // Native page outcomes retain the transport provenance in the
            // failed input's slot while preserving the healthy sibling.
            let expected = vfsi_core::OpError::unknown(injected.clone(), None).into_cause();
            let fault = Arc::new(FaultScript::one(
                OpenFaultPoint::BeforeFollowSymlink { index: 1 },
                injected,
            ));
            fs.set_fault_injector(fault.clone());
            let actual =
                fs.vlistdir_pages_impl(&dirs, AttrMask::MODE, vec![None, None], 1, 10, true);
            outcomes.push((expected, actual, fault.is_consumed()));
        }
    }
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    for (expected, actual, consumed) in outcomes {
        assert!(consumed, "the live symlink fallback must execute");
        let actual = actual.unwrap();
        assert_eq!(actual.len(), 2);
        assert!(actual[0].is_ok());
        assert_eq!(
            actual[1]
                .as_ref()
                .err()
                .expect("injected lookup failure")
                .cause(),
            &expected
        );
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn batched_symlink_resolution_recovers_after_a_transport_failure() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/vfsi-symlink-recovery-{}", std::process::id());
    let target = format!("{root}/target");
    let link = format!("{root}/link");
    fs.ensure_dir_impl(Path::new(&root), 0o755).unwrap();
    fs.ensure_dir_impl(Path::new(&target), 0o755).unwrap();
    fs.vsymlink_impl(&[Path::new("target")], &[Path::new(&link)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap();
    fs.set_auto_reconnect(true);
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeFollowSymlink { index: 1 },
        VfError::transport_with_kind(
            None,
            vfsi_core::TransportKind::Connection,
            "symlink target connection lost",
        ),
    ));
    fs.set_fault_injector(fault.clone());
    let pages = fs.vlistdir_pages_impl(
        &[Path::new(&target), Path::new(&link)],
        AttrMask::MODE,
        vec![None, None],
        1,
        10,
        true,
    );
    let resolutions = fault
        .visited()
        .iter()
        .filter(|point| matches!(point, OpenFaultPoint::BeforeFollowSymlink { index: 1 }))
        .count();
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    assert!(fault.is_consumed());
    let pages = vfsi_core::internal::collect_vec_result(pages, 2)
        .expect("read-only symlink lookup should recover and retry");
    assert_eq!(pages.len(), 2);
    assert!(pages.iter().all(|(entries, _, _)| entries.is_empty()));
    assert_eq!(
        resolutions, 1,
        "the batched healthy sibling is not resolved again; recovery uses the scalar failed-path fast path"
    );
}

#[test]
fn shared_contract_through_the_published_crate() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping NFS integration test: VFSI_NFS_SERVER not set");
        return;
    };
    let root = format!("/vfsi-nfs-contract-{}", std::process::id());
    let _ = {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    };
    test_support::run_suite(&mut fs, &root);
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .expect("remove NFS contract root");
}

#[test]
fn read_into_preserves_siblings_after_callback_error() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let name = format!("vfsi-nfs-read-into-abort-{}", std::process::id());
    let (fh, stateid) = client
        .open(
            &root,
            name.as_bytes(),
            nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            OpenCreate::Unchecked,
        )
        .expect("create test file");
    client
        .write(&fh, &stateid, 0, b"abcdef")
        .expect("write test file");
    let mut reads: Vec<ReadOp> = (0..3)
        .map(|index| ReadOp {
            fh: fh.clone(),
            stateid,
            offset: index * 2,
            count: 2,
        })
        .collect();
    // Both destinations use the same wire engine, including empty EOF replies.
    reads.push(ReadOp {
        fh: fh.clone(),
        stateid,
        offset: 6,
        count: 3,
    });
    let owned = client.readv(&reads).unwrap();
    let mut bytes = Vec::new();
    let into = client
        .readv_into(&reads, |index, data| {
            assert_eq!(index, bytes.len());
            bytes.push(data.to_vec());
            Ok(())
        })
        .unwrap();
    assert_eq!(owned.len(), reads.len());
    assert_eq!(into.len(), reads.len());
    assert_eq!(bytes.len(), reads.len());
    for ((data, result), expected) in bytes.iter().zip(into).zip(owned) {
        let (len, eof) = result.unwrap();
        let (expected, expected_eof) = expected.unwrap();
        assert_eq!(data, &expected);
        assert_eq!(len, expected.len());
        assert_eq!(eof, expected_eof);
    }
    let mut calls = 0usize;
    let outcomes = client
        .readv_into(&reads, |_, _| {
            calls += 1;
            if calls == 2 {
                Err(RpcError::transport("injected callback failure"))
            } else {
                Ok(())
            }
        })
        .expect("dispatch succeeds despite an individual decoder failure");
    client.close(&fh, &stateid).expect("close test file");
    client.remove(&root, &name).expect("remove test file");
    let error = outcomes[1].as_ref().unwrap_err();
    assert!(error.cause().is_transport());
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok() && outcomes[3].is_ok());
    assert_eq!(
        calls,
        reads.len(),
        "a failed destination must not discard siblings"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn pre_dispatch_resource_rejection_splits_merged_writes_without_replay() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let dirname = format!("vfsi-nfs-resource-fault-{}", std::process::id());
    let dir = client
        .mkdir(&root, &dirname)
        .expect("create test directory");
    let names: Vec<Vec<u8>> = (0..24)
        .map(|index| format!("file-{index}").into_bytes())
        .collect();
    let payloads: Vec<Vec<u8>> = (0..names.len())
        .map(|index| vec![index as u8; 16])
        .collect();
    let writes: Vec<PathWriteOp> = names
        .iter()
        .zip(&payloads)
        .map(|(name, data)| PathWriteOp {
            file: FileRef::Path(
                format!("{dirname}/{}", String::from_utf8_lossy(name)).into_bytes(),
            ),
            offset: 0,
            data,
            create: true,
            truncate: true,
            stateid: None,
        })
        .collect();

    client.inject_resource_rejection_once(b"writev1");
    let result = client
        .writev_path_compound(&writes, true)
        .expect("retry rejected write compound");
    assert!(!client.resource_rejection_pending());
    assert!(result.results.iter().all(Result::is_ok));
    assert_eq!(result.close_failed, None);
    assert_eq!(
        result
            .results
            .iter()
            .map(|result| result.as_ref().unwrap().0)
            .collect::<Vec<_>>(),
        vec![16; writes.len()]
    );
    for (index, name) in names.iter().enumerate() {
        let (fh, stateid) = client
            .open(
                &dir,
                name,
                nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
                OpenCreate::NoCreate,
            )
            .expect("open written file");
        let (data, _) = client
            .read(&fh, &stateid, 0, 32)
            .expect("read written file");
        assert_eq!(data, vec![index as u8; 16]);
        client.close(&fh, &stateid).expect("close written file");
    }
    {
        let remove_files = &names;
        vfsi_core::internal::collect_vec_result(
            client.remove_many(&dir, remove_files),
            remove_files.len(),
        )
        .map(|_| ())
    }
    .expect("remove test files");
    client
        .remove(&root, &dirname)
        .expect("remove test directory");
}

#[cfg(feature = "test-faults")]
#[test]
fn split_path_write_truncates_only_before_the_first_chunk() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let name = format!("vfsi-nfs-split-truncate-{}", std::process::id());
    let data = vec![b'x'; 64 * 1024];

    // The limit includes framing, so the payload must span two compounds.
    client.set_max_compound_bytes(64 * 1024);
    let outcome = client
        .writev_path_compound(
            &[PathWriteOp {
                file: FileRef::Path(name.as_bytes().to_vec()),
                offset: 0,
                data: &data,
                create: true,
                truncate: true,
                stateid: None,
            }],
            true,
        )
        .expect("write split path payload");
    assert!(outcome.results.iter().all(Result::is_ok));
    assert_eq!(outcome.results[0].as_ref().unwrap().0, data.len());
    client.set_max_compound_bytes(0);

    let (fh, stateid) = client
        .open(
            &root,
            name.as_bytes(),
            nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
            OpenCreate::NoCreate,
        )
        .expect("open written file");
    let (actual, _) = client
        .read(&fh, &stateid, 0, data.len() as u32)
        .expect("read written file");
    assert_eq!(actual, data);
    client.close(&fh, &stateid).expect("close written file");
    client.remove(&root, &name).expect("remove test file");
}

#[cfg(feature = "test-faults")]
#[test]
fn variable_depth_paths_repack_before_exceeding_compound_limit() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let dirname = format!("vfsi-nfs-deep-path-{}", std::process::id());
    let mut parent = client
        .mkdir(&root, &dirname)
        .expect("create root directory");
    let top = parent.clone();
    let mut dirs = vec![(root.clone(), dirname.clone())];
    let mut deep_path = dirname.clone();
    for index in 0..24 {
        let name = format!("d{index}");
        let child = client.mkdir(&parent, &name).expect("create path component");
        dirs.push((parent, name.clone()));
        parent = child;
        deep_path.push('/');
        deep_path.push_str(&name);
    }

    // The shallow and deep OPENs each fit in 32 operations, but the
    // combined variable-length path resolution does not. The builder must
    // repack before dispatch instead of failing its hard-cap check.
    client.max_ops = 32;
    let outcome = client
        .openv_path_compound(&[
            PathOpenOp {
                path: format!("{dirname}/shallow").into_bytes(),
                access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                create: OpenCreate::Unchecked,
                mode: None,
                truncate: false,
            },
            PathOpenOp {
                path: format!("{deep_path}/deep").into_bytes(),
                access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                create: OpenCreate::Unchecked,
                mode: None,
                truncate: false,
            },
        ])
        .expect("split variable-length paths");
    for (fh, stateid) in outcome.into_iter().map(Result::unwrap) {
        client.close(&fh, &stateid).expect("close opened file");
    }
    client.remove(&top, "shallow").expect("remove shallow file");
    client.remove(&parent, "deep").expect("remove deep file");
    for (dir, name) in dirs.into_iter().rev() {
        client.remove(&dir, &name).expect("remove test directory");
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn encoded_request_limit_rejects_oversize_write_before_dispatch() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let name = format!("vfsi-nfs-byte-limit-{}", std::process::id());
    let (fh, stateid) = client
        .open(
            &root,
            name.as_bytes(),
            nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            OpenCreate::Unchecked,
        )
        .expect("create test file");
    client.set_max_compound_bytes(1400);
    let error = client
        .write(&fh, &stateid, 0, &[7; 512])
        .expect_err("oversize encoded request must be rejected locally");
    assert_eq!(error.status, nfsv41_sys::nfsstat4_NFS4ERR_REQ_TOO_BIG);
    client.set_max_compound_bytes(0);
    let (data, _) = client.read(&fh, &stateid, 0, 512).expect("read test file");
    assert!(data.is_empty(), "rejected WRITE must not reach the server");
    let oversized_read = client
        .readv(&[ReadOp {
            fh: fh.clone(),
            stateid,
            offset: 0,
            count: u32::MAX,
        }])
        .expect_err("unbounded READ reply must be rejected locally");
    assert_eq!(
        oversized_read.raw_code(),
        nfsv41_sys::nfsstat4_NFS4ERR_REP_TOO_BIG
    );
    client.close(&fh, &stateid).expect("close test file");
    client.remove(&root, &name).expect("remove test file");
}

#[cfg(feature = "test-faults")]
#[test]
fn writev_rebuilds_batch_before_exceeding_request_bytes() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let name = format!("vfsi-nfs-byte-batch-{}", std::process::id());
    let (fh, stateid) = client
        .open(
            &root,
            name.as_bytes(),
            nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            OpenCreate::Unchecked,
        )
        .expect("create test file");
    let block = 512 * 1024;
    let payloads: Vec<Vec<u8>> = (0..8).map(|index| vec![index as u8; block]).collect();
    let ops: Vec<WriteOp> = (0..8)
        .map(|index| WriteOp {
            fh: fh.clone(),
            stateid,
            offset: (index * block) as u64,
            data: &payloads[index],
        })
        .collect();
    let written = client
        .writev(&ops)
        .expect("repack encoded byte-heavy batch");
    assert_eq!(written.len(), ops.len());
    assert!(
        written
            .iter()
            .all(|result| result.as_ref().unwrap().0 as usize == block)
    );
    let reads: Vec<ReadOp> = (0..8)
        .map(|index| ReadOp {
            fh: fh.clone(),
            stateid,
            offset: (index * block) as u64,
            count: block as u32,
        })
        .collect();
    let read_results = client.readv(&reads).expect("bound aggregate reply bytes");
    for (index, result) in read_results.iter().enumerate() {
        let (data, _) = result.as_ref().unwrap();
        assert_eq!(data.len(), block);
        assert!(data.iter().all(|byte| *byte == index as u8));
    }
    let (data, _) = client
        .read(&fh, &stateid, (7 * block) as u64, 16)
        .expect("read last block");
    assert_eq!(data, vec![7; 16]);
    client.close(&fh, &stateid).expect("close test file");
    client.remove(&root, &name).expect("remove test file");
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_repacks_variable_length_names_for_encoded_byte_limit() {
    let server = match std::env::var("VFSI_NFS_SERVER") {
        Ok(server) => server,
        Err(_) => {
            assert!(!required(), "VFSI_NFS_SERVER is required");
            return;
        }
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .ok()
        .map(|value| value.parse::<u32>().expect("NFS minor version"))
        .unwrap_or(2);
    let mut client = NfsClient::connect_minor(&server, minor).expect("connect NFS client");
    let root = client.root().clone();
    let dirname = format!("vfsi-nfs-byte-path-{}", std::process::id());
    let dir = client
        .mkdir(&root, &dirname)
        .expect("create test directory");
    let names: Vec<String> = (0..4)
        .map(|index| format!("{index}-{}", "x".repeat(236)))
        .collect();
    let opens: Vec<PathOpenOp> = names
        .iter()
        .map(|name| PathOpenOp {
            path: format!("{dirname}/{name}").into_bytes(),
            access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            create: OpenCreate::Unchecked,
            mode: None,
            truncate: false,
        })
        .collect();
    client.set_max_compound_bytes(2100);
    let outcome = client
        .openv_path_compound(&opens)
        .expect("repack encoded variable-length paths");
    client.set_max_compound_bytes(0);
    for (fh, stateid) in outcome.into_iter().map(Result::unwrap) {
        client.close(&fh, &stateid).expect("close test file");
    }
    for name in names {
        client.remove(&dir, &name).expect("remove test file");
    }
    client
        .remove(&root, &dirname)
        .expect("remove test directory");
}

#[cfg(feature = "test-faults")]
#[test]
fn failed_strict_open_quarantines_an_unconfirmed_cleanup_handle() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping NFS integration test: VFSI_NFS_SERVER not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-nfs-cleanup-fault-{}", std::process::id()));
    let _ = {
        let remove_roots: &[&std::path::Path] = &[root.as_path()];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    };
    fs.mkdir_raw_impl(root.as_path(), 0o755).unwrap();
    let paths = [root.join("one"), root.join("two")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let script = Arc::new(FaultScript::new([
        (
            OpenFaultPoint::BeforeRegister { index: 1 },
            VfError::transport(None, "injected registration failure"),
        ),
        (
            OpenFaultPoint::BeforeCloseDispatch { index: 0 },
            VfError::transport(None, "injected cleanup close failure"),
        ),
    ]));
    fs.set_fault_injector(script.clone());
    assert!(
        vfsi_sync::backend::helpers::open_raw_batch(
            &mut fs,
            &refs,
            &[libc::O_CREAT | libc::O_RDWR; 2],
            &[0o600; 2],
        )
        .unwrap_err()
        .is_transport()
    );
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
    assert_eq!(fs.test_deferred_descriptor_close_count(), 1);
    let next_path = root.join("next");
    let next = vfsi_sync::backend::helpers::open_raw_batch(
        &mut fs,
        &[next_path.as_path()],
        &[libc::O_CREAT | libc::O_RDWR],
        &[0o600],
    )
    .unwrap();
    assert_eq!(fs.test_deferred_descriptor_close_count(), 0);
    assert!(
        fs.vclose_impl(&next)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    {
        let remove_roots: &[&std::path::Path] = &[root.as_path()];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
}

#[test]
fn reconnect_restores_independent_opens_after_rename() {
    let Some(mut fs) = connect() else { return };
    let path = format!("/vfsi-reconnect-opens-{}", std::process::id());
    let renamed = format!("{path}-renamed");
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        &path,
        VfOffset::At(0),
        b"abcdef".to_vec(),
    )
    .with_creation()
    .with_truncate()]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    let files = fs
        .vopen_raw_simple_impl(&[Path::new(&path), Path::new(&path)], libc::O_RDONLY, 0)
        .unwrap();
    {
        let rename_pairs = &[(
            FileOperand::from_path(&path),
            FileOperand::from_path(&renamed),
        )];
        vfsi_core::internal::collect_vec_result(fs.vrename_impl(rename_pairs), rename_pairs.len())
            .map(|_| ())
    }
    .unwrap();
    fs.seek_raw_impl(&files[0], 2, vfsi_sync::SeekFrom::Set)
        .unwrap();
    fs.reconnect().unwrap();
    let read = fs
        .vread_impl(&[vfsi_sync::ReadOp::new(files[0].clone(), VfOffset::Cur, 2)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(read[0].data, b"cd");
    assert!(
        fs.vclose_impl(&files[..1])
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    let read = fs
        .vread_impl(&[vfsi_sync::ReadOp::new(files[1].clone(), VfOffset::Cur, 6)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    assert_eq!(read[0].data, b"abcdef");
    assert!(
        fs.vclose_impl(&files[1..])
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    {
        let remove_files = &[FileOperand::from_path(&renamed)];
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
}

#[test]
fn shared_pruning_contract_on_nfs() {
    if vfsi_sync::test_support::supervise_with_deadline("shared_pruning_contract_on_nfs") {
        return;
    }
    let Some(backend) = connect() else { return };
    let fs = vfsi_sync::FsClient::new(backend);
    let base = format!("/pruning-contract-{}", std::process::id());
    test_support::run_pruning_suite(&fs, &fs, &base, true);
    fs.remove_dir_all(&base).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn paged_traversal_callbacks_are_not_replayed_during_initial_reconnect() {
    use vfsi_core::api::ListDirControl;
    let Some(mut backend) = connect() else { return };
    let root = format!("/pruning-reconnect-{}", std::process::id());
    backend
        .ensure_dir_impl(Path::new(&format!("{root}/target/child")), 0o777)
        .unwrap();
    backend
        .vsymlink_impl(
            &[Path::new("target")],
            &[Path::new(&format!("{root}/link"))],
        )
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap();
    backend.set_auto_reconnect(true);
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeFollowSymlink { index: 1 },
        VfError::transport_with_kind(
            None,
            vfsi_core::TransportKind::Connection,
            "initial traversal connection lost",
        ),
    ));
    backend.set_fault_injector(fault.clone());
    let fs = FsClient::new(backend);
    let paths = [format!("{root}/target"), format!("{root}/link")];
    let mut calls = [0; 2];
    let done = fs
        .listdirs_all(&paths, ListDirOptions::new(), |index, page| {
            calls[index] += 1;
            assert_eq!(page.entries.len(), 1);
            Ok(ListDirControl::Continue)
        })
        .unwrap();
    assert_eq!(done, [TraversalCompletion::Complete; 2]);
    assert_eq!(
        calls,
        [1, 1],
        "each root callback runs once, after successful recovery"
    );
    assert!(fault.is_consumed());
    assert_eq!(
        fault
            .visited()
            .iter()
            .filter(|p| matches!(p, OpenFaultPoint::BeforeFollowSymlink { index: 1 }))
            .count(),
        1,
        "selective recovery does not repeat the original batched sibling lookup"
    );
    fs.remove_dir_all(&root).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn pruning_pages_retry_only_before_delivery_and_fail_closed_after_delivery() {
    use vfsi_core::api::ListDirControl;
    for continuation in [false, true] {
        let Some(mut backend) = connect() else { return };
        let root = format!("/pruning-page-fault-{}-{continuation}", std::process::id());
        for child in ["a", "b"] {
            backend
                .ensure_dir_impl(Path::new(&format!("{root}/{child}")), 0o777)
                .unwrap();
        }
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeDirectoryPage { continuation },
            VfError::transport_with_kind(
                None,
                vfsi_core::TransportKind::Connection,
                "directory page connection lost",
            ),
        ));
        backend.set_auto_reconnect(true);
        backend.set_fault_injector(fault.clone());
        let fs = FsClient::new(backend);
        let mut calls = 0;
        let result = fs.listdirs_all(
            &[&root],
            ListDirOptions::new().with_recursive(true),
            |_, page| {
                calls += 1;
                assert_eq!(
                    page.path,
                    Path::new(&root),
                    "pruned children are never visited"
                );
                Ok(ListDirControl::Prune(
                    page.entries
                        .iter()
                        .map(|e| e.path().to_path_buf())
                        .collect(),
                ))
            },
        );
        assert!(fault.is_consumed(), "page fault must actually fire");
        if continuation {
            let error = result.unwrap_err();
            assert_eq!(
                error.transport_kind(),
                Some(vfsi_core::TransportKind::Connection)
            );
            assert_eq!(
                error.index(),
                Some(0),
                "strict collection identifies the root without implying a completed prefix"
            );
            assert_eq!(
                calls, 1,
                "do not replay an already delivered admission page"
            );
        } else {
            assert_eq!(result.unwrap(), [TraversalCompletion::Complete]);
            assert_eq!(
                calls, 2,
                "first page plus continuation, each delivered once"
            );
            assert_eq!(
                fault
                    .visited()
                    .iter()
                    .filter(|point| **point
                        == OpenFaultPoint::BeforeDirectoryPage {
                            continuation: false
                        })
                    .count(),
                2
            );
        }
        fs.remove_dir_all(&root).unwrap();
    }
}

fn file_review_mixed_descriptor_end_offsets(for_write: bool) {
    let Some(mut fs) = connect() else { return };
    for mode in ["full", "openwrite", "off"] {
        fs.set_merged_mode(mode);
        let root = std::path::PathBuf::from(format!(
            "/vfsi-file-review-end-{}-{for_write}-{mode}",
            std::process::id()
        ));
        fs.ensure_dir_impl(&root, 0o755).unwrap();
        let path = root.join("file");
        let moved = root.join("moved");
        let other_path = root.join("other");
        let fd = fs
            .open_raw_impl(&path, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600)
            .unwrap();
        let other = FileOperand::from_os_path(&other_path);
        let operation = (|| -> vfsi_core::VfResult<()> {
            fs.write_raw_impl(&fd, 0, b"abc")?;
            fs.vwrite_impl(&[
                vfsi_sync::WriteOp::new(&other, VfOffset::At(0), b"xyz".as_slice()).with_creation(),
            ])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })?;
            // Resolve EOF from the retained object, even after its path changed.
            {
                let rename_pairs = &[(
                    FileOperand::from_os_path(&path),
                    FileOperand::from_os_path(&moved),
                )];
                vfsi_core::internal::collect_vec_result(
                    fs.vrename_impl(rename_pairs),
                    rename_pairs.len(),
                )
                .map(|_| ())
            }?;
            if for_write {
                let single = fs
                    .vwrite_impl(&[vfsi_sync::WriteOp::new(&fd, VfOffset::End, b"d".as_slice())])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })?;
                assert_eq!(single[0].offset, 3);
                let mixed = fs
                    .vwrite_impl(&[
                        vfsi_sync::WriteOp::new(&other, VfOffset::At(0), b"X".as_slice()),
                        vfsi_sync::WriteOp::new(&fd, VfOffset::End, b"e".as_slice()),
                    ])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })?;
                assert_eq!(mixed.len(), 2);
                assert_eq!(mixed[1].offset, 4);
                assert_eq!(mixed[1].bytes_written, 1);
                assert_eq!(fs.read_raw_impl(&fd, 0, 8)?, b"abcde");
                assert_eq!(fs.read_raw_impl(&other, 0, 8)?, b"Xyz");
            } else {
                let single = fs
                    .vread_impl(&[vfsi_sync::ReadOp::new(fd.clone(), VfOffset::End, 1)])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })?;
                assert_eq!(single[0].offset, 3);
                assert!(single[0].data.is_empty());
                let reads = [
                    vfsi_sync::ReadOp::at(other.clone(), 0, 1),
                    vfsi_sync::ReadOp::new(fd.clone(), VfOffset::End, 1),
                ];
                let mixed = fs.vread_impl(&reads).and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })?;
                assert_eq!(mixed.len(), 2);
                assert_eq!(mixed[0].data, b"x");
                assert_eq!(mixed[1].offset, 3);
                assert!(mixed[1].data.is_empty());
                assert!(mixed[1].eof);
                let mut prefix = [0];
                let mut eof = [7];
                let into = fs
                    .vread_into_impl(&reads, &mut [&mut prefix, &mut eof])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })?;
                assert_eq!(prefix, [b'x']);
                assert_eq!(eof, [7]);
                assert_eq!(into[1].offset, 3);
                assert_eq!(into[1].bytes_read, 0);
                assert!(into[1].eof);
            }
            Ok(())
        })();
        let close = fs.close_impl(&fd);
        let cleanup = {
            let remove_roots: &[&std::path::Path] = &[root.as_path()];
            vfsi_core::internal::collect_vec_result(
                fs.remove_paths_impl(remove_roots, true),
                remove_roots.len(),
            )
            .map(|_| ())
        };
        operation.unwrap();
        close.unwrap();
        cleanup.unwrap();
    }
}

#[test]
fn file_review_mixed_descriptor_end_reads_work_like_descriptor_only_reads() {
    file_review_mixed_descriptor_end_offsets(false);
}

#[test]
fn file_review_mixed_descriptor_end_writes_work_like_descriptor_only_writes() {
    file_review_mixed_descriptor_end_offsets(true);
}

#[test]
fn client_extent_copy_rejects_forward_aliases_without_mutating_source() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/vfsi-client-copy-alias-{}", std::process::id());
    fs.ensure_dir_impl(Path::new(&root), 0o700).unwrap();
    let source = Path::new(&root).join("source");
    let alias = Path::new(&root).join("alias");
    let payload: Vec<u8> = (0..(2 * 1024 * 1024 + 17))
        .map(|index| (index % 251) as u8)
        .collect();
    let mut outcomes = Vec::new();
    #[cfg(feature = "test-faults")]
    let mut close_counts = Vec::new();
    for hardlink in [false, true] {
        fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_os_path(
            &source,
            VfOffset::At(0),
            payload.clone(),
        )
        .with_creation()
        .with_truncate()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        if hardlink {
            fs.vhardlink_impl(&[&source], &[&alias])
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
                })
                .unwrap();
        }
        let destination = if hardlink { &alias } else { &source };
        for length in [Some(payload.len() as u64), None] {
            #[cfg(feature = "test-faults")]
            let before = fs.test_confirmed_path_closes();
            let result = {
                let copy_pairs = &[ExtentPair::from_os_paths(
                    &source,
                    0,
                    destination,
                    1,
                    length,
                )];
                vfsi_core::internal::collect_vec_result(
                    fs.vcopy_data_impl(copy_pairs),
                    copy_pairs.len(),
                )
                .map(|_| ())
            };
            #[cfg(feature = "test-faults")]
            close_counts.push(fs.test_confirmed_path_closes() - before);
            let actual = fs
                .read_file_impl(&FileOperand::from_os_path(&source), payload.len() + 128)
                .unwrap();
            outcomes.push((hardlink, result, actual));
        }
    }
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    #[cfg(feature = "test-faults")]
    for closes in close_counts {
        assert_eq!(
            closes, 1,
            "rejected alias copies must still close current state"
        );
    }
    for (hardlink, result, actual) in outcomes {
        assert!(
            actual == payload,
            "copy changed its own source (hardlink={hardlink}, len={})",
            actual.len()
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[test]
fn client_extent_copy_bounds_forward_aliases_by_original_eof() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/vfsi-client-copy-eof-alias-{}", std::process::id());
    fs.ensure_dir_impl(Path::new(&root), 0o700).unwrap();
    let source = Path::new(&root).join("source");
    let alias = Path::new(&root).join("alias");
    let payload: Vec<u8> = (0..(2 * 1024 * 1024 + 17))
        .map(|index| (index % 251) as u8)
        .collect();
    let original_size = payload.len() as u64;
    #[cfg(feature = "test-faults")]
    let mut close_counts = Vec::new();
    let outcomes: VfResult<Vec<_>> = (|| {
        let mut outcomes = Vec::new();
        for hardlink in [false, true] {
            if hardlink {
                fs.vhardlink_impl(&[&source], &[&alias])
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
                    })?;
            }
            for (source_offset, destination_offset) in [
                (0, original_size + 1),
                (0, original_size + 128),
                (17, original_size + 1),
            ] {
                fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_os_path(
                    &source,
                    VfOffset::At(0),
                    payload.clone(),
                )
                .with_creation()
                .with_truncate()]))
                    .and_then(|outcomes| {
                        let count = outcomes.len();
                        vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                    })?;
                let destination = if hardlink { &alias } else { &source };
                let requested = original_size + 100;
                #[cfg(feature = "test-faults")]
                let before = fs.test_confirmed_path_closes();
                {
                    let copy_pairs = &[ExtentPair::from_os_paths(
                        &source,
                        source_offset,
                        destination,
                        destination_offset,
                        Some(requested),
                    )];
                    vfsi_core::internal::collect_vec_result(
                        fs.vcopy_data_impl(copy_pairs),
                        copy_pairs.len(),
                    )
                    .map(|_| ())
                }?;
                #[cfg(feature = "test-faults")]
                close_counts.push(fs.test_confirmed_path_closes() - before);
                // Leave room for a buggy copy to read bytes appended by itself.
                let actual = fs.read_file_impl(
                    &FileOperand::from_os_path(&source),
                    (destination_offset + requested + 128) as usize,
                )?;
                let mut expected = payload.clone();
                expected.resize(destination_offset as usize, 0);
                expected.extend_from_slice(&payload[source_offset as usize..]);
                outcomes.push((
                    hardlink,
                    source_offset,
                    destination_offset,
                    actual,
                    expected,
                ));
            }
        }
        Ok(outcomes)
    })();
    let cleanup = {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    };
    let outcomes = outcomes.unwrap();
    cleanup.unwrap();
    #[cfg(feature = "test-faults")]
    for closes in close_counts {
        assert_eq!(closes, 1, "one latest CLOSE for one shared path OPEN state");
    }
    for (hardlink, source_offset, destination_offset, actual, expected) in outcomes {
        assert_eq!(
            actual.len(),
            expected.len(),
            "copy read past original EOF (hardlink={hardlink}, src={source_offset}, dst={destination_offset})"
        );
        assert!(
            actual == expected,
            "copy changed bytes (hardlink={hardlink}, src={source_offset}, dst={destination_offset})"
        );
    }
}

#[test]
fn client_extent_copy_reuses_current_state_for_safe_aliases() {
    let Some(mut fs) = connect() else { return };
    let root = format!("/vfsi-client-copy-safe-alias-{}", std::process::id());
    fs.ensure_dir_impl(Path::new(&root), 0o700).unwrap();
    let source = Path::new(&root).join("source");
    let alias = Path::new(&root).join("alias");
    let mut outcomes = Vec::new();
    #[cfg(feature = "test-faults")]
    let mut close_counts = Vec::new();
    for hardlink in [false, true] {
        if hardlink {
            fs.vhardlink_impl(&[&source], &[&alias])
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
                })
                .unwrap();
        }
        for (offset, destination_offset, length, expected) in [
            (4, 0, Some(3), b"456".as_slice()),
            (0, 10, Some(5), b"012345678901234".as_slice()),
            (4, 0, None, b"456789".as_slice()),
            (0, 0, Some(3), b"012".as_slice()),
            (0, 10, Some(20), b"01234567890123456789".as_slice()),
            (0, 1, Some(0), b"0".as_slice()),
        ] {
            fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_os_path(
                &source,
                VfOffset::At(0),
                b"0123456789".to_vec(),
            )
            .with_creation()
            .with_truncate()]))
                .and_then(|outcomes| {
                    let count = outcomes.len();
                    vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
                })
                .unwrap();
            let destination = if hardlink { &alias } else { &source };
            #[cfg(feature = "test-faults")]
            let before = fs.test_confirmed_path_closes();
            let result = {
                let copy_pairs = &[ExtentPair::from_os_paths(
                    &source,
                    offset,
                    destination,
                    destination_offset,
                    length,
                )];
                vfsi_core::internal::collect_vec_result(
                    fs.vcopy_data_impl(copy_pairs),
                    copy_pairs.len(),
                )
                .map(|_| ())
            };
            #[cfg(feature = "test-faults")]
            let closes = fs.test_confirmed_path_closes() - before;
            let actual = fs
                .read_file_impl(&FileOperand::from_os_path(&source), 64)
                .unwrap();
            outcomes.push((hardlink, result, actual, expected));
            #[cfg(feature = "test-faults")]
            close_counts.push(closes);
        }
    }
    {
        let remove_roots: &[&std::path::Path] = &[Path::new(&root)];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    #[cfg(feature = "test-faults")]
    for closes in close_counts {
        assert_eq!(closes, 1, "one latest CLOSE for one shared path OPEN state");
    }
    for (hardlink, result, actual, expected) in outcomes {
        result.unwrap_or_else(|error| {
            panic!("safe alias copy failed (hardlink={hardlink}): {error}")
        });
        assert_eq!(actual, expected);
    }
}

#[test]
fn recursive_copy_allows_ancestor_destinations_with_both_copy_strategies() {
    let Some(mut backend) = connect() else { return };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::path::PathBuf::from(format!(
        "/vfsi-copy-ancestor-{}-{nonce}",
        std::process::id()
    ));
    let source = root.join("parent/source");
    backend
        .ensure_dir_impl(&source.join("empty"), 0o755)
        .unwrap();
    let result = (|| {
        let file = backend.open_raw_impl(
            &source.join("file"),
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
            0o640,
        )?;
        let write = backend.write_raw_impl(&file, 0, b"payload");
        let close = backend.close_impl(&file);
        write?;
        close?;
        for server_side in [false, true] {
            for destination in [root.join("parent"), root.clone()] {
                backend.copy_tree_impl(&source, &destination, false, server_side)?;
                assert_eq!(
                    backend.read_raw_impl(
                        &FileOperand::from_os_path(&destination.join("file")),
                        0,
                        7
                    )?,
                    b"payload",
                );
                assert_eq!(
                    backend.stat_impl(&destination.join("empty"))?.ftype,
                    VfType::Directory
                );
                assert_eq!(
                    backend.read_raw_impl(
                        &FileOperand::from_os_path(&source.join("file")),
                        0,
                        7
                    )?,
                    b"payload",
                );
            }
        }
        Ok::<_, vfsi_sync::VfError>(())
    })();
    let cleanup = {
        let remove_roots: &[&std::path::Path] = &[root.as_path()];
        vfsi_core::internal::collect_vec_result(
            backend.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    };
    result.unwrap();
    cleanup.unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn whole_file_vectors_keep_repair_prefixes_and_completed_siblings() {
    let Some(mut fs) = connect() else { return };
    fs.set_auto_reconnect(false);
    let paths: Vec<_> = (0..3)
        .map(|index| format!("/vfsi-whole-prefix-{}-{index}", std::process::id()))
        .collect();
    let bytes = [b"abcdefgh", b"ijklmnop", b"qrstuvwx"];
    let writes: Vec<_> = paths
        .iter()
        .zip(bytes)
        .map(|(path, data)| {
            vfsi_sync::WriteOp::from_path(path, VfOffset::At(0), data.to_vec())
                .with_creation()
                .with_truncate()
        })
        .collect();
    fs.vwrite_impl(&borrow_writes(&writes))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    let files = fs
        .vopen_raw_simple_impl(
            &paths.iter().map(Path::new).collect::<Vec<_>>(),
            libc::O_RDONLY,
            0,
        )
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeReadRepair { index: 1 },
        VfError::errno(None, libc::EIO as u32),
    ));
    fs.set_fault_injector(script.clone());
    fs.test_short_read_for_request_once(1, 2);
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let outcomes = fs.vread_all_impl(&files).unwrap();
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    assert!(script.is_consumed());
    assert_eq!(outcomes[0].as_ref().unwrap(), bytes[0]);
    assert_eq!(outcomes[2].as_ref().unwrap(), bytes[2]);
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Failed);
    assert_eq!(error.cause().raw_code(), libc::EIO as u32);
    let Some(vfsi_core::IoProgress::Read(prefix)) = error.progress() else {
        panic!("confirmed whole-file prefix missing");
    };
    assert_eq!(
        (prefix.offset(), prefix.data().unwrap()),
        (0, b"ij".as_slice())
    );
    assert!(
        compounds <= 2,
        "completed siblings must not be replayed: {compounds}"
    );
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    {
        let remove_files = &paths
            .iter()
            .map(|path| FileOperand::from_path(path))
            .collect::<Vec<_>>();
        vfsi_core::internal::collect_vec_result(fs.vremove_impl(remove_files), remove_files.len())
            .map(|_| ())
    }
    .unwrap();
}

#[cfg(feature = "test-faults")]
fn raw_open_outcome_fixture(label: &str) -> Option<(NfsClient, vfsi_nfs::client::NfsFh, String)> {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert!(!required(), "VFSI_NFS_SERVER is required");
        return None;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut client = NfsClient::connect_minor(&server, minor).unwrap();
    let root = client.root().clone();
    let name = format!("vfsi-open-outcomes-{label}-{}", std::process::id());
    let directory = client.mkdir(&root, &name).unwrap();
    Some((client, directory, name))
}

#[cfg(feature = "test-faults")]
#[test]
fn raw_open_outcomes_keep_successful_owners_around_a_failed_input_without_replay() {
    use vfsi_core::FailureState;
    let Some((mut client, directory, name)) = raw_open_outcome_fixture("siblings") else {
        return;
    };
    let requests: Vec<_> = ["first", "/", "missing", "last"]
        .iter()
        .enumerate()
        .map(|(index, leaf)| PathOpenOp {
            path: if *leaf == "/" {
                b"/".to_vec()
            } else {
                format!("{name}/{leaf}").into_bytes()
            },
            access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            create: if index == 2 {
                OpenCreate::NoCreate
            } else {
                OpenCreate::Unchecked
            },
            mode: Some(0o600),
            truncate: false,
        })
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let outcomes = client.openv_path_compound(&requests).unwrap();
    assert_eq!(vfsi_nfs::compound::thread_compound_stats().0, 2);
    assert_eq!(outcomes.len(), 4);
    let mut successful = 0;
    for (index, outcome) in outcomes.into_iter().enumerate() {
        if index == 1 {
            let error = outcome.unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert_eq!(error.cause().index(), None);
        } else if index == 2 {
            let error = outcome.unwrap_err();
            assert_eq!(error.state(), FailureState::Failed);
            assert_eq!(error.cause().index(), None);
            assert_eq!(
                error.cause().status(),
                Some(vfsi_core::StatusCode::Nfs(
                    nfsv41_sys::nfsstat4_NFS4ERR_NOENT
                ))
            );
        } else {
            let (fh, stateid) = outcome.unwrap();
            assert_eq!(client.write(&fh, &stateid, 0, b"still open").unwrap().0, 10);
            assert_eq!(client.read(&fh, &stateid, 0, 10).unwrap().0, b"still open");
            client.close(&fh, &stateid).unwrap();
            successful += 1;
        }
    }
    assert_eq!(successful, 2);
    for leaf in ["first", "last"] {
        client.remove(&directory, leaf).unwrap();
    }
    let root = client.root().clone();
    client.remove(&root, &name).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn raw_open_outcomes_preserve_earlier_handles_and_exact_unsent_or_uncertain_chunks() {
    use vfsi_core::FailureState;
    for lost_reply in [false, true] {
        let Some((mut client, directory, name)) =
            raw_open_outcome_fixture(if lost_reply { "lost" } else { "unsent" })
        else {
            return;
        };
        client.max_ops = 24;
        let requests: Vec<_> = (0..16)
            .map(|index| PathOpenOp {
                path: format!("{name}/file-{index}").into_bytes(),
                access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                create: OpenCreate::Exclusive,
                mode: Some(0o600),
                truncate: false,
            })
            .collect();
        let script = Arc::new(FaultScript::one(
            if lost_reply {
                OpenFaultPoint::AfterOpenChunkReply { chunk: 1 }
            } else {
                OpenFaultPoint::BeforeOpenChunk { chunk: 1 }
            },
            VfError::transport(None, "injected OPEN chunk failure"),
        ));
        client.set_fault_injector(script.clone());
        let outcomes = client.openv_path_compound(&requests).unwrap();
        assert_eq!(outcomes.len(), requests.len());
        assert!(script.is_consumed());
        let mut successes = 0;
        let mut unknown = 0;
        let mut unsent = 0;
        let mut seen_error = false;
        for (index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                Ok((fh, stateid)) => {
                    assert!(
                        !seen_error,
                        "earlier confirmed owners must be retained in input order"
                    );
                    client.write(&fh, &stateid, 0, b"kept").unwrap();
                    assert_eq!(client.read(&fh, &stateid, 0, 4).unwrap().0, b"kept");
                    client.close(&fh, &stateid).unwrap();
                    successes += 1;
                }
                Err(error) => {
                    seen_error = true;
                    assert_eq!(error.cause().index(), None);
                    assert!(error.progress().is_none());
                    match error.state() {
                        FailureState::Unknown => {
                            assert!(lost_reply);
                            assert_eq!(unsent, 0, "uncertain chunk precedes untouched inputs");
                            unknown += 1;
                        }
                        FailureState::NotAttempted => unsent += 1,
                        state => panic!("unexpected {state:?}"),
                    }
                    let exists = client
                        .lookup(&directory, format!("file-{index}").as_bytes())
                        .is_ok();
                    assert_eq!(exists, error.state() == FailureState::Unknown);
                }
            }
        }
        assert!(successes > 0 && unsent > 0);
        assert_eq!(unknown > 0, lost_reply);
        assert_eq!(client.deferred_path_close_count(), 0);
        client.set_max_compound_bytes(0);
        for index in 0..successes + unknown {
            client.remove(&directory, &format!("file-{index}")).unwrap();
        }
        let root = client.root().clone();
        client.remove(&root, &name).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn raw_open_finalization_failure_retains_siblings_and_defers_only_its_own_cleanup() {
    use vfsi_core::FailureState;
    let Some((mut client, directory, name)) = raw_open_outcome_fixture("finalize") else {
        return;
    };
    let requests: Vec<_> = (0..3)
        .map(|index| PathOpenOp {
            path: format!("{name}/file-{index}").into_bytes(),
            access: nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            create: OpenCreate::Exclusive,
            mode: Some(0o600),
            truncate: false,
        })
        .collect();
    let script = Arc::new(FaultScript::new([
        (
            OpenFaultPoint::BeforeOpenFinalize { index: 1 },
            VfError::errno(None, libc::EACCES as u32),
        ),
        (
            OpenFaultPoint::BeforePathCloseBatch,
            VfError::transport(None, "cleanup unavailable"),
        ),
    ]));
    client.set_fault_injector(script.clone());
    let outcomes = client.openv_path_compound(&requests).unwrap();
    assert!(script.is_consumed());
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(error.cause().raw_code(), libc::EACCES as u32);
    assert_eq!(
        client.deferred_path_close_count(),
        1,
        "only failed input transfers its cleanup owner"
    );
    assert!(
        client.lookup(&directory, b"file-1").is_ok(),
        "no rollback of confirmed creation"
    );

    // Empty vectors do no I/O, including deferred cleanup.
    let _ = vfsi_nfs::compound::thread_compound_stats();
    assert!(client.openv_path_compound(&[]).unwrap().is_empty());
    assert_eq!(vfsi_nfs::compound::thread_compound_stats().0, 0);
    assert_eq!(client.deferred_path_close_count(), 1);
    // The next real operation drains that exact deferred state, leaving
    // successful siblings open and independently usable.
    let work = PathOpenOp {
        path: requests[1].path.clone(),
        access: nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
        create: OpenCreate::NoCreate,
        mode: None,
        truncate: false,
    };
    let mut work = client.openv_path_compound(&[work]).unwrap();
    assert_eq!(client.deferred_path_close_count(), 0);
    let (fh, stateid) = work.pop().unwrap().unwrap();
    client.close(&fh, &stateid).unwrap();
    for (index, outcome) in outcomes.into_iter().enumerate() {
        if index == 1 {
            continue;
        }
        let (fh, stateid) = outcome.unwrap();
        client.write(&fh, &stateid, 0, b"live").unwrap();
        assert_eq!(client.read(&fh, &stateid, 0, 4).unwrap().0, b"live");
        client.close(&fh, &stateid).unwrap();
    }
    for index in 0..3 {
        client.remove(&directory, &format!("file-{index}")).unwrap();
    }
    let root = client.root().clone();
    client.remove(&root, &name).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn native_open_outcomes_keep_registered_siblings_and_deferred_failed_cleanup() {
    let Some(mut fs) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-native-open-owners-{}", std::process::id()));
    fs.mkdir_raw_impl(&root, 0o755).unwrap();
    let paths: Vec<_> = (0..3).map(|index| root.join(format!("f{index}"))).collect();
    let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let script = Arc::new(FaultScript::new([
        (
            OpenFaultPoint::AfterRegister { index: 1 },
            VfError::errno(None, libc::EIO as u32),
        ),
        (
            OpenFaultPoint::BeforeCloseDispatch { index: 0 },
            VfError::transport(None, "cleanup unavailable"),
        ),
    ]));
    fs.set_fault_injector(script.clone());
    let outcomes = fs
        .vopen_raw_impl(
            &refs,
            &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 3],
            &[0o600; 3],
        )
        .unwrap();
    assert!(script.is_consumed());
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(fs.test_open_handle_count(), 2);
    assert_eq!(fs.test_deferred_descriptor_close_count(), 1);
    let files: Vec<_> = outcomes.into_iter().filter_map(Result::ok).collect();
    for file in &files {
        fs.write_raw_impl(file, 0, b"live").unwrap();
        assert_eq!(fs.read_raw_impl(file, 0, 4).unwrap(), b"live");
    }
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert_eq!(fs.test_open_handle_count(), 0);
    let next = fs
        .vopen_raw_impl(&refs[..1], &[libc::O_RDONLY], &[0])
        .unwrap();
    assert_eq!(fs.test_deferred_descriptor_close_count(), 0);
    assert!(
        fs.vclose_impl(&[next.into_iter().next().unwrap().unwrap()])
            .unwrap()[0]
            .is_ok()
    );
    {
        let remove_roots: &[&std::path::Path] = &[&root];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
}

#[test]
fn native_open_without_create_truncates_only_requested_targets() {
    let Some(mut fs) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-native-open-truncate-{}", std::process::id()));
    fs.mkdir_raw_impl(&root, 0o755).unwrap();
    let first = root.join("first");
    let second = root.join("second");
    for path in [&first, &second] {
        let file = fs
            .open_raw_impl(path, libc::O_CREAT | libc::O_RDWR, 0o640)
            .unwrap();
        fs.write_raw_impl(&file, 0, b"existing").unwrap();
        fs.close_impl(&file).unwrap();
    }
    let outcomes = fs
        .vopen_raw_impl(
            &[&first, &second],
            &[libc::O_WRONLY | libc::O_TRUNC, libc::O_RDONLY],
            &[0; 2],
        )
        .unwrap();
    assert!(outcomes.iter().all(Result::is_ok));
    assert_eq!(
        fs.stat_impl(&first).unwrap().size,
        0,
        "O_TRUNC applies without O_CREAT"
    );
    assert_eq!(
        fs.stat_impl(&second).unwrap().size,
        8,
        "read-only sibling is untouched"
    );
    assert_eq!(
        fs.stat_impl(&first).unwrap().mode & 0o7777,
        0o640,
        "opening an existing file does not change its mode"
    );
    let files: Vec<_> = outcomes.into_iter().map(Result::unwrap).collect();
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    {
        let remove_roots: &[&std::path::Path] = &[&root];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
}

#[test]
fn native_open_symlink_repairs_preserve_healthy_exclusive_siblings() {
    let Some(mut fs) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-native-open-symlink-{}", std::process::id()));
    fs.mkdir_raw_impl(&root, 0o755).unwrap();
    let target = root.join("target");
    let link = root.join("link");
    let other = root.join("other");
    let last = root.join("last");
    let seed = fs
        .open_raw_impl(&target, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    fs.write_raw_impl(&seed, 0, b"existing").unwrap();
    fs.close_impl(&seed).unwrap();
    fs.symlink_raw_impl(Path::new("target"), &link).unwrap();
    let outcomes = fs
        .vopen_raw_impl(
            &[&link, &other],
            &[
                libc::O_WRONLY | libc::O_TRUNC,
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            ],
            &[0, 0o600],
        )
        .unwrap();
    assert!(
        outcomes.iter().all(Result::is_ok),
        "repair must not replay the healthy exclusive create"
    );
    assert_eq!(fs.stat_impl(&target).unwrap().size, 0);
    let files: Vec<_> = outcomes.into_iter().map(Result::unwrap).collect();
    fs.write_raw_impl(&files[0], 0, b"replacement").unwrap();
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    let outcomes = fs
        .vopen_raw_impl(
            &[&link, &last],
            &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 2],
            &[0o600; 2],
        )
        .unwrap();
    let mut outcomes = outcomes.into_iter();
    let error = outcomes.next().unwrap().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Failed);
    assert_eq!(
        error.cause().status(),
        Some(vfsi_core::StatusCode::Nfs(
            nfsv41_sys::nfsstat4_NFS4ERR_EXIST
        ))
    );
    let last = outcomes.next().unwrap().unwrap();
    assert_eq!(
        fs.stat_impl(&target).unwrap().size,
        b"replacement".len() as u64,
        "exclusive symlink rejection does not touch its target"
    );
    assert!(fs.vclose_impl(&[last]).unwrap()[0].is_ok());
    {
        let remove_roots: &[&std::path::Path] = &[&root];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn raw_write_outcomes_preserve_successes_around_a_wire_failure_without_replay() {
    use vfsi_core::FailureState;
    let Some((mut client, directory, name)) = raw_open_outcome_fixture("write-status") else {
        return;
    };
    let mut files = Vec::new();
    for leaf in ["first", "last"] {
        files.push(
            client
                .open(
                    &directory,
                    leaf.as_bytes(),
                    nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                    OpenCreate::Unchecked,
                )
                .unwrap(),
        );
    }
    let writes = [
        WriteOp {
            fh: files[0].0.clone(),
            stateid: files[0].1,
            offset: 0,
            data: b"first",
        },
        // A directory cannot accept WRITE, regardless of its stateid.
        WriteOp {
            fh: directory.clone(),
            stateid: files[0].1,
            offset: 0,
            data: b"bad",
        },
        WriteOp {
            fh: files[1].0.clone(),
            stateid: files[1].1,
            offset: 0,
            data: b"last",
        },
    ];
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let results = client.writev(&writes).unwrap();
    let stats = vfsi_nfs::compound::thread_compound_stats();
    assert_eq!(
        (stats.0, stats.1),
        (2, 10),
        "only the untouched final pair may resume"
    );
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], Ok((5, nfsv41_sys::stable_how4_FILE_SYNC4)));
    assert_eq!(results[2], Ok((4, nfsv41_sys::stable_how4_FILE_SYNC4)));
    let error = results[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert!(matches!(
        error.cause().status(),
        Some(vfsi_core::StatusCode::Nfs(_))
    ));
    assert_eq!(error.cause().index(), None);
    assert!(error.progress().is_none());
    for ((fh, stateid), (leaf, expected)) in files
        .iter()
        .zip([("first", b"first".as_slice()), ("last", b"last".as_slice())])
    {
        assert_eq!(client.read(fh, stateid, 0, 16).unwrap().0, expected);
        client.close(fh, stateid).unwrap();
        client.remove(&directory, leaf).unwrap();
    }
    let root = client.root().clone();
    client.remove(&root, &name).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn raw_write_compound_faults_preserve_confirmed_submitted_and_untouched_inputs() {
    use vfsi_core::FailureState;
    for first in [0, 3] {
        for lost_reply in [false, true] {
            let label = format!("write-fault-{first}-{lost_reply}");
            let Some((mut client, directory, name)) = raw_open_outcome_fixture(&label) else {
                return;
            };
            client.test_limit_compound_operations(8);
            let mut files = Vec::new();
            for index in 0..8 {
                let (fh, stateid) = client
                    .open(
                        &directory,
                        format!("file-{index}").as_bytes(),
                        nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
                        OpenCreate::Unchecked,
                    )
                    .unwrap();
                client.write(&fh, &stateid, 0, b"old").unwrap();
                files.push((fh, stateid));
            }
            let point = if lost_reply {
                OpenFaultPoint::AfterWriteCompoundReply { first }
            } else {
                OpenFaultPoint::BeforeWriteCompound { first }
            };
            let script = Arc::new(FaultScript::one(
                point,
                VfError::transport(None, "injected WRITE reply loss"),
            ));
            client.set_fault_injector(script.clone());
            let writes: Vec<_> = files
                .iter()
                .map(|(fh, stateid)| WriteOp {
                    fh: fh.clone(),
                    stateid: *stateid,
                    offset: 0,
                    data: b"new",
                })
                .collect();
            let _ = vfsi_nfs::compound::thread_compound_stats();
            assert!(client.writev(&[]).unwrap().is_empty());
            let result = client.writev(&writes);
            let stats = vfsi_nfs::compound::thread_compound_stats();
            assert!(script.is_consumed());
            let sent_compounds = first as u64 / 3 + u64::from(lost_reply);
            assert_eq!((stats.0, stats.1), (sent_compounds, sent_compounds * 7));
            if first == 0 && !lost_reply {
                assert!(result.unwrap_err().is_transport());
            } else {
                let results = result.unwrap();
                assert_eq!(results.len(), writes.len());
                for (index, result) in results.into_iter().enumerate() {
                    if index < first {
                        assert_eq!(result, Ok((3, nfsv41_sys::stable_how4_FILE_SYNC4)));
                    } else {
                        let error = result.unwrap_err();
                        assert_eq!(
                            error.state(),
                            if lost_reply && index < first + 3 {
                                FailureState::Unknown
                            } else {
                                FailureState::NotAttempted
                            }
                        );
                        assert!(error.progress().is_none());
                        assert_eq!(error.cause().index(), None);
                    }
                }
            }
            for (index, (fh, stateid)) in files.iter().enumerate() {
                let expected = if index < first + if lost_reply { 3 } else { 0 } {
                    b"new"
                } else {
                    b"old"
                };
                assert_eq!(&client.read(fh, stateid, 0, 3).unwrap().0, expected);
                client.close(fh, stateid).unwrap();
                client.remove(&directory, &format!("file-{index}")).unwrap();
            }
            let root = client.root().clone();
            client.remove(&root, &name).unwrap();
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn raw_write_preflight_rejects_only_invalid_inputs_and_batches_healthy_siblings() {
    let Some((mut client, directory, name)) = raw_open_outcome_fixture("write-preflight") else {
        return;
    };
    let (fh, stateid) = client
        .open(
            &directory,
            b"file",
            nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            OpenCreate::Unchecked,
        )
        .unwrap();
    client.set_max_compound_bytes(4096);
    let oversized = vec![b'b'; 8192];
    let writes = [
        WriteOp {
            fh: fh.clone(),
            stateid,
            offset: 0,
            data: b"abc",
        },
        WriteOp {
            fh: fh.clone(),
            stateid,
            offset: 4,
            data: &oversized,
        },
        WriteOp {
            fh: fh.clone(),
            stateid,
            offset: 8,
            data: b"xyz",
        },
        WriteOp {
            fh: fh.clone(),
            stateid,
            offset: u64::MAX,
            data: b"bad",
        },
    ];
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let results = client.writev(&writes).unwrap();
    let stats = vfsi_nfs::compound::thread_compound_stats();
    assert_eq!(
        (stats.0, stats.1),
        (1, 5),
        "healthy siblings retain one compound"
    );
    assert_eq!(results.len(), writes.len());
    for index in [0, 2] {
        assert_eq!(results[index], Ok((3, nfsv41_sys::stable_how4_FILE_SYNC4)));
    }
    for (index, status) in [
        (
            1,
            vfsi_core::StatusCode::Nfs(nfsv41_sys::nfsstat4_NFS4ERR_REQ_TOO_BIG),
        ),
        (3, vfsi_core::StatusCode::Errno(libc::EOVERFLOW as u32)),
    ] {
        let error = results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
        assert_eq!(error.cause().status(), Some(status));
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    assert_eq!(
        client.read(&fh, &stateid, 0, 16).unwrap().0,
        [b'a', b'b', b'c', 0, 0, 0, 0, 0, b'x', b'y', b'z',]
    );
    client.set_max_compound_bytes(0);
    client.close(&fh, &stateid).unwrap();
    client.remove(&directory, "file").unwrap();
    let root = client.root().clone();
    client.remove(&root, &name).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn raw_write_resource_retry_setup_failure_keeps_per_input_results() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vfsi_core::internal::faults::FaultInjector;
    struct RejectResend(AtomicUsize);
    impl FaultInjector for RejectResend {
        fn check(&self, point: &OpenFaultPoint) -> vfsi_core::VfResult<()> {
            if matches!(point, OpenFaultPoint::BeforeWriteCompound { .. })
                && self.0.fetch_add(1, Ordering::Relaxed) == 1
            {
                return Err(VfError::transport(None, "resend setup failed"));
            }
            Ok(())
        }
    }
    let Some((mut client, directory, name)) = raw_open_outcome_fixture("write-resource") else {
        return;
    };
    let (fh, stateid) = client
        .open(
            &directory,
            b"file",
            nfsv41_sys::OPEN4_SHARE_ACCESS_BOTH,
            OpenCreate::Unchecked,
        )
        .unwrap();
    let writes: Vec<_> = (0..3)
        .map(|index| WriteOp {
            fh: fh.clone(),
            stateid,
            offset: index,
            data: b"x",
        })
        .collect();
    let fault = Arc::new(RejectResend(AtomicUsize::new(0)));
    client.set_fault_injector(fault.clone());
    client.inject_resource_rejection_once(b"writev");
    let results = client
        .writev(&writes)
        .expect("an accepted/rejected compound cannot become outer rejection");
    assert_eq!(results.len(), writes.len());
    assert_eq!(fault.0.load(Ordering::Relaxed), 2);
    assert!(!client.resource_rejection_pending());
    for result in results {
        let error = result.unwrap_err();
        assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    assert!(client.read(&fh, &stateid, 0, 8).unwrap().0.is_empty());
    client.close(&fh, &stateid).unwrap();
    client.remove(&directory, "file").unwrap();
    let root = client.root().clone();
    client.remove(&root, &name).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn merged_write_outcomes_keep_partial_prefix_and_later_success() {
    use vfsi_core::{FailureState, IoProgress};
    use vfsi_nfs::client::CloseOp;
    for repair_failure in [false, true] {
        for close in [false, true] {
            let Some((mut client, directory, name)) =
                raw_open_outcome_fixture(&format!("merged-write-{repair_failure}-{close}"))
            else {
                return;
            };
            client.set_max_compound_bytes(64 * 1024);
            let per = client.per_op_bytes();
            let data = vec![b'a'; if repair_failure { 32 } else { per * 2 + 17 }];
            let script = if repair_failure {
                client.test_short_merged_write_once(7);
                let script = Arc::new(FaultScript::one(
                    OpenFaultPoint::BeforeWriteRepair { index: 0 },
                    VfError::errno(None, libc::EACCES as u32),
                ));
                client.set_fault_injector(script.clone());
                Some(script)
            } else {
                client.test_fail_merged_write_once(0, 1);
                None
            };
            let _ = vfsi_nfs::compound::thread_compound_stats();
            let outcome = client
                .writev_path_compound(
                    &[
                        PathWriteOp {
                            file: FileRef::Path(format!("{name}/first").into_bytes()),
                            offset: 0,
                            data: &data,
                            create: true,
                            truncate: true,
                            stateid: None,
                        },
                        PathWriteOp {
                            file: FileRef::Path(format!("{name}/last").into_bytes()),
                            offset: 0,
                            data: b"last",
                            create: true,
                            truncate: true,
                            stateid: None,
                        },
                    ],
                    close,
                )
                .unwrap();
            let stats = vfsi_nfs::compound::thread_compound_stats();
            if let Some(script) = script {
                assert!(script.is_consumed());
            }
            assert_eq!(
                stats.0,
                if repair_failure { 1 } else { 3 },
                "only untouched work resumes; acknowledged data is never replayed"
            );
            assert_eq!(outcome.results.len(), 2);
            let confirmed = if repair_failure { 7 } else { per };
            let error = outcome.results[0].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::Failed);
            assert_eq!(error.cause().index(), None);
            assert_eq!(
                error.progress(),
                Some(&IoProgress::Write {
                    bytes_written: confirmed
                })
            );
            assert_eq!(
                outcome.results[1],
                Ok((4, nfsv41_sys::stable_how4_FILE_SYNC4))
            );
            let closes: Vec<_> = outcome
                .opened
                .iter()
                .enumerate()
                .filter(|(index, (fh, _))| {
                    !outcome.opened[index + 1..]
                        .iter()
                        .any(|(later, _)| later == fh)
                })
                .map(|(_, (fh, stateid))| CloseOp {
                    fh: fh.clone(),
                    stateid: *stateid,
                })
                .collect();
            client.close_many_path(&closes).unwrap();
            for (leaf, expected) in [
                ("first", data[..confirmed].to_vec()),
                ("last", b"last".to_vec()),
            ] {
                let (fh, stateid) = client
                    .open(
                        &directory,
                        leaf.as_bytes(),
                        nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
                        OpenCreate::NoCreate,
                    )
                    .unwrap();
                assert_eq!(
                    client
                        .read(&fh, &stateid, 0, (expected.len() + 1) as u32)
                        .unwrap()
                        .0,
                    expected
                );
                client.close(&fh, &stateid).unwrap();
                client.remove(&directory, leaf).unwrap();
            }
            let root = client.root().clone();
            client.remove(&root, &name).unwrap();
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn merged_write_outcomes_isolate_local_rejections_without_splitting_healthy_batch() {
    use vfsi_core::FailureState;
    use vfsi_nfs::client::CloseOp;
    let Some((mut client, directory, name)) = raw_open_outcome_fixture("merged-preflight") else {
        return;
    };
    let paths = ["first", "overflow", "last"].map(|leaf| format!("{name}/{leaf}").into_bytes());
    let make = |path: Vec<u8>, offset| PathWriteOp {
        file: FileRef::Path(path),
        offset,
        data: b"new",
        create: true,
        truncate: true,
        stateid: None,
    };
    let ops = [
        make(paths[0].clone(), 0),
        make(b"/".to_vec(), 0),
        make(paths[1].clone(), u64::MAX),
        make(paths[2].clone(), 0),
    ];
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let outcome = client.writev_path_compound(&ops, false).unwrap();
    assert_eq!(vfsi_nfs::compound::thread_compound_stats().0, 1);
    assert_eq!(outcome.results.len(), 4);
    for index in [0, 3] {
        assert_eq!(
            outcome.results[index],
            Ok((3, nfsv41_sys::stable_how4_FILE_SYNC4))
        );
    }
    for (index, errno) in [(1, libc::EISDIR), (2, libc::EOVERFLOW)] {
        let error = outcome.results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().to_errno(), errno);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    let closes: Vec<_> = outcome
        .opened
        .into_iter()
        .map(|(fh, stateid)| CloseOp { fh, stateid })
        .collect();
    client.close_many_path(&closes).unwrap();
    assert!(
        client.lookup(&directory, b"overflow").is_err(),
        "rejected create must not dispatch"
    );
    for leaf in ["first", "last"] {
        client.remove(&directory, leaf).unwrap();
    }
    let root = client.root().clone();
    client.remove(&root, &name).unwrap();
}
