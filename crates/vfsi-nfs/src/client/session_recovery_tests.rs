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

#[cfg(all(test, feature = "test-faults"))]
mod unaddressed_open_tests {
    use super::super::*;
    use crate::compound::no_create_how;

    #[test]
    fn live_named_open_without_getfh_quarantines_identity_before_any_more_rpc() {
        let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let mut fixture = NfsClient::connect_minor(&host, minor).unwrap();
        let root = fixture.root().clone();
        let name = format!("unaddressed-open-{}-{minor}", std::process::id());
        let directory = fixture.mkdir(&root, &name).unwrap();
        let (known_fh, stateid) = fixture
            .open(
                &directory,
                b"file",
                OPEN4_SHARE_ACCESS_READ,
                OpenCreate::Unchecked,
            )
            .unwrap();
        fixture.close(&known_fh, &stateid).unwrap();

        let mut client = NfsClient::connect_minor(&host, minor).unwrap();
        // First prove an ordinary failed OPEN does not quarantine the identity.
        let mut failed = Compound::new();
        failed.putfh(&directory.as_nfs_fh());
        failed.open_claim_null(
            0,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
            client.session.clientid,
            b"missing-owner",
            no_create_how(),
            b"missing",
        );
        failed.getfh();
        let reply = client.call_compound(&mut failed).unwrap();
        assert_eq!(reply.status(), nfsstat4_NFS4ERR_NOENT);
        assert!(!client.needs_fresh_identity());
        client.ensure_usable().unwrap();

        // A later READ failure must keep the exact OPEN/GETFH cleanup
        // operand without unnecessarily quarantining a usable identity.
        let mut known = Compound::new();
        known.putfh(&directory.as_nfs_fh());
        known.open_claim_null(
            0,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
            client.session.clientid,
            b"known-owner",
            no_create_how(),
            b"file",
        );
        known.getfh();
        let invalid = stateid4 {
            seqid: 0,
            other: [255; 12],
        };
        known.read(&invalid, 0, 1);
        let reply = client.call_compound(&mut known).unwrap();
        assert_ne!(reply.status(), nfsstat4_NFS4_OK);
        assert_eq!(reply.op_status(3), nfsstat4_NFS4_OK);
        assert!(!client.needs_fresh_identity());
        let fh = NfsFh::from_nfs_fh(reply.getfh(3));
        client.close(&fh, &reply.open(2).stateid).unwrap();

        // CLAIM_FH already pins the operand; a known OPEN result needs no
        // extra GETFH even when a later protocol operation fails.
        let mut retained = Compound::new();
        retained.putfh(&known_fh.as_nfs_fh());
        retained.open_claim_fh(
            OPEN4_SHARE_ACCESS_READ,
            client.session.clientid,
            b"fh-owner",
        );
        retained.read(&invalid, 0, 1);
        let reply = client.call_compound(&mut retained).unwrap();
        assert_ne!(reply.status(), nfsstat4_NFS4_OK);
        assert_eq!(reply.op_status(2), nfsstat4_NFS4_OK);
        assert!(!client.needs_fresh_identity());
        client.close(&known_fh, &reply.open(2).stateid).unwrap();

        // The server acquires an actual OPEN, then fails before GETFH. LOOKUP
        // on this regular file deterministically produces NOTDIR.
        let mut compound = Compound::new();
        compound.putfh(&directory.as_nfs_fh());
        compound.open_claim_null(
            0,
            OPEN4_SHARE_ACCESS_READ,
            OPEN4_SHARE_DENY_NONE,
            client.session.clientid,
            b"unaddressed-owner",
            no_create_how(),
            b"file",
        );
        compound.lookup(b"child");
        compound.getfh();
        let reply = client.call_compound(&mut compound).unwrap();
        assert_eq!(reply.op_status(2), nfsstat4_NFS4_OK);
        assert_eq!(reply.status(), nfsstat4_NFS4ERR_NOTDIR);
        let acquired = reply.open(2).stateid;
        assert!(
            client.needs_fresh_identity(),
            "successful OPEN cannot lose its cleanup owner"
        );
        let _ = crate::compound::thread_compound_stats();
        assert!(
            client
                .lookup(&directory, b"file")
                .unwrap_err()
                .is_transport()
        );
        assert_eq!(
            crate::compound::thread_compound_stats().0,
            0,
            "a quarantined identity must not renew its lease with more work"
        );
        assert!(client.openv_path_compound(&[]).unwrap().is_empty());
        let rejection = client
            .readv_path_compound(
                &[PathReadOp {
                    file: FileRef::Path(b"file".to_vec()),
                    offset: 0,
                    count: 1,
                    stateid: None,
                }],
                true,
            )
            .err()
            .expect("quarantined READ is rejected before submission");
        assert!(rejection.is_transport());
        assert_eq!(crate::compound::thread_compound_stats().0, 0);

        assert!(client.getattr_path_compound(&[]).unwrap().is_empty());
        let rejection = client
            .getattr_path_compound(&[PathGetattrOp {
                file: FileRef::Handle(directory.clone()),
                attrs: vec![FATTR4_TYPE],
            }])
            .expect_err("quarantined GETATTR is rejected before submission");
        assert!(rejection.is_transport());
        assert_eq!(crate::compound::thread_compound_stats().0, 0);

        // Test-only teardown uses the exact FH pinned before this OPEN. The
        // production recovery path never reconstructs one through a pathname.
        let mut close = Compound::new();
        close.putfh(&known_fh.as_nfs_fh());
        close.close(0, &acquired);
        let closed = client.session.compound(&mut close).unwrap();
        assert_eq!(closed.status(), nfsstat4_NFS4_OK);
        client.shutdown().unwrap();
        fixture.remove(&directory, "file").unwrap();
        fixture.remove(&root, &name).unwrap();
        fixture.shutdown().unwrap();
    }
}

#[cfg(all(test, feature = "test-faults"))]
mod lost_open_identity_tests {
    use super::super::*;
    use crate::{compound::no_create_how, rpc::RpcClient};
    use std::{io::Read, net::TcpListener, time::Duration};

    #[test]
    fn lost_open_reply_quarantines_named_and_claim_fh_stateids() {
        let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        for named in [false, true] {
            let mut client = NfsClient::connect_minor(&host, minor).unwrap();
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let endpoint = listener.local_addr().unwrap().to_string();
            let receiver = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut received = Vec::new();
                loop {
                    let mut marker = [0; 4];
                    socket.read_exact(&mut marker).unwrap();
                    let marker = u32::from_be_bytes(marker);
                    let bytes = (marker & 0x7fff_ffff) as usize;
                    assert!(bytes < 4096 && received.len() + bytes < 4096);
                    let start = received.len();
                    received.resize(start + bytes, 0);
                    socket.read_exact(&mut received[start..]).unwrap();
                    if marker & 0x8000_0000 != 0 {
                        break;
                    }
                }
                // Verify an actual COMPOUND call, rather than a connect probe.
                assert!(received.len() >= 24);
                assert_eq!(
                    u32::from_be_bytes(received[20..24].try_into().unwrap()),
                    crate::rpc::NFSPROC4_COMPOUND
                );
                // The complete request was published; completion is unknowable
                // to the caller when this connection closes without a reply.
                received.len()
            });
            client.session.rpc = RpcClient::connect_with_timeouts(
                &endpoint,
                Duration::from_millis(500),
                Duration::from_millis(500),
            )
            .unwrap();
            let mut request = Compound::new();
            request.putfh(&client.root().as_nfs_fh());
            if named {
                request.open_claim_null(
                    0,
                    OPEN4_SHARE_ACCESS_READ,
                    OPEN4_SHARE_DENY_NONE,
                    client.session.clientid,
                    b"lost-owner",
                    no_create_how(),
                    b"file",
                );
                request.getfh();
            } else {
                request.open_claim_fh(
                    OPEN4_SHARE_ACCESS_READ,
                    client.session.clientid,
                    b"lost-fh-owner",
                );
            }
            let error = client
                .call_compound(&mut request)
                .err()
                .expect("reply was lost");
            assert!(error.is_transport());
            assert!(receiver.join().unwrap() > 0);
            assert!(
                client.needs_fresh_identity(),
                "even CLAIM_FH can lose its acquired stateid"
            );
            let _ = crate::compound::thread_compound_stats();
            assert!(client.call_compound(&mut Compound::new()).is_err());
            assert_eq!(crate::compound::thread_compound_stats().0, 0);
            // This test deliberately replaced the transport; do not attempt
            // server teardown through its already closed response stub.
            client.abandon();
        }
    }
}

#[cfg(all(test, feature = "test-faults"))]
mod replacement_quarantine_tests {
    use super::super::*;
    use std::time::Duration;

    #[test]
    fn live_failed_replacement_preserves_quarantine_only_for_its_shared_identity() {
        let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
            assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
            return;
        };
        let minor = std::env::var("VFSI_NFS_MINOR")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let owner = format!("failed-replacement-{}-{minor}", std::process::id());
        let verifier = crate::session::make_verifier();
        let connect = || {
            NfsClient::connect_minor_with_identity(
                &host,
                minor,
                Duration::from_secs(5),
                Duration::from_secs(5),
                &crate::rpc::NfsAuth::AuthSys,
                Some(owner.as_bytes()),
                Some(verifier),
            )
            .unwrap()
        };
        let mut retained = connect();
        let mut unrelated = NfsClient::connect_minor(&host, minor).unwrap();
        unrelated.quarantine_open_identity();
        retained.inherit_open_quarantine(&unrelated);
        assert!(
            !retained.needs_fresh_identity(),
            "another identity cannot quarantine this client"
        );
        unrelated.shutdown().unwrap();

        let mut attempted = connect();
        assert!(
            retained.session.shares_identity(&attempted.session),
            "fixture must really reuse the explicit client identity"
        );
        attempted.quarantine_open_identity();
        retained.inherit_open_quarantine(&attempted);
        assert!(
            retained.needs_fresh_identity(),
            "a discarded replacement must not erase unknown OPEN ownership"
        );
        attempted.shutdown().unwrap();
        retained.shutdown().unwrap();
    }
}
