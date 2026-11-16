use super::*;
use std::sync::Arc;
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_core::{FailureState, OpError};

fn fixture(label: &str) -> Option<(NfsClient, NfsFh, NfsFh, String, Vec<NfsFh>)> {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut client = NfsClient::connect_minor(&server, minor).unwrap();
    let root = client.root().clone();
    let name = format!("vfsi-readlink-{label}-{}", std::process::id());
    let dir = client.mkdir(&root, &name).unwrap();
    let handles = (0..5)
        .map(|index| {
            client
                .symlink(
                    &dir,
                    &format!("link-{index}"),
                    &format!("../target-{index}"),
                )
                .unwrap()
        })
        .collect();
    Some((client, root, dir, name, handles))
}
fn cleanup(client: &mut NfsClient, root: &NfsFh, dir: &NfsFh, name: &str) {
    for index in 0..5 {
        client.remove(dir, &format!("link-{index}")).unwrap();
    }
    client.remove(root, name).unwrap();
}
fn assert_partition<T: std::fmt::Debug>(results: &[Result<T, OpError>]) {
    assert_eq!(results.len(), 5);
    for result in &results[..2] {
        assert!(result.is_ok());
    }
    for result in &results[2..4] {
        let error = result.as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Unknown);
        let _: &std::io::Error = error.cause();
        assert!(error.progress().is_none());
    }
    assert_eq!(
        results[4].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
}

#[test]
fn raw_readlink_preserves_known_results_across_status_failure_and_reply_loss() {
    let Some((mut client, root, dir, name, handles)) = fixture("wire") else {
        return;
    };
    let mut ops: Vec<_> = handles
        .iter()
        .map(|fh| ReadlinkOp { fh: fh.clone() })
        .collect();
    ops[2].fh = dir.clone(); // A real server-side READLINK/INVAL, not a missing path.
    let _ = crate::compound::thread_compound_stats();
    let results = client.readlink_many(&ops).unwrap();
    for (index, result) in results.iter().enumerate() {
        if index == 2 {
            let error = result.as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::Failed);
            assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
        } else {
            assert_eq!(
                result.as_ref().unwrap(),
                format!("../target-{index}").as_bytes()
            );
        }
    }
    assert_eq!(
        crate::compound::thread_compound_stats().0,
        2,
        "completed READLINKs are not replayed after the failed operation"
    );
    ops[2].fh = handles[2].clone();
    let max_ops = client.max_ops;
    client.max_ops = 5; // Two input pairs per RPC.
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterReadlinkCompoundReply { first: 2 },
        vfsi_core::internal::errors::transport(
            vfsi_core::TransportKind::Other,
            "second READLINK reply lost",
        ),
    ));
    client.set_fault_injector(fault.clone());
    let _ = crate::compound::thread_compound_stats();
    assert_partition(&client.readlink_many(&ops).unwrap());
    assert!(fault.is_consumed());
    assert_eq!(crate::compound::thread_compound_stats().0, 2);
    client.max_ops = 1;
    let _ = crate::compound::thread_compound_stats();
    assert!(client.readlink_many(&ops).is_err());
    assert_eq!(crate::compound::thread_compound_stats().0, 0);
    assert!(client.readlink_many(&[]).unwrap().is_empty());
    client.max_ops = max_ops;
    cleanup(&mut client, &root, &dir, &name);
}

#[test]
fn child_lookup_preserves_known_results_across_status_failure_and_reply_loss() {
    let Some((mut client, root, dir, name, _)) = fixture("lookup") else {
        return;
    };
    let mut ops: Vec<_> = (0..5)
        .map(|index| (dir.clone(), format!("link-{index}").into_bytes()))
        .collect();
    ops[2].1 = b"missing".to_vec();
    let _ = crate::compound::thread_compound_stats();
    let results = client.lookup_getattr_many(&ops).unwrap();
    for (index, result) in results.iter().enumerate() {
        if index == 2 {
            let error = result.as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::Failed);
            assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
        } else {
            assert_eq!(result.as_ref().unwrap().1, nfs_ftype4_NF4LNK);
        }
    }
    assert_eq!(crate::compound::thread_compound_stats().0, 2);
    ops[2].1 = b"link-2".to_vec();
    let max_ops = client.max_ops;
    client.max_ops = 9; // Two four-op child lookups per RPC.
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterLookupChunkReply { first: 2 },
        vfsi_core::internal::errors::transport(
            vfsi_core::TransportKind::Other,
            "second child-lookup reply lost",
        ),
    ));
    client.set_fault_injector(fault.clone());
    let _ = crate::compound::thread_compound_stats();
    assert_partition(&client.lookup_getattr_many(&ops).unwrap());
    assert!(fault.is_consumed());
    assert_eq!(crate::compound::thread_compound_stats().0, 2);
    client.max_ops = 1;
    let _ = crate::compound::thread_compound_stats();
    assert!(client.lookup_getattr_many(&ops).is_err());
    assert_eq!(crate::compound::thread_compound_stats().0, 0);
    assert!(client.lookup_getattr_many(&[]).unwrap().is_empty());
    client.max_ops = max_ops;
    cleanup(&mut client, &root, &dir, &name);
}

#[test]
fn live_lookup_type_rejects_short_and_trailing_attribute_bytes() {
    use crate::rpc::{
        RpcClient,
        test_support::{accepted_reply, read_record, successful_sequence_reply},
    };
    use std::{net::TcpListener, time::Duration};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for scalar in [true, false] {
        for length in [0usize, 3, 4, 8] {
            let mut client = NfsClient::connect_minor(&server, minor).unwrap();
            let root = client.root().clone();
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let peer = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_record(&mut socket).unwrap();
                let mut payload = successful_sequence_reply(&request, 5);
                for word in [
                    nfs_opnum4_NFS4_OP_PUTFH,
                    0,
                    nfs_opnum4_NFS4_OP_LOOKUP,
                    0,
                    nfs_opnum4_NFS4_OP_GETFH,
                    0,
                    4,
                ] {
                    payload.extend_from_slice(&word.to_be_bytes());
                }
                payload.extend_from_slice(b"file");
                for word in [
                    nfs_opnum4_NFS4_OP_GETATTR,
                    0,
                    1,
                    1 << FATTR4_TYPE,
                    length as u32,
                ] {
                    payload.extend_from_slice(&word.to_be_bytes());
                }
                let mut attrs = nfs_ftype4_NF4REG.to_be_bytes().to_vec();
                attrs.resize(length, 0);
                payload.extend_from_slice(&attrs);
                payload.resize(payload.len() + (4 - length % 4) % 4, 0);
                accepted_reply(&mut socket, &request, &payload);
            });
            let rpc = RpcClient::connect_with_timeouts(
                &address.to_string(),
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .unwrap();
            let original = client.replace_test_rpc(rpc);
            let result = if scalar {
                client
                    .lookup_getattr(&root, b"file")
                    .map_err(vfsi_core::internal::errors::from_rpc)
            } else {
                client
                    .lookup_getattr_many(&[(root, b"file".to_vec())])
                    .unwrap()
                    .remove(0)
                    .map_err(OpError::into_cause)
            };
            client.replace_test_rpc(original);
            peer.join().unwrap();
            client.shutdown().unwrap();
            if length == 4 {
                let (fh, kind) = result.unwrap();
                assert_eq!(fh.as_bytes(), b"file");
                assert_eq!(kind, nfs_ftype4_NF4REG);
            } else {
                let error = result.expect_err("TYPE must occupy exactly four bytes");
                assert_eq!(
                    vfsi_core::transport_kind(&error),
                    Some(vfsi_core::TransportKind::InvalidReply),
                    "scalar={scalar}, length={length}: {error:?}",
                );
            }
        }
    }
}
