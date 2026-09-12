//! Opt-in integration coverage for the SMB2/3 backend.
//!
//! Set `VFSI_SMB_SERVER` and `VFSI_SMB_SHARE` to run against a Samba share.
//! `VFSI_SMB_USERNAME`, `VFSI_SMB_PASSWORD`, and `VFSI_SMB_DOMAIN` default to
//! empty strings for guest access.
//! Set `VFSI_SMB_RESTART_COMMAND` to a command that restarts the configured
//! server to enable recovery coverage.

use std::path::{Path, PathBuf};

use vnfs::{
    AttrMask, ExtentPair, SmbVecFs, VF_CAP_HARDLINKS, VF_CAP_LSTAT, VF_CAP_NON_UTF8_PATHS,
    VF_CAP_POSIX_METADATA, VF_CAP_SERVER_COPY, VF_CAP_SYMLINKS, VF_ERR_UNSUPPORTED, VecFs, VfAttrs,
    VfFile, VfOffset, WriteOp,
};

mod common;

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
fn samba_round_trip_and_copy() {
    let Some(mut fs) = connect() else {
        eprintln!("skipping SMB integration test: VFSI_SMB_SERVER/SHARE not set");
        return;
    };
    let dialect = fs.smb_dialect().expect("SMB dialect");
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
        .readv(&[vnfs::ReadOp::from_os_path(&source, VfOffset::At(0), 64)])
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
        .readv(&[vnfs::ReadOp::from_os_path(&file, VfOffset::At(0), 64)])
        .expect("path read should reconnect and re-establish the share");
    assert_eq!(result[0].data, b"after restart");
    assert_eq!(fs.stat(&file).unwrap().size, 13);
    fs.rm(&[root.as_path()], true).unwrap();
}
