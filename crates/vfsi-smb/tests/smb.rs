//! Opt-in integration coverage for the SMB2/3 backend.
//!
//! Set `VFSI_SMB_SERVER` and `VFSI_SMB_SHARE` to run against a Samba share.
//! `VFSI_SMB_USERNAME`, `VFSI_SMB_PASSWORD`, and `VFSI_SMB_DOMAIN` default to
//! empty strings for guest access.
//! Set `VFSI_SMB_RESTART_COMMAND` to a command that restarts the configured
//! server to enable recovery coverage.

use std::path::{Path, PathBuf};

use vfsi_smb::SmbExtensions;
use vfsi_smb::SmbVecFs;
use vfsi_sync::{
    AttrMask, ExtentPair, ReadOp, VF_CAP_HARDLINKS, VF_CAP_LSTAT, VF_CAP_NON_UTF8_PATHS,
    VF_CAP_POSIX_METADATA, VF_CAP_SERVER_COPY, VF_CAP_SYMLINKS, VF_ERR_UNSUPPORTED, VecFs, VfAttrs,
    VfFile, VfOffset, WriteOp,
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
fn rust_native_file_workflow_on_smb() {
    let Some(fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let client = vfsi_sync::FsClient::new(fs);
    let root = PathBuf::from(format!("/vfsi-smb-native-{}", std::process::id()));
    let _ = client.remove_dir_all(&root);
    client.create_dir(&root).unwrap();
    let paths = [root.join("one"), root.join("two")];
    let files = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .openv(&paths)
        .unwrap();
    client
        .writev(&[
            files[0].write_request_at(0, b"one"),
            files[1].write_request_at(0, b"two"),
        ])
        .unwrap();
    let values = client
        .readv(&[
            files[0].read_request_at(0, 3),
            files[1].read_request_at(0, 3),
        ])
        .unwrap();
    assert_eq!(values[0].data, b"one");
    assert_eq!(values[1].data, b"two");
    client.closev(files).unwrap();
    client.remove_dir_all(&root).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_openv_injected_registration_failure_closes_all_successes() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-openv-fault-{}", std::process::id()));
    let _ = fs.rm(&[root.as_path()], true);
    fs.mkdir(root.as_path(), 0o755).unwrap();
    let paths = [root.join("f0"), root.join("f1"), root.join("f2")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 1 },
        VfError::transport(None, "injected registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VecFs::openv(
        &mut fs,
        &refs,
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(fs.test_open_handle_count(), 0);
    fs.rm(&[root.as_path()], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_failed_strict_open_quarantines_unconfirmed_cleanup() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-cleanup-fault-{}", std::process::id()));
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
    assert_eq!(fs.test_deferred_close_count(), 1);
    let next_path = root.join("next");
    let next = fs
        .openv(
            &[next_path.as_path()],
            &[libc::O_CREAT | libc::O_RDWR],
            &[0o600],
        )
        .unwrap();
    assert_eq!(fs.test_deferred_close_count(), 0);
    fs.closev(&next).unwrap();
    fs.rm(&[root.as_path()], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_closev_failure_keeps_handles_available_for_cleanup() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-closev-fault-{}", std::process::id()));
    let _ = fs.rm(&[root.as_path()], true);
    fs.mkdir(root.as_path(), 0o755).unwrap();
    let paths = [root.join("f0"), root.join("f1")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let files = VecFs::openv(
        &mut fs,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseDispatch { index: 0 },
        VfError::transport(None, "injected close failure"),
    ));
    fs.set_fault_injector(script.clone());
    assert!(fs.closev(&files).unwrap_err().is_transport());
    assert_eq!(fs.test_open_handle_count(), 2);
    assert!(script.is_consumed());
    fs.closev(&files).unwrap();
    fs.rm(&[root.as_path()], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_closev_removes_successes_on_both_sides_of_a_failure() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-close-results-{}", std::process::id()));
    let _ = fs.rm(&[root.as_path()], true);
    fs.mkdir(root.as_path(), 0o755).unwrap();
    let paths = [root.join("f0"), root.join("f1"), root.join("f2")];
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let files = VecFs::openv(
        &mut fs,
        &refs,
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseItem { index: 1 },
        VfError::failure(1, libc::EIO as u32),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs.closev(&files).unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 1);
    fs.close(&files[1]).unwrap();
    assert_eq!(fs.test_open_handle_count(), 0);
    fs.rm(&[root.as_path()], true).unwrap();
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
    let _ = fs.rm(&[root.as_path()], true);
    fs.mkdir(root.as_path(), 0o755).unwrap();
    let path = root.join("file");
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 0 },
        VfError::transport(None, "injected scalar registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    assert!(
        fs.open(path.as_path(), libc::O_CREAT | libc::O_RDWR, 0o600)
            .unwrap_err()
            .is_transport()
    );
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
    fs.rm(&[root.as_path()], true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn smb_confirmed_write_advances_descriptor_when_flush_path_fails() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let root = PathBuf::from(format!("/vfsi-smb-partial-write-{}", std::process::id()));
    let _ = fs.rm(&[root.as_path()], true);
    fs.mkdir(root.as_path(), 0o755).unwrap();
    let path = root.join("file");
    let file = fs
        .open(path.as_path(), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterWriteChunk { chunk: 0 },
        VfError::transport(None, "injected failure after confirmed write"),
    ));
    fs.set_fault_injector(script.clone());
    assert!(
        fs.writev(&[WriteOp::new(file.clone(), VfOffset::Cur, b"a".to_vec(),)])
            .unwrap_err()
            .is_transport()
    );
    assert!(script.is_consumed());
    fs.writev(&[WriteOp::new(file.clone(), VfOffset::Cur, b"b".to_vec())])
        .unwrap();
    let read = fs
        .readv(&[ReadOp::new(file.clone(), VfOffset::At(0), 2)])
        .unwrap();
    assert_eq!(read[0].data, b"ab");
    fs.close(&file).unwrap();
    fs.rm(&[root.as_path()], true).unwrap();
}

#[test]
fn samba_round_trip_and_copy() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let dialect = fs.smb_dialect_revision();
    assert!((0x0202..=0x0311).contains(&dialect));
    if let Ok(expected) = std::env::var("VFSI_SMB_EXPECT_DIALECT") {
        let expected = u16::from_str_radix(expected.trim_start_matches("0x"), 16)
            .expect("hex VFSI_SMB_EXPECT_DIALECT");
        assert_eq!(dialect, expected);
    }
    let expect_copy = std::env::var("VFSI_SMB_EXPECT_SERVER_COPY").as_deref() != Ok("0");
    assert_eq!(fs.capabilities() & VF_CAP_SERVER_COPY != 0, expect_copy);
    assert_eq!(
        fs.capabilities()
            & (VF_CAP_POSIX_METADATA
                | VF_CAP_SYMLINKS
                | VF_CAP_HARDLINKS
                | VF_CAP_NON_UTF8_PATHS
                | VF_CAP_LSTAT),
        0
    );

    let root = PathBuf::from(format!("/vfsi-smb-test-{}", std::process::id()));
    let _ = fs.rm(&[root.as_path()], true);
    fs.ensure_dir(&root, 0o755).expect("create test directory");

    let batch_dirs = [root.join("batch-a"), root.join("batch-b")];
    let dir_attrs: Vec<VfAttrs> = batch_dirs
        .iter()
        .map(|path| VfAttrs {
            file: VfFile::from_os_path(path),
            masks: AttrMask::MODE,
            mode: 0o755,
            ..VfAttrs::default()
        })
        .collect();
    fs.mkdirv(&dir_attrs).expect("concurrent mkdirv");
    let batch_files = [batch_dirs[0].join("one"), batch_dirs[1].join("two")];
    let batch_refs: Vec<&Path> = batch_files.iter().map(PathBuf::as_path).collect();
    let opened = fs
        .openv_simple(&batch_refs, libc::O_CREAT | libc::O_RDWR, 0o644)
        .expect("concurrent openv");
    fs.closev(&opened).expect("concurrent closev");
    let renamed_files = [batch_dirs[0].join("renamed"), batch_dirs[1].join("renamed")];
    fs.renamev(&[
        (
            VfFile::from_os_path(&batch_files[0]),
            VfFile::from_os_path(&renamed_files[0]),
        ),
        (
            VfFile::from_os_path(&batch_files[1]),
            VfFile::from_os_path(&renamed_files[1]),
        ),
    ])
    .expect("concurrent renamev");
    fs.removev(&[
        VfFile::from_os_path(&renamed_files[0]),
        VfFile::from_os_path(&renamed_files[1]),
    ])
    .expect("concurrent removev files");
    fs.removev(&[
        VfFile::from_os_path(&batch_dirs[0]),
        VfFile::from_os_path(&batch_dirs[1]),
    ])
    .expect("concurrent removev directories");

    let source = root.join("source.bin");
    let renamed = root.join("renamed.bin");
    let copied = root.join("copied.bin");
    let file = fs
        .open(&source, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o644)
        .expect("open source");
    assert_eq!(fs.write(&file, 0, b"hello over smb").unwrap(), 14);
    assert_eq!(fs.read(&file, 6, 4).unwrap(), b"over");
    fs.close(&file).unwrap();

    let path_read = fs
        .readv(&[ReadOp::from_os_path(&source, VfOffset::At(0), 64)])
        .unwrap();
    assert_eq!(path_read[0].data, b"hello over smb");

    fs.renamev(&[(
        VfFile::from_os_path(&source),
        VfFile::from_os_path(&renamed),
    )])
    .unwrap();
    fs.copyv(&[ExtentPair::from_os_paths(&renamed, 0, &copied, 0, None)])
        .unwrap();
    assert_eq!(
        fs.read_allv(&[VfFile::from_os_path(&copied)]).unwrap()[0],
        b"hello over smb"
    );

    let patch = WriteOp::from_os_path(&copied, VfOffset::At(6), b"SMB3".to_vec());
    fs.writev(&[patch]).unwrap();
    assert_eq!(
        fs.read_allv(&[VfFile::from_os_path(&copied)]).unwrap()[0],
        b"hello SMB3 smb"
    );
    assert_eq!(fs.lstat(&copied).unwrap_err().err_no(), VF_ERR_UNSUPPORTED);
    let no_follow_mode = VfAttrs {
        file: VfFile::from_os_path(&copied),
        masks: AttrMask::MODE,
        mode: 0o600,
        ..VfAttrs::default()
    };
    assert_eq!(
        fs.lsetattrsv(&[no_follow_mode]).unwrap_err().err_no(),
        VF_ERR_UNSUPPORTED
    );

    // Exceed Samba's usual single-request limit so writev chunks the write
    // and read_allv falls back to the crate's pipelined whole-file reader.
    let large = root.join("large.bin");
    let large_data = vec![b'L'; 10 * 1024 * 1024 + 123];
    fs.writev(&[
        WriteOp::from_os_path(&large, VfOffset::At(0), large_data.clone())
            .with_creation()
            .with_truncate(),
    ])
    .expect("large SMB write");
    assert_eq!(
        fs.read_allv(&[VfFile::from_os_path(&large)])
            .expect("large SMB read")[0],
        large_data
    );
    fs.rm(&[Path::new(&root)], true).expect("remove test tree");
}

#[test]
fn shared_suite_on_smb() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB conformance test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let base = format!("/vfsi-smb-conformance-{}", std::process::id());
    let _ = fs.rm(&[Path::new(&base)], true);
    common::run_suite(&mut fs, &base);
    fs.rm(&[Path::new(&base)], true)
        .expect("remove conformance root");
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
    let _ = fs.rm(&[root.as_path()], true);
    fs.ensure_dir(&root, 0o755).unwrap();
    fs.writev(&[
        WriteOp::from_os_path(&file, VfOffset::At(0), b"after restart".to_vec())
            .with_creation()
            .with_truncate(),
    ])
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
        .readv(&[ReadOp::from_os_path(&file, VfOffset::At(0), 64)])
        .expect("path read should reconnect and re-establish the share");
    assert_eq!(result[0].data, b"after restart");
    assert_eq!(fs.stat(&file).unwrap().size, 13);
    fs.rm(&[root.as_path()], true).unwrap();
}
