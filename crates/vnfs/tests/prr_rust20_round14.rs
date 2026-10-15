#![cfg(all(feature = "posix", feature = "test-faults"))]

use std::{path::Path, sync::Arc};
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_sync::backend::HandleBackend;

#[test]
fn creation_permission_fault_releases_the_unregistered_descriptor() {
    for flags in [
        libc::O_CREAT | libc::O_WRONLY,
        libc::O_CREAT | libc::O_RDWR,
        libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut backend = vfsi_posix::backend(root.path()).unwrap();
        let script = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeSetPermissions { index: 0 },
            vnfs::Error::failure(0, libc::EPERM as u32),
        ));
        backend.set_fault_injector(script.clone());
        let error = backend
            .open_raw_impl(Path::new("/created"), flags, 0o600)
            .unwrap_err();
        assert_eq!(error.err_no(), libc::EPERM as u32);
        assert!(script.is_consumed());
        assert_eq!(backend.test_open_handle_count(), 0);
        assert!(root.path().join("created").is_file());
        // A later OPEN still succeeds; no live handle or permission retry is hidden.
        let file = backend
            .open_raw_impl(Path::new("/created"), libc::O_RDONLY, 0)
            .unwrap();
        backend.close_impl(&file).unwrap();
        assert_eq!(backend.test_open_handle_count(), 0);
    }
}

#[test]
fn opening_an_existing_file_does_not_inject_creation_permission_failure() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("existing"), b"preserved").unwrap();
    let mut backend = vfsi_posix::backend(root.path()).unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeSetPermissions { index: 0 },
        vnfs::Error::failure(0, libc::EPERM as u32),
    ));
    backend.set_fault_injector(script.clone());
    let file = backend
        .open_raw_impl(Path::new("/existing"), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    assert!(!script.is_consumed());
    backend.close_impl(&file).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("existing")).unwrap(),
        b"preserved"
    );
    assert_eq!(backend.test_open_handle_count(), 0);
}

#[test]
fn failed_lookup_does_not_consume_creation_permission_fault() {
    let root = tempfile::tempdir().unwrap();
    let mut backend = vfsi_posix::backend(root.path()).unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeSetPermissions { index: 0 },
        vnfs::Error::failure(0, libc::EPERM as u32),
    ));
    backend.set_fault_injector(script.clone());
    let error = backend
        .open_raw_impl(Path::new("/missing"), libc::O_RDONLY, 0)
        .unwrap_err();
    assert_eq!(error.err_no(), libc::ENOENT as u32);
    assert!(!script.is_consumed());
    assert_eq!(backend.test_open_handle_count(), 0);
}
