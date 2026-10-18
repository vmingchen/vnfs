//! Stable client owners must support connection replacement without losing I/O.
use std::path::Path;
use vfsi_nfs::{NfsRecoveryPolicy, NfsVecFs};
use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::{ReadOp, VfOffset};

#[test]
fn stable_owner_reconnect_preserves_independent_opens_and_shutdown() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let mut fs =
        NfsVecFs::builder(&server)
            .minor_version(Some(minor))
            .client_owner(format!("prr-stable-owner-{}-{minor}", std::process::id()))
            .recovery_policy(NfsRecoveryPolicy::new().with_reconnect_attempts(
                std::num::NonZeroUsize::new(1).expect("nonzero tuning value"),
            ))
            .connect()
            .unwrap();
    let path = format!("/prr-stable-owner-{}-{minor}", std::process::id());
    let setup = fs
        .open_raw_impl(
            Path::new(&path),
            libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
            0o600,
        )
        .unwrap();
    fs.write_raw_impl(&setup, 0, b"abcdef").unwrap();
    fs.close_impl(&setup).unwrap();
    let first = fs.open_raw_impl(Path::new(&path), libc::O_RDWR, 0).unwrap();
    let second = fs
        .open_raw_impl(Path::new(&path), libc::O_RDONLY, 0)
        .unwrap();
    fs.seek_raw_impl(&first, 2, vfsi_sync::SeekFrom::Set)
        .unwrap();
    let mut failure = None;
    for expected in [b"c", b"d", b"e"] {
        if let Err(error) = fs.reconnect() {
            failure = Some(error);
            break;
        }
        assert_eq!(
            fs.vread_impl(&[ReadOp::new(first.clone(), VfOffset::Cur, 1)])
                .unwrap()[0]
                .data,
            expected
        );
    }
    let close_first = fs.close_impl(&first);
    let read_second = fs.read_raw_impl(&second, 0, 6);
    let close_second = fs.close_impl(&second);
    fs.remove_paths_impl(&[Path::new(&path)], true).unwrap();
    let shutdown = fs.shutdown();
    assert!(
        failure.is_none(),
        "stable-owner reconnect failed: {failure:?}"
    );
    close_first.unwrap();
    assert_eq!(read_second.unwrap(), b"abcdef");
    close_second.unwrap();
    shutdown.unwrap();
}

#[test]
fn failed_stable_owner_reconnect_keeps_original_descriptors_usable() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let mut fs =
        NfsVecFs::builder(&server)
            .minor_version(Some(minor))
            .client_owner(format!("prr-owner-failure-{}-{minor}", std::process::id()))
            .recovery_policy(NfsRecoveryPolicy::new().with_reconnect_attempts(
                std::num::NonZeroUsize::new(1).expect("nonzero tuning value"),
            ))
            .connect()
            .unwrap();
    let path = format!("/prr-owner-failure-{}-{minor}", std::process::id());
    let file = fs
        .open_raw_impl(
            Path::new(&path),
            libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
            0o600,
        )
        .unwrap();
    fs.write_raw_impl(&file, 0, b"retained").unwrap();
    fs.set_max_compound_bytes(1);
    let failed = fs.reconnect();
    fs.set_max_compound_bytes(4096);
    fs.set_auto_reconnect(false);
    let read = fs.read_raw_impl(&file, 0, 8);
    let close = fs.close_impl(&file);
    fs.remove_paths_impl(&[Path::new(&path)], true).unwrap();
    let shutdown = fs.shutdown();
    assert!(
        failed.is_err(),
        "reopen must fail under an impossible request cap"
    );
    assert_eq!(read.unwrap(), b"retained");
    close.unwrap();
    shutdown.unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn reconnect_cleanup_failure_is_retained_and_retried_at_shutdown() {
    use std::sync::Arc;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let mut fs =
        NfsVecFs::builder(&server)
            .minor_version(Some(minor))
            .client_owner(format!("prr-owner-cleanup-{}-{minor}", std::process::id()))
            .recovery_policy(NfsRecoveryPolicy::new().with_reconnect_attempts(
                std::num::NonZeroUsize::new(1).expect("nonzero tuning value"),
            ))
            .connect()
            .unwrap();
    let path = format!("/prr-owner-cleanup-{}-{minor}", std::process::id());
    let file = fs
        .open_raw_impl(
            Path::new(&path),
            libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
            0o600,
        )
        .unwrap();
    fs.write_raw_impl(&file, 0, b"alive").unwrap();
    let script = Arc::new(FaultScript::new([(
        OpenFaultPoint::BeforePathCloseBatch,
        vfsi_core::VfError::transport(None, "obsolete state cleanup failure"),
    )]));
    fs.set_fault_injector(script.clone());
    fs.reconnect().unwrap();
    assert!(script.is_consumed());
    assert_eq!(fs.test_deferred_path_close_count(), 1);
    assert_eq!(fs.read_raw_impl(&file, 0, 5).unwrap(), b"alive");
    fs.close_impl(&file).unwrap();
    fs.remove_paths_impl(&[Path::new(&path)], true).unwrap();
    fs.shutdown()
        .expect("shutdown must retry the obsolete OPEN cleanup");
}
