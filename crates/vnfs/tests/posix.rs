#![cfg(all(feature = "posix", unix))]
use vnfs::files::{ReadOp, Vfsi, VfsiExt};
use vnfs::posix::Posix;

// Run this target with only `posix`: neither Auto nor NFS may supply the facade.
#[test]
fn standalone_posix_facade_is_rooted_and_rejects_missing_roots() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), b"alpha").unwrap();
    let fs = Posix::new(root.path()).unwrap();
    let results = fs.vread([ReadOp::whole("/a")], Default::default()).unwrap();
    assert_eq!(results[0].data(), Some(b"alpha".as_slice()));
    assert_eq!(fs.read("/a").unwrap(), b"alpha");
    let missing = root.path().join("missing");
    assert!(Posix::new(&missing).is_err());
    assert!(!missing.exists());
}
