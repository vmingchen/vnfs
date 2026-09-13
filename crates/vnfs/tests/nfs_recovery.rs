//! Coordinated server-restart coverage for NFS session and open-state recovery.
//!
//! The test is inert unless `VNFS_RECOVERY_CONTROL_DIR` is set. CI waits for
//! `ready`, restarts NFS-Ganesha, then creates `continue`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vnfs::{NfsVecFs, ReadOp, SeekFrom, VecFs, VfFile, VfOffset, WriteOp};

fn client() -> NfsVecFs {
    let minor = match std::env::var("VNFS_TEST_MINOR").as_deref() {
        Ok("1") => Some(1),
        Ok("2") => Some(2),
        _ => None,
    };
    NfsVecFs::builder("127.0.0.1")
        .minor_version(minor)
        .client_owner(format!("vnfs-recovery-test-{}", std::process::id()))
        .connect()
        .expect("connect to local NFS server")
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn read_recovers_after_server_restart_and_reopens_live_descriptor() {
    let Some(control) = std::env::var_os("VNFS_RECOVERY_CONTROL_DIR").map(PathBuf::from) else {
        eprintln!("skipping restart coordination; VNFS_RECOVERY_CONTROL_DIR is unset");
        return;
    };

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let dir = format!("/.vnfs-recovery-{}-{unique}", std::process::id());
    let first_path = format!("{dir}/first");
    let second_path = format!("{dir}/second");
    let mut fs = client();
    fs.mkdir(Path::new(&dir), 0o755)
        .expect("create recovery directory");
    fs.writev(&[
        WriteOp::from_path(&first_path, VfOffset::At(0), b"abcdef".to_vec())
            .with_creation()
            .with_truncate(),
        WriteOp::from_path(&second_path, VfOffset::At(0), b"uvwxyz".to_vec())
            .with_creation()
            .with_truncate(),
    ])
    .expect("create recovery files");
    let files = fs
        .openv_simple(
            &[Path::new(&first_path), Path::new(&second_path)],
            libc::O_RDONLY,
            0,
        )
        .expect("open recovery files");
    fs.fseek(&files[0], 2, SeekFrom::Set)
        .expect("position first descriptor");
    fs.fseek(&files[1], 1, SeekFrom::Set)
        .expect("position second descriptor");

    std::fs::write(control.join("ready"), b"").expect("signal ready");
    wait_for(&control.join("continue"));

    // The first request uses the dead session. The client must reconnect,
    // reopen both descriptors in one vector without replaying create/truncate
    // flags, preserve their numeric identities and cursors, and retry this
    // side-effect-free vector read.
    let result = fs
        .readv(&[
            ReadOp::new(files[0].clone(), VfOffset::Cur, 3),
            ReadOp::new(files[1].clone(), VfOffset::Cur, 4),
        ])
        .expect("read should recover after restart");
    assert_eq!(result[0].data, b"cde");
    assert_eq!(result[1].data, b"vwxy");
    assert_eq!(
        fs.stat(Path::new(&first_path))
            .expect("stat recovered file")
            .size,
        6
    );
    assert_eq!(
        fs.stat(Path::new(&second_path))
            .expect("stat recovered file")
            .size,
        6
    );

    fs.closev(&files).expect("close recovered descriptors");
    fs.removev(&[
        VfFile::from_path(&first_path),
        VfFile::from_path(&second_path),
    ])
    .expect("remove recovery files");
    fs.removev(&[VfFile::from_path(&dir)])
        .expect("remove recovery directory");
}
