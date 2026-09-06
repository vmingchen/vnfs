//! Opt-in integration coverage for the SMB2/3 backend.
//!
//! Set `VFSI_SMB_SERVER` and `VFSI_SMB_SHARE` to run against a Samba share.
//! `VFSI_SMB_USERNAME`, `VFSI_SMB_PASSWORD`, and `VFSI_SMB_DOMAIN` default to
//! empty strings for guest access.

use std::path::{Path, PathBuf};

use vnfs::{ExtentPair, SmbVecFs, VecFs, VfFile, VfOffset, WriteOp};

fn connect() -> Option<SmbVecFs> {
    let server = std::env::var("VFSI_SMB_SERVER").ok()?;
    let share = std::env::var("VFSI_SMB_SHARE").ok()?;
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
    assert!(matches!(fs.smb_dialect(), Some(0x0202..=0x0311)));

    let root = PathBuf::from(format!("/vfsi-smb-test-{}", std::process::id()));
    let _ = fs.rm(&[root.as_path()], true);
    fs.ensure_dir(&root, 0o755).expect("create test directory");

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
    fs.rm(&[Path::new(&root)], true).expect("remove test tree");
}
