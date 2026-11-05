#[cfg(all(test, feature = "test-faults"))]
mod live_shutdown_tests {
    use super::super::CloseOp;
    use super::super::{NfsClient, OpenCreate};
    use std::sync::Arc;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};

    #[test]
    fn shutdown_drains_failed_unregistered_open_cleanup() {
        let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let mut client = NfsClient::connect_minor(&host, minor).unwrap();
        let root = client.root().clone();
        let name = format!("prr-shutdown-deferred-{}-{minor}", std::process::id());
        let dir = client.mkdir(&root, &name).unwrap();
        let (fh, stateid) = client
            .open(
                &dir,
                b"file",
                nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
                OpenCreate::Unchecked,
            )
            .unwrap();
        let script = Arc::new(FaultScript::new([(
            OpenFaultPoint::BeforePathCloseBatch,
            vfsi_core::VfError::transport(None, "defer cleanup"),
        )]));
        client.set_fault_injector(script.clone());
        client.close_path_or_defer(vec![CloseOp { fh, stateid }]);
        assert!(script.is_consumed());
        assert_eq!(client.deferred_path_close_count(), 1);
        client.remove(&dir, "file").unwrap();
        client.remove(&root, &name).unwrap();
        let result = client.shutdown();
        assert_eq!(
            client.deferred_path_close_count(),
            0,
            "shutdown retains responsibility for deferred OPENs"
        );
        result.unwrap();
    }

    #[test]
    fn deferred_cleanup_retains_only_the_failed_close_and_unattempted_suffix() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use vfsi_core::internal::faults::FaultInjector;

        struct FailSecondClose(AtomicUsize);
        impl FaultInjector for FailSecondClose {
            fn check(&self, point: &OpenFaultPoint) -> vfsi_core::VfRes {
                if matches!(point, OpenFaultPoint::BeforePathClose)
                    && self.0.fetch_add(1, Ordering::SeqCst) == 1
                {
                    return Err(vfsi_core::VfError::transport(None, "second CLOSE failed"));
                }
                Ok(())
            }
        }
        let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let mut client = NfsClient::connect_minor(&host, minor).unwrap();
        let root = client.root().clone();
        let name = format!("file-review-close-prefix-{}-{minor}", std::process::id());
        let dir = client.mkdir(&root, &name).unwrap();
        let mut opens = Vec::new();
        for name in [b"first".as_slice(), b"second", b"third"] {
            let (fh, stateid) = client
                .open(
                    &dir,
                    name,
                    nfsv41_sys::OPEN4_SHARE_ACCESS_READ,
                    OpenCreate::Unchecked,
                )
                .unwrap();
            opens.push(CloseOp { fh, stateid });
        }
        let defer = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforePathCloseBatch,
            vfsi_core::VfError::transport(None, "queue deferred closes"),
        ));
        client.set_fault_injector(defer.clone());
        client.close_path_or_defer(opens);
        assert!(defer.is_consumed());
        let expected: Vec<_> = client.deferred_path_closes[1..]
            .iter()
            .map(|close| close.fh.clone())
            .collect();
        let fault = Arc::new(FailSecondClose(AtomicUsize::new(0)));
        client.set_fault_injector(fault.clone());
        let before = client.confirmed_path_closes();
        let first = client.drain_deferred_path_closes();
        let queued: Vec<_> = client
            .deferred_path_closes
            .iter()
            .map(|close| close.fh.clone())
            .collect();
        let completed = client.confirmed_path_closes() - before;
        let retry = client.drain_deferred_path_closes();
        let total = client.confirmed_path_closes() - before;
        let remaining = client.deferred_path_close_count();
        for name in ["first", "second", "third"] {
            client.remove(&dir, name).unwrap();
        }
        client.remove(&root, &name).unwrap();
        client.shutdown().unwrap();

        assert!(first.unwrap_err().is_transport());
        assert_eq!(completed, 1);
        assert_eq!(
            queued, expected,
            "retain original order without replaying the closed prefix"
        );
        retry.unwrap();
        assert_eq!(total, 3);
        assert_eq!(remaining, 0);
        assert_eq!(fault.0.load(Ordering::SeqCst), 4);
    }
}
