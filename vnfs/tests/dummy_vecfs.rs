//! Tests for the `std::fs`-backed [`DummyVecFs`]. These need no NFS server:
//! the suite runs against a temporary directory, proving the `VecFs` API
//! works on non-NFS filesystems too.

mod common;

use vnfs::VecFs;
use vnfs::dummy_vecfs::DummyVecFs;

/// A `DummyVecFs` rooted at a fresh unique temp directory.
fn dummy() -> DummyVecFs {
    let root = std::env::temp_dir().join(format!(
        "vnfs_dummy_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    DummyVecFs::new(root)
}

#[test]
fn dummy_full_suite() {
    let mut fs = dummy();
    common::run_suite(&mut fs, "/");
}

#[test]
fn dummy_getcwd() {
    let mut fs = dummy();
    assert_eq!(fs.getcwd(), "/");
    fs.ensure_dir("/a/b", 0o755).unwrap();
    fs.chdir("/a/b").unwrap();
    assert_eq!(fs.getcwd(), "/a/b");
}

#[test]
fn dummy_write_read_roundtrip() {
    use vnfs::{VecFs, VfIoVec};

    let mut fs = dummy();
    fs.ensure_dir("/data", 0o755).unwrap();
    let payload = b"roundtrip content".to_vec();
    let mut w = VfIoVec::from_path("/data/f", 0, payload.len(), payload.clone());
    w.is_creation = true;
    fs.writev(std::slice::from_mut(&mut w)).unwrap();

    let mut r = VfIoVec::from_path("/data/f", 0, payload.len(), Vec::new());
    fs.readv(std::slice::from_mut(&mut r)).unwrap();
    assert_eq!(r.data, payload);
}

#[test]
fn dummy_errors_on_missing_file() {
    use vnfs::{VecFs, VfError, VfIoVec};

    let mut fs = dummy();
    fs.ensure_dir("/data", 0o755).unwrap();
    let mut r = VfIoVec::from_path("/data/missing", 0, 8, Vec::new());
    let res = fs.readv(std::slice::from_mut(&mut r));
    match res {
        Err(VfError { index, err_no }) => {
            assert_eq!(index, 0);
            assert!(r.is_failure);
            assert_eq!(err_no, 2, "ENOENT");
        }
        Ok(()) => panic!("readv of missing file must fail"),
    }
}

#[test]
fn dummy_stays_under_root() {
    // Absolute paths map inside the root, never to the real filesystem root.
    let name = format!("vnfs_root_{}", std::process::id());
    let root = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_dir_all(&root);
    let mut fs = DummyVecFs::new(root.clone());

    fs.ensure_dir(&format!("/{}", name), 0o755).unwrap();
    let inside_root = root.join(&name);
    assert!(inside_root.is_dir(), "created inside the root");

    let real = std::path::Path::new("/").join(&name);
    assert!(
        !real.exists(),
        "must not create anything under the real filesystem root"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&real);
}
