//! Public-package integration coverage for the NFS backend.

use std::path::Path;
#[cfg(feature = "test-faults")]
use std::{path::PathBuf, sync::Arc};

use vfsi_core::RpcError;
#[cfg(feature = "test-faults")]
use vfsi_core::VfError;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_nfs::NfsVecFs;
#[cfg(feature = "test-faults")]
use vfsi_nfs::client::{FileRef, PathOpenOp, PathWriteOp, WriteOp};
use vfsi_nfs::client::{NfsClient, OpenCreate, ReadOp};
use vfsi_sync::{VecFs, test_support};

fn required() -> bool {
    std::env::var("VFSI_NFS_REQUIRED").as_deref() == Ok("1")
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
    fs.ensure_dir(Path::new(&root), 0o755).unwrap();
    // Enough entries for multiple 32-KiB READDIR pages. One-item REMOVE
    // batches fail only `blocked`, without fabricating a successful prefix.
    let paths: Vec<_> = std::iter::once(format!("{root}/blocked"))
        .chain((0..1500).map(|index| format!("{root}/file-{index:04}")))
        .collect();
    fs.writev(
        &paths
            .iter()
            .map(|path| {
                vfsi_sync::WriteOp::at(vfsi_sync::VfFile::from_path(path), 0, b"x".to_vec())
                    .with_creation()
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let failure = Arc::new(PersistentFailure {
        attempts: AtomicUsize::new(0),
        cookies: Mutex::new(Vec::new()),
    });
    fs.set_fault_injector(failure.clone());
    let error = fs
        .rm_with_options(
            &[Path::new(&root)],
            true,
            vfsi_sync::RemoveOptions::new()
                .batch(1)
                .continue_on_error(true),
        )
        .unwrap_err();
    let attempts = failure.attempts.load(Ordering::Relaxed);
    let cookies = failure.cookies.lock().unwrap().clone();
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    // Clean up before assertions, even when the operation's behavior regresses.
    let remaining: Vec<_> = fs
        .listdir(Path::new(&root), vfsi_sync::AttrMask::empty(), 2, false)
        .unwrap()
        .into_iter()
        .map(|attrs| attrs.file.path().unwrap().to_path_buf())
        .collect();
    fs.rm(&[Path::new(&root)], true).unwrap();
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
        fs.ensure_dir(Path::new(&root), 0o755).unwrap();
        fs.writev(&[
            vfsi_sync::WriteOp::at(vfsi_sync::VfFile::from_path(&path), 0, b"x".to_vec())
                .with_creation(),
        ])
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
        let result = fs.rm_with_options(
            &[Path::new(&root)],
            true,
            vfsi_sync::RemoveOptions::new().batch(1).retries(retries),
        );
        let exists = fs.exists(Path::new(&path)).unwrap();
        fs.set_fault_injector(Arc::new(FaultScript::new([])));
        if exists {
            fs.rm(&[Path::new(&root)], true).unwrap();
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
    let mut file = fs.create(&path).unwrap();
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
        fs.write_allv(&requests).unwrap()
    } else {
        fs.writev(&requests).unwrap()
    };
    let compounds = vfsi_nfs::compound::thread_compound_stats().0;
    let cursor = file.seek_native(std::io::SeekFrom::Current(0)).unwrap();
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
    assert!(results.iter().all(|result| result.written == 32));
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
            let files = fs.openv_simple(&[Path::new(&path)], flags, 0o600).unwrap();
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
                .writev(&[
                    vfsi_sync::WriteOp::new(files[0].clone(), first, b"abcd".to_vec()),
                    vfsi_sync::WriteOp::new(files[0].clone(), second, b"XY".to_vec()),
                ])
                .unwrap();
            let compounds = vfsi_nfs::compound::thread_compound_stats().0;
            let waves = script
                .visited()
                .iter()
                .filter(|point| matches!(point, OpenFaultPoint::AfterWriteChunk { .. }))
                .count();
            let cursor = fs.fseek(&files[0], 0, vfsi_sync::SeekFrom::Cur).unwrap();
            let contents = fs
                .readv(&[vfsi_sync::ReadOp::at(files[0].clone(), 0, 16)])
                .unwrap();
            fs.closev(&files).unwrap();
            fs.removev(&[vfsi_sync::VfFile::from_path(&path)]).unwrap();
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
                fs.writev(&[vfsi_sync::WriteOp::from_path(
                    path,
                    vfsi_sync::VfOffset::At(0),
                    data.to_vec(),
                )
                .with_creation()
                .with_truncate()])
                    .unwrap();
            }
            let files = fs
                .openv_simple(
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
                fs.readv_into(&reads, &mut [&mut first, &mut second])
                    .map(|result| {
                        (
                            vec![first.to_vec(), second.to_vec()],
                            result.iter().map(|item| item.offset).collect::<Vec<_>>(),
                        )
                    })
            } else {
                fs.readv(&reads).map(|result| {
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
                .map(|file| fs.fseek(file, 0, vfsi_sync::SeekFrom::Cur).unwrap())
                .collect();
            fs.closev(&files).unwrap();
            fs.removev(
                &paths
                    .iter()
                    .map(|path| vfsi_sync::VfFile::from_path(path))
                    .collect::<Vec<_>>(),
            )
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
                    vec![0, 0],
                    "failed vector changed cursors: into={into}"
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
            .openv_simple(
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
                VfError::client(0, libc::ENOSPC as u32)
            },
        ));
        fs.set_fault_injector(script.clone());
        let error = fs
            .writev(&[
                vfsi_sync::WriteOp::new(files[0].clone(), vfsi_sync::VfOffset::Cur, payload),
                vfsi_sync::WriteOp::at(files[1].clone(), 0, b"must not execute".to_vec()),
            ])
            .unwrap_err();
        let first_cursor = fs.fseek(&files[0], 0, vfsi_sync::SeekFrom::Cur).unwrap();
        let later = fs
            .readv(&[vfsi_sync::ReadOp::at(files[1].clone(), 0, 32)])
            .unwrap();
        fs.closev(&files).unwrap();
        fs.removev(
            &paths
                .iter()
                .map(|path| vfsi_sync::VfFile::from_path(path))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(script.is_consumed());
        if transport {
            assert!(error.is_transport());
            assert_eq!(error.index(), None);
        } else {
            assert_eq!(error.err_no(), libc::ENOSPC as u32);
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
        .openv_simple(&refs, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
        .unwrap();
    let script = Arc::new(FaultScript::new([]));
    fs.set_fault_injector(script.clone());
    let writes: Vec<_> = files
        .iter()
        .map(|file| vfsi_sync::WriteOp::at(file.clone(), 0, b"batched".to_vec()))
        .collect();
    let _ = vfsi_nfs::compound::thread_compound_stats();
    let results = fs.writev(&writes).unwrap();
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
    let contents = fs.readv(&reads).unwrap();
    fs.closev(&files).unwrap();
    fs.removev(
        &paths
            .iter()
            .map(|path| vfsi_sync::VfFile::from_path(path))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(waves, 1, "small independent writes must remain vectorized");
    assert_eq!(
        compounds, 1,
        "small independent writes must use one real compound"
    );
    assert!(results.iter().all(|result| result.written == 7));
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
        fs.writev(&[vfsi_sync::WriteOp::from_path(
            &path,
            vfsi_sync::VfOffset::At(0),
            payload.clone(),
        )
        .with_creation()
        .with_truncate()])
            .unwrap();
        let files = fs
            .openv_simple(&[Path::new(&path)], libc::O_RDONLY, 0)
            .unwrap();
        fs.test_short_read_once(7);
        let request = vfsi_sync::ReadOp::at(files[0].clone(), 0, size);
        let observed = if into {
            let mut buffer = vec![0; size];
            let result = fs.readv_into(&[request], &mut [&mut buffer]).unwrap();
            buffer.truncate(result[0].read);
            buffer
        } else {
            fs.readv(&[request]).unwrap().remove(0).data
        };
        fs.closev(&files).unwrap();
        fs.removev(&[vfsi_sync::VfFile::from_path(&path)]).unwrap();
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
        .write_allv(&[file.write_request_at(0, &payload)])
        .unwrap();
    assert_eq!(results[0].written, size);
    file.close().unwrap();
    let observed = fs.read(&path).unwrap();
    fs.remove_file(&path).unwrap();
    assert!(
        observed == payload,
        "successful write_allv must leave no unwritten gaps"
    );
}

#[test]
fn descriptor_positional_io_preserves_sequential_cursor() {
    let Some(backend) = connect() else { return };
    let fs = vfsi_sync::FsClient::new(backend);
    let path = format!("/vfsi-positional-cursor-{}", std::process::id());
    fs.write(&path, b"abcdefgh").unwrap();
    let mut file = fs
        .open_with(vfsi_sync::OpenRequest::new(
            &path,
            vfsi_sync::OpenFlags::READ | vfsi_sync::OpenFlags::WRITE,
        ))
        .unwrap();
    for kind in 0..5 {
        file.seek_native(std::io::SeekFrom::Start(0)).unwrap();
        let mut buffer = [0; 2];
        match kind {
            0 => {
                file.read_at(&mut buffer, 4).unwrap();
            }
            1 => {
                fs.readv(&[file.read_request_at(4, 2)]).unwrap();
            }
            2 => {
                fs.readv_into(&mut [file.read_request_at_into(4, &mut buffer)])
                    .unwrap();
            }
            3 => {
                file.write_at(b"XY", 4).unwrap();
            }
            _ => {
                fs.writev(&[file.write_request_at(4, b"XY")]).unwrap();
            }
        }
        let cursor = file.seek_native(std::io::SeekFrom::Current(0)).unwrap();
        assert_eq!(cursor, 0, "positional operation {kind} changed cursor");
        assert_eq!(file.read_native(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer, b"ab");
    }
    file.close().unwrap();
    fs.remove_file(&path).unwrap();
}

#[test]
fn reconnect_rejects_replacement_of_an_open_file() {
    let Some(mut fs) = connect() else { return };
    let path = format!("/vfsi-reconnect-identity-{}", std::process::id());
    let saved = format!("{path}-original");
    fs.writev(&[vfsi_sync::WriteOp::from_path(
        &path,
        vfsi_sync::VfOffset::At(0),
        b"original".to_vec(),
    )
    .with_creation()
    .with_truncate()])
        .unwrap();
    let files = fs
        .openv_simple(&[Path::new(&path)], libc::O_RDWR, 0)
        .unwrap();
    // Also exercise the successful unchanged-identity recovery path.
    fs.reconnect().unwrap();
    fs.renamev(&[(
        vfsi_sync::VfFile::from_path(&path),
        vfsi_sync::VfFile::from_path(&saved),
    )])
    .unwrap();
    fs.writev(&[vfsi_sync::WriteOp::from_path(
        &path,
        vfsi_sync::VfOffset::At(0),
        b"replaced".to_vec(),
    )
    .with_creation()
    .with_truncate()])
        .unwrap();
    let result = fs.reconnect();
    let old_contents = fs
        .readv(&[vfsi_sync::ReadOp::at(files[0].clone(), 0, 8)])
        .unwrap();
    fs.closev(&files).unwrap();
    fs.removev(&[
        vfsi_sync::VfFile::from_path(&path),
        vfsi_sync::VfFile::from_path(&saved),
    ])
    .unwrap();
    assert_eq!(result.unwrap_err().err_no(), libc::ESTALE as u32);
    assert_eq!(
        old_contents[0].data, b"original",
        "failed recovery must leave old handles intact"
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
            .openv_simple(
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
            .writev(&[
                vfsi_sync::WriteOp::new(files[0].clone(), offset, first.clone()),
                vfsi_sync::WriteOp::new(files[0].clone(), offset, second.clone()),
            ])
            .unwrap();
        assert_eq!(results[0].written, first.len());
        assert_eq!(results[1].written, second.len());
        assert_eq!(results[1].offset, if cursor { length as u64 } else { 0 });
        let position = fs.fseek(&files[0], 0, vfsi_sync::SeekFrom::Cur).unwrap();
        assert_eq!(
            position,
            if cursor {
                (length + second.len()) as i64
            } else {
                0
            }
        );
        fs.closev(&files).unwrap();
        let observed = fs
            .read_allv(&[vfsi_sync::VfFile::from_path(&path)])
            .unwrap();
        let expected = if cursor {
            [first, second].concat()
        } else {
            [second.clone(), first[second.len()..].to_vec()].concat()
        };
        fs.removev(&[vfsi_sync::VfFile::from_path(&path)]).unwrap();
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
            fs.writev(&[vfsi_sync::WriteOp::from_path(
                &path,
                vfsi_sync::VfOffset::At(0),
                b"contents".to_vec(),
            )
            .with_creation()
            .with_truncate()])
                .unwrap();
            let files = fs
                .openv_simple(&[Path::new(&path)], libc::O_RDWR, 0)
                .unwrap();
            let result = if read {
                fs.test_short_read_once(0);
                let request = vfsi_sync::ReadOp::new(files[0].clone(), vfsi_sync::VfOffset::Cur, 4);
                if into {
                    fs.readv_into(&[request], &mut [&mut [0; 4]]).map(|_| ())
                } else {
                    fs.readv(&[request]).map(|_| ())
                }
            } else {
                fs.test_short_write_once(0);
                fs.writev(&[vfsi_sync::WriteOp::new(
                    files[0].clone(),
                    vfsi_sync::VfOffset::Cur,
                    b"new".to_vec(),
                )])
                .map(|_| ())
            };
            assert_eq!(result.unwrap_err().err_no(), libc::EIO as u32);
            assert_eq!(fs.fseek(&files[0], 0, vfsi_sync::SeekFrom::Cur).unwrap(), 0);
            fs.closev(&files).unwrap();
            assert_eq!(
                fs.read_allv(&[vfsi_sync::VfFile::from_path(&path)])
                    .unwrap()[0],
                b"contents"
            );
            fs.removev(&[vfsi_sync::VfFile::from_path(&path)]).unwrap();
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
        fs.writev(&[vfsi_sync::WriteOp::from_path(
            &path,
            vfsi_sync::VfOffset::At(0),
            payload.clone(),
        )
        .with_creation()
        .with_truncate()])
            .unwrap();
        let files = fs
            .openv_simple(&[Path::new(&path)], libc::O_RDONLY, 0)
            .unwrap();
        fs.test_short_read_once(if small { 2 } else { 7 });
        let request = vfsi_sync::ReadOp::at(files[0].clone(), 0, size * 2);
        let (observed, eof) = if into {
            let mut buffer = vec![0; size * 2];
            let result = fs.readv_into(&[request], &mut [&mut buffer]).unwrap();
            buffer.truncate(result[0].read);
            (buffer, result[0].eof)
        } else {
            let result = fs.readv(&[request]).unwrap().remove(0);
            (result.data, result.eof)
        };
        fs.closev(&files).unwrap();
        fs.removev(&[vfsi_sync::VfFile::from_path(&path)]).unwrap();
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
fn shared_contract_through_the_published_crate() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping NFS integration test: VFSI_NFS_SERVER not set");
        return;
    };
    let root = format!("/vfsi-nfs-contract-{}", std::process::id());
    let _ = fs.rm(&[Path::new(&root)], true);
    test_support::run_suite(&mut fs, &root);
    fs.rm(&[Path::new(&root)], true)
        .expect("remove NFS contract root");
}

#[test]
fn read_into_stops_callbacks_on_first_error() {
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
    let reads: Vec<ReadOp> = (0..3)
        .map(|index| ReadOp {
            fh: fh.clone(),
            stateid,
            offset: index * 2,
            count: 2,
        })
        .collect();
    let mut calls = 0usize;
    let error = client
        .readv_into(&reads, |_, _| {
            calls += 1;
            if calls == 2 {
                Err(RpcError::transport("injected callback failure"))
            } else {
                Ok(())
            }
        })
        .expect_err("callback error must propagate");
    client.close(&fh, &stateid).expect("close test file");
    client.remove(&root, &name).expect("remove test file");
    assert!(error.is_transport());
    assert_eq!(error.op_index, 1);
    assert_eq!(calls, 2, "callbacks must stop on the first error");
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
    assert_eq!(result.failed, None);
    assert_eq!(result.close_failed, None);
    assert_eq!(result.counts, vec![Some(16); writes.len()]);
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
    client.remove_many(&dir, &names).expect("remove test files");
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
    assert_eq!(outcome.failed, None);
    assert_eq!(outcome.counts, vec![Some(data.len() as u32)]);
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
    assert_eq!(outcome.failed, None);
    for (fh, stateid) in outcome.opened.into_iter().map(Option::unwrap) {
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
        oversized_read.status,
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
    assert!(written.iter().all(|(count, _)| *count as usize == block));
    let reads: Vec<ReadOp> = (0..8)
        .map(|index| ReadOp {
            fh: fh.clone(),
            stateid,
            offset: (index * block) as u64,
            count: block as u32,
        })
        .collect();
    let read_results = client.readv(&reads).expect("bound aggregate reply bytes");
    for (index, (data, _)) in read_results.iter().enumerate() {
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
    assert_eq!(outcome.failed, None);
    client.set_max_compound_bytes(0);
    for (fh, stateid) in outcome.opened.into_iter().map(Option::unwrap) {
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
    let _ = fs.rm(&[root.as_path()], true);
    fs.mkdir(root.as_path(), 0o755).unwrap();
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
        fs.openv(&refs, &[libc::O_CREAT | libc::O_RDWR; 2], &[0o600; 2],)
            .unwrap_err()
            .is_transport()
    );
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
    assert_eq!(fs.test_deferred_descriptor_close_count(), 1);
    let next_path = root.join("next");
    let next = fs
        .openv(
            &[next_path.as_path()],
            &[libc::O_CREAT | libc::O_RDWR],
            &[0o600],
        )
        .unwrap();
    assert_eq!(fs.test_deferred_descriptor_close_count(), 0);
    fs.closev(&next).unwrap();
    fs.rm(&[root.as_path()], true).unwrap();
}
