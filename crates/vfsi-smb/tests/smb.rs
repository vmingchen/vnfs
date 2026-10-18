//! Opt-in integration coverage for the SMB2/3 backend.
//!
//! Set `VFSI_SMB_SERVER` and `VFSI_SMB_SHARE` to run against a Samba share.
//! `VFSI_SMB_USERNAME`, `VFSI_SMB_PASSWORD`, and `VFSI_SMB_DOMAIN` default to
//! empty strings for guest access.
//! Set `VFSI_SMB_RESTART_COMMAND` to a command that restarts the configured
//! server to enable recovery coverage.
//! The ignored DFS rejection test additionally needs `VFSI_SMB_DFS_SHARE`
//! naming a share configured with `msdfs root = yes` on that server.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
};
use vfsi_smb::{SmbConnectOptions, SmbVecFs};
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::test_support::borrow_writes;
use vfsi_sync::*;
use vfsi_sync::{
    AttrMask, Capabilities, ExtentPair, FileOperand, ReadOp, VF_ERR_UNSUPPORTED, VfAttrs, VfOffset,
    WriteOp,
};

use vfsi_sync::test_support as common;

#[cfg(feature = "test-faults")]
use std::sync::Arc;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
#[cfg(feature = "test-faults")]
use vfsi_sync::VfError;

fn required(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

/// Remove test fixtures through the local backing directory when the server
/// is running on this host. SMB intentionally lacks no-follow metadata, so the
/// generic recursive remover rejects these path-based cleanups.
fn cleanup_test_tree(path: &Path) {
    let Ok(local_root) = std::env::var("VFSI_SMB_LOCAL_ROOT") else {
        return;
    };
    let relative = path
        .strip_prefix("/")
        .expect("SMB test fixture paths are share-root relative");
    assert!(
        relative
            .components()
            .all(|component| { matches!(component, std::path::Component::Normal(_)) })
    );
    match fs::remove_dir_all(Path::new(&local_root).join(relative)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove local SMB test fixture {path:?}: {error}"),
    }
}

fn connect() -> Option<SmbVecFs> {
    let (server, share) = match (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) {
        (Ok(server), Ok(share)) => (server, share),
        _ => {
            assert!(
                !required("VFSI_SMB_REQUIRED"),
                "VFSI_SMB_SERVER and VFSI_SMB_SHARE are required in this integration job"
            );
            return None;
        }
    };
    let username = std::env::var("VFSI_SMB_USERNAME").unwrap_or_default();
    let password = std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default();
    let domain = std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default();
    Some(
        SmbVecFs::connect(&server, &share, &username, &password, &domain)
            .expect("connect to configured SMB test share"),
    )
}

#[test]
#[ignore = "requires VFSI_SMB_SERVER and a VFSI_SMB_DFS_SHARE advertising DFS"]
fn dfs_share_is_rejected_at_connect_while_an_ordinary_share_works() {
    let server = std::env::var("VFSI_SMB_SERVER").expect("VFSI_SMB_SERVER is required");
    let share = std::env::var("VFSI_SMB_DFS_SHARE").expect("VFSI_SMB_DFS_SHARE is required");
    let username = std::env::var("VFSI_SMB_USERNAME").unwrap_or_default();
    let password = std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default();
    let domain = std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default();
    let ordinary = connect().expect("ordinary share fixture is required");
    ordinary.shutdown().unwrap();
    let error = SmbVecFs::connect(&server, &share, &username, &password, &domain)
        .err()
        .expect("DFS membership must be rejected before exposing a backend");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(!error.is_transport());
    assert_eq!(error.path(), Some(Path::new(&share)));
    assert!(
        error
            .to_string()
            .contains("SMB DFS referrals are unsupported")
    );
}

#[test]
fn recursive_copy_uses_share_defaults_for_every_destination_directory() {
    let Some(mut fs) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-smb-copy-defaults-{}", std::process::id()));
    let source = root.join("source");
    fs.ensure_dir_impl(&source.join("nested/empty"), 0o777)
        .unwrap();
    fs.vwrite_impl(&borrow_writes(&[
        WriteOp::from_os_path(&source.join("root-file"), VfOffset::At(0), b"root".to_vec())
            .with_creation(),
        WriteOp::from_os_path(
            &source.join("nested/file"),
            VfOffset::At(0),
            b"child".to_vec(),
        )
        .with_creation(),
    ]))
    .unwrap();
    for server_copy in [false, true] {
        let destination = root.join(format!("copy-{server_copy}"));
        fs.copy_tree_impl(&source, &destination, false, server_copy)
            .unwrap();
        for (path, contents) in [
            ("root-file", b"root".as_slice()),
            ("nested/file", b"child".as_slice()),
        ] {
            assert_eq!(
                fs.read_raw_impl(&FileOperand::from_os_path(&destination.join(path)), 0, 16)
                    .unwrap(),
                contents
            );
        }
        assert_eq!(
            fs.stat_impl(&destination.join("nested/empty"))
                .unwrap()
                .ftype,
            VfType::Directory
        );
        fs.remove_paths_impl(
            &[
                &destination.join("nested/file"),
                &destination.join("root-file"),
            ],
            false,
        )
        .unwrap();
        fs.remove_paths_impl(
            &[
                &destination.join("nested/empty"),
                &destination.join("nested"),
                &destination,
            ],
            false,
        )
        .unwrap();
    }
    // Only implicit defaults change: explicit unsupported Unix modes still fail.
    let private = root.join("private");
    assert_eq!(
        fs.mkdir_raw_impl(&private, 0o700).unwrap_err().raw_code(),
        VF_ERR_UNSUPPORTED
    );
    assert!(!fs.exists_impl(&private).unwrap());
    fs.remove_paths_impl(
        &[&source.join("nested/file"), &source.join("root-file")],
        false,
    )
    .unwrap();
    fs.remove_paths_impl(
        &[
            &source.join("nested/empty"),
            &source.join("nested"),
            &source,
            &root,
        ],
        false,
    )
    .unwrap();
}

#[test]
fn native_directory_visitor_reenters_client_across_snapshot_pages() {
    let Some(backend) = connect() else {
        return;
    };
    let Ok(local_root) = std::env::var("VFSI_SMB_LOCAL_ROOT") else {
        return;
    };
    let name = format!("vfsi-smb-visit-{}", std::process::id());
    let local_dir = Path::new(&local_root).join(&name);
    let _ = fs::remove_dir_all(&local_dir);
    fs::create_dir(&local_dir).unwrap();
    for index in 0..3 {
        fs::write(local_dir.join(format!("item-{index}")), b"x").unwrap();
    }

    let client = vfsi_sync::FsClient::new(backend);
    let remote_dir = format!("/{name}");
    for sort in [false, true] {
        let mut seen = Vec::new();
        client
            .listdir(
                &remote_dir,
                vfsi_core::api::WalkOptions::new(vfsi_core::api::ListDirOptions::new())
                    .with_sort_by_name(sort),
                |entry| {
                    assert_eq!(client.attrs(entry.entry.path())?.len(), Some(1));
                    seen.push(entry.entry.path().to_path_buf());
                    Ok(vfsi_core::api::WalkControl::Continue)
                },
            )
            .unwrap();
        assert_eq!(seen.len(), 3);
        if sort {
            assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }
    drop(client);
    fs::remove_dir_all(local_dir).unwrap();
}

#[test]
fn configurable_deadlines_connect_to_samba() {
    let (server, share) = match (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) {
        (Ok(server), Ok(share)) => (server, share),
        _ => return,
    };
    let username = std::env::var("VFSI_SMB_USERNAME").unwrap_or_default();
    let password = std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default();
    let domain = std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default();
    let mut options = SmbConnectOptions::default();
    options.connect_timeout = std::time::Duration::from_secs(5);
    options.request_timeout = std::time::Duration::from_secs(5);
    SmbVecFs::connect_with_options(&server, &share, &username, &password, &domain, options)
        .expect("connect with explicit SMB deadlines")
        .shutdown()
        .expect("disconnect SMB deadline test client");
}

#[cfg(feature = "test-faults")]
#[test]
fn concurrent_write_preflight_detects_hard_link_aliases() {
    let Some(mut client) = connect() else {
        eprintln!("skipping SMB alias test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let Ok(local_root) = std::env::var("VFSI_SMB_LOCAL_ROOT") else {
        eprintln!("skipping SMB alias test: VFSI_SMB_LOCAL_ROOT not set");
        return;
    };
    let name = format!("vfsi-smb-alias-{}", std::process::id());
    let local_dir = Path::new(&local_root).join(&name);
    let _ = fs::remove_dir_all(&local_dir);
    fs::create_dir(&local_dir).unwrap();
    fs::set_permissions(&local_dir, fs::Permissions::from_mode(0o777)).unwrap();
    let local_first = local_dir.join("first");
    let local_alias = local_dir.join("alias");
    fs::write(&local_first, b"0000").unwrap();
    fs::set_permissions(&local_first, fs::Permissions::from_mode(0o666)).unwrap();
    fs::hard_link(&local_first, &local_alias).unwrap();

    let first = PathBuf::from(format!("/{name}/first"));
    let alias = PathBuf::from(format!("/{name}/alias"));
    client
        .vwrite_impl(&borrow_writes(&[
            WriteOp::from_os_path(&first, VfOffset::At(0), b"1111".to_vec()),
            WriteOp::from_os_path(&alias, VfOffset::At(0), b"2222".to_vec()),
        ]))
        .unwrap();
    assert!(!client.test_last_writev_was_concurrent());
    assert_eq!(
        client
            .vread_all_impl(&[FileOperand::from_os_path(&first)])
            .unwrap()[0],
        b"2222"
    );
    client.shutdown().unwrap();
    fs::remove_dir_all(local_dir).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn concurrent_write_preflight_keeps_distinct_files_vectorized() {
    let Some(mut client) = connect() else {
        eprintln!("skipping SMB identity test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-identities-{}", std::process::id()));
    cleanup_test_tree(&root);
    client.mkdir_raw_impl(&root, 0o777).unwrap();
    let first = root.join("first");
    let second = root.join("second");
    client
        .vwrite_impl(&borrow_writes(&[
            WriteOp::from_os_path(&first, VfOffset::At(0), b"0000".to_vec()).with_creation(),
            WriteOp::from_os_path(&second, VfOffset::At(0), b"0000".to_vec()).with_creation(),
        ]))
        .unwrap();
    client
        .vwrite_impl(&borrow_writes(&[
            WriteOp::from_os_path(&first, VfOffset::At(0), b"1111".to_vec()),
            WriteOp::from_os_path(&second, VfOffset::At(0), b"2222".to_vec()),
        ]))
        .unwrap();
    assert!(client.test_last_writev_was_concurrent());
    cleanup_test_tree(&root);
    client.shutdown().unwrap();
}

#[test]
fn recursive_remove_requires_no_follow_metadata() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB removal capability test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-no-lstat-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(&root, 0o777).unwrap();
    let child = root.join("keep");
    fs.vwrite_impl(&borrow_writes(&[WriteOp::from_os_path(
        &child,
        VfOffset::At(0),
        b"x".to_vec(),
    )
    .with_creation()]))
        .unwrap();

    let error = fs.remove_paths_impl(&[root.as_path()], true).unwrap_err();
    assert_eq!(error.raw_code(), VF_ERR_UNSUPPORTED);
    assert!(
        fs.exists_impl(&child).unwrap(),
        "unsupported removal must not mutate"
    );
    cleanup_test_tree(&root);
}

#[test]
fn timestamp_updates_reject_control_values_without_changing_the_file() {
    let Some(mut fs) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-smb-time-controls-{}", std::process::id()));
    fs.create_dir_impl(&root, 0o777).unwrap();
    let path = root.join("file");
    let file = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR, 0o666)
        .unwrap();
    let baseline = VfAttrs {
        file: file.clone(),
        masks: AttrMask::ATIME | AttrMask::MTIME,
        atime_sec: 1_700_000_000,
        mtime_sec: 1_700_000_001,
        ..VfAttrs::default()
    };
    fs.vsetattrs_raw_impl(std::slice::from_ref(&baseline))
        .unwrap();
    for target in [file.clone(), FileOperand::from_os_path(&path)] {
        let update = VfAttrs {
            file: target,
            masks: AttrMask::ATIME | AttrMask::MTIME,
            atime_sec: -11_644_473_600,
            mtime_sec: -11_644_473_600,
            ..VfAttrs::default()
        };
        let error = fs.vsetattrs_raw_impl(&[update]).unwrap_err();
        assert_eq!(error.raw_code(), libc::EINVAL as u32);
        assert_eq!(error.index(), Some(0));
        let mut attrs = [VfAttrs {
            file: file.clone(),
            masks: AttrMask::ATIME | AttrMask::MTIME,
            ..VfAttrs::default()
        }];
        fs.vgetattrs_impl(&mut attrs).unwrap();
        assert_eq!(attrs[0].atime_sec, baseline.atime_sec);
        assert_eq!(attrs[0].mtime_sec, baseline.mtime_sec);
    }
    fs.close_impl(&file).unwrap();
    cleanup_test_tree(&root);
}

#[test]
fn rust_native_file_workflow_on_smb() {
    let Some(fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let client = vfsi_sync::FsClient::new(fs);
    let root = PathBuf::from(format!("/vfsi-smb-native-{}", std::process::id()));
    cleanup_test_tree(&root);
    client.create_dir(&root).unwrap();
    let paths = [root.join("one"), root.join("two")];
    let files = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .vopen(&paths)
        .unwrap();
    client
        .vwrite_native(&[
            files[0].write_request_at(0, b"one"),
            files[1].write_request_at(0, b"two"),
        ])
        .unwrap();
    let values = client
        .vread_native(&[
            files[0].read_request_at(0, 3),
            files[1].read_request_at(0, 3),
        ])
        .unwrap();
    assert_eq!(values[0].data, b"one");
    assert_eq!(values[1].data, b"two");
    client.vclose_owned(files).unwrap();
    cleanup_test_tree(&root);
}

#[test]
fn descriptor_metadata_never_reopens_a_replaced_path() {
    let Some(mut fs) = connect() else {
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-retained-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.create_dir_impl(&root, 0o777).unwrap();
    let original = root.join("original");
    let renamed = root.join("renamed");
    let file = fs
        .open_raw_impl(&original, libc::O_CREAT | libc::O_RDWR, 0o666)
        .unwrap();
    fs.write_raw_impl(&file, 0, b"original").unwrap();
    assert_eq!(fs.fstat_impl(&file).unwrap().size, 8);
    fs.vrename_impl(&[(
        FileOperand::from_os_path(&original),
        FileOperand::from_os_path(&renamed),
    )])
    .unwrap();
    fs.vwrite_impl(&borrow_writes(&[WriteOp::from_os_path(
        &original,
        VfOffset::At(0),
        b"replacement".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    assert_eq!(fs.fstat_impl(&file).unwrap().size, 8);
    assert_eq!(fs.read_raw_impl(&file, 0, 8).unwrap(), b"original");
    assert_eq!(fs.stat_impl(&original).unwrap().size, 11);
    fs.close_impl(&file).unwrap();
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_openv_injected_registration_failure_closes_all_successes() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-openv-fault-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(root.as_path(), 0o777).unwrap();
    let paths = [root.join("f0"), root.join("f1"), root.join("f2")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 1 },
        VfError::transport(None, "injected registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VectorBackend::vopen_raw_impl(
        &mut fs,
        &refs,
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 3],
        &[0o666; 3],
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(fs.test_open_handle_count(), 0);
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_failed_strict_open_quarantines_unconfirmed_cleanup() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-cleanup-fault-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(root.as_path(), 0o777).unwrap();
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
        fs.vopen_raw_impl(&refs, &[libc::O_CREAT | libc::O_RDWR; 2], &[0o666; 2],)
            .unwrap_err()
            .is_transport()
    );
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
    assert_eq!(fs.test_deferred_close_count(), 1);
    let next_path = root.join("next");
    let next = fs
        .vopen_raw_impl(
            &[next_path.as_path()],
            &[libc::O_CREAT | libc::O_RDWR],
            &[0o666],
        )
        .unwrap();
    assert_eq!(fs.test_deferred_close_count(), 0);
    fs.vclose_impl(&next).unwrap();
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn owned_scalar_close_failure_drains_backend_cleanup_before_extraction() {
    let Some(mut fs) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-smb-owned-close-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(&root, 0o777).unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseDispatch { index: 0 },
        VfError::transport(None, "injected scalar close failure"),
    ));
    fs.set_fault_injector(script.clone());
    let client = vfsi_sync::FsClient::new(fs);
    let file = client.create(root.join("file")).unwrap();
    assert!(file.close().unwrap_err().is_transport());
    assert!(script.is_consumed());
    client
        .drain_cleanup()
        .expect("failed scalar close must drain the retained remote handle");
    let backend = client
        .into_inner()
        .expect("completed deferred cleanup must allow backend extraction");
    assert_eq!(backend.test_open_handle_count(), 0);
    assert_eq!(backend.test_deferred_close_count(), 0);
    backend.shutdown().unwrap();
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn owned_partial_close_drains_live_and_already_closed_handles() {
    let Some(mut fs) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-smb-owned-closev-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(&root, 0o777).unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseItem { index: 1 },
        VfError::failure(1, libc::EIO as u32),
    ));
    fs.set_fault_injector(script.clone());
    let client = vfsi_sync::FsClient::new(fs);
    let paths = [root.join("first"), root.join("second"), root.join("third")];
    let requests: Vec<_> = paths
        .iter()
        .map(|path| OpenOp::new(path, OpenFlags::WRITE | OpenFlags::CREATE_NEW))
        .collect();
    let mut files = client.vopen(&requests).unwrap();
    let error = client.vclose(&mut files).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(script.is_consumed());
    drop(files);
    client
        .drain_cleanup()
        .expect("cleanup must accept confirmed closes and finish the remaining handle");
    let backend = client
        .into_inner()
        .expect("partial vector close must not leave stale cleanup ownership");
    assert_eq!(backend.test_open_handle_count(), 0);
    assert_eq!(backend.test_deferred_close_count(), 0);
    backend.shutdown().unwrap();
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_closev_failure_keeps_handles_available_for_cleanup() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-closev-fault-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(root.as_path(), 0o777).unwrap();
    let paths = [root.join("f0"), root.join("f1")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let files = VectorBackend::vopen_raw_impl(
        &mut fs,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 2],
        &[0o666; 2],
    )
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseDispatch { index: 0 },
        VfError::transport(None, "injected close failure"),
    ));
    fs.set_fault_injector(script.clone());
    assert!(fs.vclose_impl(&files).unwrap_err().is_transport());
    assert_eq!(fs.test_open_handle_count(), 2);
    assert!(script.is_consumed());
    fs.vclose_impl(&files).unwrap();
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_closev_removes_successes_on_both_sides_of_a_failure() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-close-results-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(root.as_path(), 0o777).unwrap();
    let paths = [root.join("f0"), root.join("f1"), root.join("f2")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let files = VectorBackend::vopen_raw_impl(
        &mut fs,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[0o666; 3],
    )
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseItem { index: 1 },
        VfError::failure(1, libc::EIO as u32),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs.vclose_impl(&files).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 1);
    fs.close_impl(&files[1]).unwrap();
    assert_eq!(fs.test_open_handle_count(), 0);
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_scalar_open_registration_failure_closes_remote_open() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!(
        "/vfsi-smb-scalar-open-fault-{}",
        std::process::id()
    ));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(root.as_path(), 0o777).unwrap();
    let path = root.join("file");
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 0 },
        VfError::transport(None, "injected scalar registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    assert!(
        fs.open_raw_impl(path.as_path(), libc::O_CREAT | libc::O_RDWR, 0o666)
            .unwrap_err()
            .is_transport()
    );
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
    cleanup_test_tree(&root);
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_confirmed_write_advances_descriptor_when_flush_path_fails() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-partial-write-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.mkdir_raw_impl(root.as_path(), 0o777).unwrap();
    let path = root.join("file");
    let file = fs
        .open_raw_impl(path.as_path(), libc::O_CREAT | libc::O_RDWR, 0o666)
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterWriteChunk { chunk: 0 },
        VfError::transport(None, "injected failure after confirmed write"),
    ));
    fs.set_fault_injector(script.clone());
    assert!(
        fs.vwrite_impl(&borrow_writes(&[WriteOp::new(
            file.clone(),
            VfOffset::Cur,
            b"a".to_vec(),
        )]))
        .unwrap_err()
        .is_transport()
    );
    assert!(script.is_consumed());
    fs.vwrite_impl(&borrow_writes(&[WriteOp::new(
        file.clone(),
        VfOffset::Cur,
        b"b".to_vec(),
    )]))
    .unwrap();
    let read = fs
        .vread_impl(&[ReadOp::new(file.clone(), VfOffset::At(0), 2)])
        .unwrap();
    assert_eq!(read[0].data, b"ab");
    fs.close_impl(&file).unwrap();
    cleanup_test_tree(&root);
}

#[test]
fn samba_round_trip_and_copy() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let dialect = fs.dialect() as u16;
    assert!((0x0202..=0x0311).contains(&dialect));
    if let Ok(expected) = std::env::var("VFSI_SMB_EXPECT_DIALECT") {
        let expected = u16::from_str_radix(expected.trim_start_matches("0x"), 16)
            .expect("hex VFSI_SMB_EXPECT_DIALECT");
        assert_eq!(dialect, expected);
    }
    let expect_copy = std::env::var("VFSI_SMB_EXPECT_SERVER_COPY").as_deref() != Ok("0");
    assert_eq!(
        fs.capabilities().contains(Capabilities::SERVER_COPY),
        expect_copy
    );
    assert!(!fs.capabilities().intersects(Capabilities::UNIX_SEMANTICS));
    assert_eq!(fs.server_copy_enabled(), expect_copy);

    let root = PathBuf::from(format!("/vfsi-smb-test-{}", std::process::id()));
    cleanup_test_tree(&root);
    fs.ensure_dir_impl(&root, 0o777)
        .expect("create test directory");

    let batch_dirs = [root.join("batch-a"), root.join("batch-b")];
    let dir_attrs: Vec<VfAttrs> = batch_dirs
        .iter()
        .map(|path| VfAttrs {
            file: FileOperand::from_os_path(path),
            masks: AttrMask::MODE,
            mode: 0o777,
            ..VfAttrs::default()
        })
        .collect();
    fs.vmkdir_impl(&dir_attrs).expect("concurrent mkdirv");
    let batch_files = [batch_dirs[0].join("one"), batch_dirs[1].join("two")];
    let batch_refs: Vec<&Path> = batch_files.iter().map(PathBuf::as_path).collect();
    let opened = fs
        .vopen_raw_simple_impl(&batch_refs, libc::O_CREAT | libc::O_RDWR, 0o666)
        .expect("concurrent openv");
    fs.vclose_impl(&opened).expect("concurrent closev");
    let renamed_files = [batch_dirs[0].join("renamed"), batch_dirs[1].join("renamed")];
    fs.vrename_impl(&[
        (
            FileOperand::from_os_path(&batch_files[0]),
            FileOperand::from_os_path(&renamed_files[0]),
        ),
        (
            FileOperand::from_os_path(&batch_files[1]),
            FileOperand::from_os_path(&renamed_files[1]),
        ),
    ])
    .expect("concurrent renamev");
    fs.vremove_impl(&[
        FileOperand::from_os_path(&renamed_files[0]),
        FileOperand::from_os_path(&renamed_files[1]),
    ])
    .expect("concurrent removev files");
    fs.vremove_impl(&[
        FileOperand::from_os_path(&batch_dirs[0]),
        FileOperand::from_os_path(&batch_dirs[1]),
    ])
    .expect("concurrent removev directories");

    let source = root.join("source.bin");
    let renamed = root.join("renamed.bin");
    let copied = root.join("copied.bin");
    let file = fs
        .open_raw_impl(&source, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
        .expect("open source");
    assert_eq!(fs.write_raw_impl(&file, 0, b"hello over smb").unwrap(), 14);
    assert_eq!(fs.read_raw_impl(&file, 6, 4).unwrap(), b"over");
    fs.close_impl(&file).unwrap();

    let path_read = fs
        .vread_impl(&[ReadOp::from_os_path(&source, VfOffset::At(0), 64)])
        .unwrap();
    assert_eq!(path_read[0].data, b"hello over smb");

    fs.vrename_impl(&[(
        FileOperand::from_os_path(&source),
        FileOperand::from_os_path(&renamed),
    )])
    .unwrap();
    fs.vcopy_impl(
        &[ExtentPair::from_os_paths(&renamed, 0, &copied, 0, None)],
        vfsi_core::CopyOptions::new(),
    )
    .unwrap();
    assert_eq!(
        fs.vread_all_impl(&[FileOperand::from_os_path(&copied)])
            .unwrap()[0],
        b"hello over smb"
    );

    let patch = WriteOp::from_os_path(&copied, VfOffset::At(6), b"SMB3".to_vec());
    fs.vwrite_impl(&borrow_writes(&[patch])).unwrap();
    assert_eq!(
        fs.vread_all_impl(&[FileOperand::from_os_path(&copied)])
            .unwrap()[0],
        b"hello SMB3 smb"
    );
    assert_eq!(
        fs.lstat_impl(&copied).unwrap_err().raw_code(),
        VF_ERR_UNSUPPORTED
    );
    let no_follow_mode = VfAttrs {
        file: FileOperand::from_os_path(&copied),
        masks: AttrMask::MODE,
        mode: 0o600,
        ..VfAttrs::default()
    };
    assert_eq!(
        fs.vsetattrs_raw_nofollow_impl(&[no_follow_mode])
            .unwrap_err()
            .raw_code(),
        VF_ERR_UNSUPPORTED
    );

    // Exceed Samba's usual single-request limit so vwrite_impl chunks
    // the write and vread_all_impl uses the bounded whole-file reader.
    let large = root.join("large.bin");
    let large_data = vec![b'L'; 10 * 1024 * 1024 + 123];
    fs.vwrite_impl(&borrow_writes(&[WriteOp::from_os_path(
        &large,
        VfOffset::At(0),
        large_data.clone(),
    )
    .with_creation()
    .with_truncate()]))
        .expect("large SMB write");
    assert_eq!(
        fs.vread_all_impl(&[FileOperand::from_os_path(&large)])
            .expect("large SMB read")[0],
        large_data
    );
    cleanup_test_tree(Path::new(&root));
}

#[test]
fn shared_suite_on_smb() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB conformance test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let base = format!("/vfsi-smb-conformance-{}", std::process::id());
    cleanup_test_tree(Path::new(&base));
    common::run_suite(&mut fs, &base);
    cleanup_test_tree(Path::new(&base));
}

#[test]
fn path_reads_recover_after_server_restart() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB reconnect test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let Ok(restart) = std::env::var("VFSI_SMB_RESTART_COMMAND") else {
        assert!(
            !required("VFSI_SMB_REQUIRE_RECONNECT"),
            "VFSI_SMB_RESTART_COMMAND is required in this reconnect job"
        );
        eprintln!("skipping SMB reconnect test: VFSI_SMB_RESTART_COMMAND not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-reconnect-{}", std::process::id()));
    let file = root.join("survives.bin");
    cleanup_test_tree(&root);
    fs.ensure_dir_impl(&root, 0o777).unwrap();
    fs.vwrite_impl(&borrow_writes(&[WriteOp::from_os_path(
        &file,
        VfOffset::At(0),
        b"after restart".to_vec(),
    )
    .with_creation()
    .with_truncate()]))
        .unwrap();
    assert!(
        std::process::Command::new("sh")
            .arg("-c")
            .arg(restart)
            .status()
            .expect("run Samba restart command")
            .success()
    );

    let result = fs
        .vread_impl(&[ReadOp::from_os_path(&file, VfOffset::At(0), 64)])
        .expect("path read should reconnect and re-establish the share");
    assert_eq!(result[0].data, b"after restart");
    assert_eq!(fs.stat_impl(&file).unwrap().size, 13);
    cleanup_test_tree(&root);
}

#[cfg(unix)]
#[test]
fn listing_walk_and_tree_copy_do_not_follow_directory_reparse_points() {
    let Some(mut fs) = connect() else { return };
    let Ok(local_root) = std::env::var("VFSI_SMB_LOCAL_ROOT") else {
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-reparse-{}", std::process::id()));
    let source = root.join("source");
    let local_root = Path::new(&local_root).join(root.strip_prefix("/").unwrap());
    let local_source = local_root.join("source");
    let local_outside = local_root.join("outside");
    fs::create_dir_all(&local_source).unwrap();
    fs::create_dir_all(&local_outside).unwrap();
    // Samba may force a different Unix user than the process creating fixtures.
    fs::set_permissions(&local_root, fs::Permissions::from_mode(0o777)).unwrap();
    fs::write(local_outside.join("secret"), b"outside source tree").unwrap();
    std::os::unix::fs::symlink("../outside", local_source.join("alias")).unwrap();

    let shallow = fs
        .listdir_impl(&source, AttrMask::MODE, 0, false)
        .expect("shallow listing should identify the directory reparse point");
    assert!(shallow.iter().any(|entry| {
        entry
            .file
            .path()
            .is_some_and(|path| path.ends_with("alias"))
            && entry.ftype == VfType::Symlink
    }));

    let walked = fs
        .walk_impl(&source, AttrMask::MODE, &mut |_, _| {})
        .expect("walk should stop at the directory reparse point");
    assert!(!walked.iter().flat_map(|dir| &dir.entries).any(|entry| {
        entry
            .file
            .path()
            .is_some_and(|path| path.ends_with("alias/secret"))
    }));

    let entries = fs
        .listdir_impl(&source, AttrMask::MODE, 0, true)
        .expect("recursive listing should identify the directory reparse point");
    assert!(entries.iter().any(|entry| {
        entry
            .file
            .path()
            .is_some_and(|path| path.ends_with("alias"))
            && entry.ftype == VfType::Symlink
    }));
    assert!(!entries.iter().any(|entry| {
        entry
            .file
            .path()
            .is_some_and(|path| path.ends_with("alias/secret"))
    }));

    let destination = root.join("copy");
    assert!(
        fs.copy_tree_impl(&source, &destination, false, false)
            .is_err()
    );
    assert!(!fs.exists_impl(&destination.join("alias/secret")).unwrap());

    // SMB cannot create symlinks, so requesting preservation must report the
    // unsupported operation instead of silently copying through the link.
    let preserve_destination = root.join("copy-preserving-links");
    let error = fs
        .copy_tree_impl(&source, &preserve_destination, true, false)
        .expect_err("requested symlink preservation must not be ignored");
    assert_eq!(error.raw_code(), VF_ERR_UNSUPPORTED);

    // Isolate a regular-file link: ignoring `symlinks=true` would silently
    // create a regular destination file containing the link target's bytes.
    let file_source = root.join("file-source");
    let local_file_source = local_root.join("file-source");
    fs::create_dir_all(&local_file_source).unwrap();
    std::os::unix::fs::symlink("../outside/secret", local_file_source.join("alias")).unwrap();
    let file_destination = root.join("file-copy-preserving-links");
    let error = fs
        .copy_tree_impl(&file_source, &file_destination, true, false)
        .expect_err("file-symlink preservation must not copy through silently");
    assert_eq!(error.raw_code(), VF_ERR_UNSUPPORTED);
    assert!(!fs.exists_impl(&file_destination.join("alias")).unwrap());
    cleanup_test_tree(&root);
}

#[test]
fn remove_reparse_entries_without_deleting_their_targets() {
    let Some(mut fs) = connect() else { return };
    let Ok(local_root) = std::env::var("VFSI_SMB_LOCAL_ROOT") else {
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-remove-reparse-{}", std::process::id()));
    cleanup_test_tree(&root);
    let local_root = Path::new(&local_root).join(root.strip_prefix("/").unwrap());
    fs::create_dir_all(&local_root).unwrap();
    // Allow the server's forced Unix user to unlink entries in this owned fixture.
    fs::set_permissions(&local_root, fs::Permissions::from_mode(0o777)).unwrap();

    let target_file = local_root.join("target-file");
    let file_link = local_root.join("file-link");
    let ordinary_file = local_root.join("ordinary-file");
    let ordinary_directory = local_root.join("ordinary-directory");
    fs::write(&target_file, b"keep target").unwrap();
    fs::write(&ordinary_file, b"remove file").unwrap();
    fs::create_dir(&ordinary_directory).unwrap();
    std::os::unix::fs::symlink("target-file", &file_link).unwrap();

    let target_directory = local_root.join("target-directory");
    let directory_link = local_root.join("directory-link");
    fs::create_dir(&target_directory).unwrap();
    fs::write(target_directory.join("keep"), b"keep target directory").unwrap();
    std::os::unix::fs::symlink("target-directory", &directory_link).unwrap();

    let dangling_link = local_root.join("dangling-link");
    std::os::unix::fs::symlink("missing-target", &dangling_link).unwrap();
    let paths = [
        PathBuf::from(format!("{}/file-link", root.display())),
        PathBuf::from(format!("{}/directory-link", root.display())),
        PathBuf::from(format!("{}/dangling-link", root.display())),
        PathBuf::from(format!("{}/ordinary-file", root.display())),
        PathBuf::from(format!("{}/ordinary-directory", root.display())),
    ];
    fs.vremove_impl(
        &paths
            .iter()
            .map(|path| FileOperand::from_os_path(path.as_path()))
            .collect::<Vec<_>>(),
    )
    .expect("remove only the final reparse entries");

    assert_eq!(fs::read(target_file).unwrap(), b"keep target");
    assert_eq!(
        fs::read(target_directory.join("keep")).unwrap(),
        b"keep target directory"
    );
    assert_eq!(
        fs::symlink_metadata(ordinary_file).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        fs::symlink_metadata(ordinary_directory).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    for link in [file_link, directory_link, dangling_link] {
        assert_eq!(
            fs::symlink_metadata(link).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
    cleanup_test_tree(&root);
}

#[test]
fn copy_options_reject_unsupported_symlink_preservation_before_lookup() {
    let Some(mut fs) = connect() else {
        return;
    };
    let options = vfsi_sync::CopyOptions::new().with_follow_source_symlinks(false);
    fs.vcopy_impl(&[], options).unwrap();
    let error = fs
        .vcopy_impl(
            &[ExtentPair::new(
                "/missing-source",
                0,
                "/unused-destination",
                0,
                None,
            )],
            options,
        )
        .unwrap_err();
    assert_eq!(error.raw_code(), VF_ERR_UNSUPPORTED);
    assert_eq!(error.index(), Some(0));
}

#[test]
fn client_extent_copy_rejects_forward_aliases_before_mutation() {
    let Some(mut backend) = connect() else { return };
    let Ok(local_root) = std::env::var("VFSI_SMB_LOCAL_ROOT") else {
        assert!(
            !required("VFSI_SMB_REQUIRED"),
            "local backing root is required for the hard-link alias fixture"
        );
        return;
    };
    let root = PathBuf::from(format!("/vfsi-copy-alias-{}", std::process::id()));
    let local = Path::new(&local_root).join(root.strip_prefix("/").unwrap());
    backend.ensure_dir_impl(&root, 0o777).unwrap();
    let source = root.join("source");
    let alias = root.join("alias");
    let data: Vec<u8> = (0..(2 * 1024 * 1024 + 257))
        .map(|i| (i % 251) as u8)
        .collect();
    fs::write(local.join("source"), &data).unwrap();
    fs::hard_link(local.join("source"), local.join("alias")).unwrap();
    for destination in [&source, &alias] {
        for length in [Some(data.len() as u64 - 1), None] {
            let result = backend.vcopy_data_impl(&[ExtentPair::from_os_paths(
                &source,
                0,
                destination,
                1,
                length,
            )]);
            let unchanged = fs::read(local.join("source")).unwrap() == data;
            assert!(unchanged, "copy overwrote unread source bytes");
            let error =
                result.expect_err("forward alias copy must not overwrite unread source bytes");
            assert_eq!(error.raw_code(), libc::EINVAL as u32);
            assert_eq!(error.index(), Some(0));
            assert!(unchanged, "rejected copies must leave the source unchanged");
        }
    }
    cleanup_test_tree(&root);
}

#[test]
fn client_extent_copy_preserves_safe_shifted_ranges() {
    let Some(mut backend) = connect() else { return };
    let root = PathBuf::from(format!("/vfsi-copy-safe-{}", std::process::id()));
    backend.ensure_dir_impl(&root, 0o777).unwrap();
    let source = root.join("source");
    let other = root.join("other");
    for (src, dst, length, separate, expected) in [
        (4, 0, None, false, b"456789".as_slice()),
        (8, 10, Some(10), false, b"012345678989".as_slice()),
        (0, 2, Some(6), true, b"\x00\x00012345".as_slice()),
        (0, 0, None, false, b"0123456789".as_slice()),
    ] {
        backend
            .vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_os_path(
                &source,
                VfOffset::At(0),
                b"0123456789".to_vec(),
            )
            .with_creation()
            .with_truncate()]))
            .unwrap();
        let destination = if separate { &other } else { &source };
        backend
            .vcopy_data_impl(&[ExtentPair::from_os_paths(
                &source,
                src,
                destination,
                dst,
                length,
            )])
            .unwrap_or_else(|error| {
                panic!("copy {src} -> {dst}, {length:?}, separate={separate}: {error:?}")
            });
        assert_eq!(
            backend
                .vread_all_impl(&[FileOperand::from_os_path(destination)])
                .unwrap()[0],
            expected
        );
    }
    cleanup_test_tree(&root);
}
