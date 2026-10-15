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
}
