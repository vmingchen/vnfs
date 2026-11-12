use super::*;
use crate::rpc::{
    RpcClient,
    test_support::{accepted_reply, read_record},
};
use std::{net::TcpListener, thread, time::Duration};
use vfsi_core::{FailureState, StatusCode};

fn live_client() -> Option<NfsClient> {
    let Ok(host) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    Some(NfsClient::connect_minor(&host, minor).unwrap())
}

/// Echo the submitted SEQUENCE, then the protocol's synthetic uncached-reply
/// failure on PUTFH. The original compound's actual results are unavailable.
fn uncached_peer(client: &mut NfsClient) -> thread::JoinHandle<usize> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let endpoint = listener.local_addr().unwrap().to_string();
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(700)))
            .unwrap();
        let mut count = 0;
        while let Ok(request) = read_record(&mut socket) {
            count += 1;
            let word = |at| u32::from_be_bytes(request[at..at + 4].try_into().unwrap());
            let mut at = 24; // fixed RPC CALL header
            for _ in 0..2 {
                // credentials and verifier
                at += 8 + (word(at + 4) as usize).div_ceil(4) * 4;
            }
            at += 4 + (word(at) as usize).div_ceil(4) * 4; // COMPOUND tag
            at += 8; // minor version, operation count
            assert_eq!(word(at), nfs_opnum4_NFS4_OP_SEQUENCE);
            let sequence = &request[at + 4..at + 28]; // session ID, sequence, slot
            let mut payload = Vec::new();
            for value in [
                nfsstat4_NFS4ERR_RETRY_UNCACHED_REP,
                0,
                2,
                nfs_opnum4_NFS4_OP_SEQUENCE,
                nfsstat4_NFS4_OK,
            ] {
                payload.extend_from_slice(&value.to_be_bytes());
            }
            payload.extend_from_slice(sequence);
            for value in [
                0u32,
                0,
                0,
                nfs_opnum4_NFS4_OP_PUTFH,
                nfsstat4_NFS4ERR_RETRY_UNCACHED_REP,
            ] {
                payload.extend_from_slice(&value.to_be_bytes());
            }
            accepted_reply(&mut socket, &request, &payload);
        }
        count
    });
    client.session.rpc = RpcClient::connect_with_timeouts(
        &endpoint,
        Duration::from_secs(1),
        Duration::from_millis(300),
    )
    .unwrap();
    peer
}

#[test]
fn uncached_reply_marks_every_submitted_mutation_unknown_without_suffix_replay() {
    let Some(mut client) = live_client() else {
        return;
    };
    let peer = uncached_peer(&mut client);
    let root = client.root().clone();
    client.max_ops = 5; // two PUTFH/REMOVE inputs in the first wave
    let names: [&[u8]; 3] = [b"first", b"second", b"later-wave"];
    let outcomes = client
        .fixed_results::<2, _, _>(
            b"removev",
            &names,
            4096,
            |name| name.len(),
            |compound, name, _| {
                compound.putfh(&root.as_nfs_fh());
                compound.remove(name);
            },
            |_, _, _| Ok(()),
        )
        .unwrap();
    client.abandon();
    drop(client);
    let transmissions = peer.join().unwrap();
    assert_eq!(outcomes.len(), 3);
    for (index, result) in outcomes.iter().enumerate() {
        let error = result.as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if index < 2 {
                FailureState::Unknown
            } else {
                FailureState::NotAttempted
            }
        );
        assert_eq!(
            error.cause().status(),
            Some(StatusCode::Nfs(nfsstat4_NFS4ERR_RETRY_UNCACHED_REP))
        );
        assert!(!error.cause().is_transport(), "retain the native NFS cause");
    }
    assert_eq!(
        transmissions, 1,
        "never dispatch a suffix whose original effects are hidden"
    );
}

#[test]
fn uncached_reply_quarantines_every_possibly_acquired_open_owner() {
    for named in [false, true] {
        let Some(mut client) = live_client() else {
            return;
        };
        let peer = uncached_peer(&mut client);
        let root = client.root().clone();
        let mut request = Compound::new();
        request.putfh(&root.as_nfs_fh());
        if named {
            request.open_claim_null(
                0,
                OPEN4_SHARE_ACCESS_READ,
                OPEN4_SHARE_DENY_NONE,
                client.session.clientid,
                b"uncached-owner",
                crate::compound::no_create_how(),
                b"file",
            );
            request.getfh();
        } else {
            request.open_claim_fh(
                OPEN4_SHARE_ACCESS_READ,
                client.session.clientid,
                b"uncached-owner",
            );
        }
        let error = client
            .call_compound(&mut request)
            .err()
            .expect("hidden OPEN result cannot succeed");
        assert_eq!(error.status, nfsstat4_NFS4ERR_RETRY_UNCACHED_REP);
        assert!(client.needs_fresh_identity());
        assert!(
            client
                .getattr_path_compound(&[PathGetattrOp {
                    file: FileRef::Handle(root),
                    attrs: vec![FATTR4_TYPE],
                }])
                .is_err(),
            "do not renew the quarantined identity"
        );
        client.abandon();
        drop(client);
        assert_eq!(peer.join().unwrap(), 1);
    }
}

#[test]
fn uncached_status_is_uncertain_for_short_write_repair() {
    let error = io_progress::write_rpc_error(RpcError::op(1, nfsstat4_NFS4ERR_RETRY_UNCACHED_REP));
    assert_eq!(error.state(), FailureState::Unknown);
    assert_eq!(
        error.cause().status(),
        Some(StatusCode::Nfs(nfsstat4_NFS4ERR_RETRY_UNCACHED_REP))
    );
    assert!(!uncertain_completion(&vfsi_core::VfError::nfs(
        None,
        nfsstat4_NFS4ERR_ACCESS
    )));
}

#[test]
fn uncached_reply_keeps_merged_read_and_write_inputs_unknown() {
    for write in [false, true] {
        let Some(mut client) = live_client() else {
            return;
        };
        let peer = uncached_peer(&mut client);
        let root = client.root().clone();
        let errors: Vec<_> = if write {
            let ops: Vec<_> = (0..2)
                .map(|index| PathWriteOp {
                    file: FileRef::Handle(root.clone()),
                    offset: index,
                    data: b"x",
                    create: false,
                    truncate: false,
                    stateid: None,
                })
                .collect();
            client
                .writev_path_compound(&ops, false)
                .unwrap()
                .results
                .into_iter()
                .map(|r| r.unwrap_err())
                .collect()
        } else {
            let ops: Vec<_> = (0..2)
                .map(|_| PathReadOp {
                    file: FileRef::Handle(root.clone()),
                    offset: 0,
                    count: 1,
                    stateid: None,
                })
                .collect();
            client
                .readv_path_compound(&ops, false)
                .unwrap()
                .results
                .into_iter()
                .map(|r| r.unwrap_err())
                .collect()
        };
        client.abandon();
        drop(client);
        assert_eq!(peer.join().unwrap(), 1);
        assert_eq!(errors.len(), 2);
        for error in errors {
            assert_eq!(error.state(), FailureState::Unknown);
            assert_eq!(
                error.cause().status(),
                Some(StatusCode::Nfs(nfsstat4_NFS4ERR_RETRY_UNCACHED_REP))
            );
        }
    }
}

#[test]
fn live_scalar_write_rejects_invalid_count_and_stability() {
    for count in [0u32, 2, 3, 4] {
        for committed in [0u32, 1, 2, 3] {
            let Some(mut client) = live_client() else {
                return;
            };
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let peer = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_record(&mut socket).unwrap();
                let word = |at| u32::from_be_bytes(request[at..at + 4].try_into().unwrap());
                let mut at = 24;
                for _ in 0..2 {
                    at += 8 + (word(at + 4) as usize).div_ceil(4) * 4;
                }
                at += 4 + (word(at) as usize).div_ceil(4) * 4 + 8;
                assert_eq!(word(at), nfs_opnum4_NFS4_OP_SEQUENCE);
                let mut payload = Vec::new();
                for word in [0u32, 0, 3, nfs_opnum4_NFS4_OP_SEQUENCE, 0] {
                    payload.extend_from_slice(&word.to_be_bytes());
                }
                payload.extend_from_slice(&request[at + 4..at + 28]);
                for word in [
                    0u32,
                    0,
                    0,
                    nfs_opnum4_NFS4_OP_PUTFH,
                    0,
                    nfs_opnum4_NFS4_OP_WRITE,
                    0,
                    count,
                    committed,
                    0,
                    0,
                ] {
                    payload.extend_from_slice(&word.to_be_bytes());
                }
                accepted_reply(&mut socket, &request, &payload);
            });
            let rpc = RpcClient::connect_with_timeouts(
                &address.to_string(),
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .unwrap();
            let original = std::mem::replace(&mut client.session.rpc, rpc);
            let result = client.write(
                &client.root().clone(),
                &stateid4 {
                    seqid: 0,
                    other: [0; 12],
                },
                0,
                b"abc",
            );
            client.session.rpc = original;
            peer.join().unwrap();
            if count <= 3 && committed == stable_how4_FILE_SYNC4 {
                assert_eq!(result.unwrap(), (count, committed));
            } else {
                assert_eq!(
                    result.unwrap_err().transport_kind,
                    Some(vfsi_core::TransportKind::InvalidReply)
                );
            }
        }
    }
}
