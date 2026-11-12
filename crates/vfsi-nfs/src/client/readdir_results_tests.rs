use super::*;
use std::sync::Arc;
use vfsi_core::{
    FailureState, VfError,
    internal::faults::{FaultScript, OpenFaultPoint},
};

#[test]
fn readdir_vectors_preserve_pages_statuses_and_exact_submission_boundaries() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    for children in [false, true] {
        for phase in [
            "healthy",
            "status",
            "before",
            "lost",
            "resource",
            "request_bytes",
        ] {
            let mut client = NfsClient::connect_minor(&server, 1).unwrap();
            let root = client.root().clone();
            let name = format!("readdir-results-{}-{children}-{phase}", std::process::id());
            let directory = client.mkdir(&root, &name).unwrap();
            let mut directories = Vec::new();
            let mut regular = None;
            for index in 0..5 {
                let dir = client.mkdir(&directory, &format!("d{index}")).unwrap();
                let (fh, state) = client
                    .open(
                        &dir,
                        b"leaf",
                        OPEN4_SHARE_ACCESS_BOTH,
                        OpenCreate::Exclusive,
                    )
                    .unwrap();
                client.close(&fh, &state).unwrap();
                regular = Some(fh);
                directories.push(dir);
            }
            let mut names: Vec<_> = (0..5)
                .map(|index| format!("d{index}").into_bytes())
                .collect();
            let mut handles = directories.clone();
            if phase == "status" {
                names[1] = b"missing".to_vec();
                handles[1] = regular.unwrap();
            }
            let original_ops = client.max_ops;
            let original_bytes = client.max_compound_bytes;
            let point = match phase {
                "before" => Some(OpenFaultPoint::BeforeReadDirCompound {
                    children,
                    first: 2,
                    count: 2,
                }),
                "lost" => Some(OpenFaultPoint::AfterReadDirCompoundReply {
                    children,
                    first: 2,
                    count: 2,
                }),
                _ => None,
            };
            let fault = point.map(|point| {
                Arc::new(FaultScript::one(
                    point,
                    VfError::transport(None, "selected READDIR response unavailable"),
                ))
            });
            if let Some(fault) = &fault {
                client.max_ops = if children { 9 } else { 5 };
                client.set_fault_injector(fault.clone());
            }
            if phase == "resource" {
                client.inject_resource_rejection_once(if children {
                    b"readdir_children"
                } else {
                    b"readdir_pages"
                });
            }
            if phase == "request_bytes" {
                client.max_compound_bytes = client.rpc_envelope_reserve + 512;
            }
            let _ = crate::compound::thread_compound_stats();
            let pages = if children {
                let ops: Vec<_> = names
                    .into_iter()
                    .map(|name| (directory.clone(), name))
                    .collect();
                client
                    .readdir_children_bounded(&ops, &[FATTR4_TYPE], 4096)
                    .unwrap()
                    .into_iter()
                    .enumerate()
                    .map(|(index, result)| {
                        result.map(|page| {
                            assert_eq!(page.fh, directories[index]);
                            (page.entries, page.cookie)
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                let ops: Vec<_> = handles
                    .into_iter()
                    .map(|fh| (fh, DirCookie::default()))
                    .collect();
                client
                    .readdir_pages_bounded(&ops, &[FATTR4_TYPE], 4096)
                    .unwrap()
            };
            let requests = crate::compound::thread_compound_stats().0;
            assert_eq!(pages.len(), 5);
            for (index, result) in pages.into_iter().enumerate() {
                let expected = match phase {
                    "status" if index == 1 => Some(FailureState::Failed),
                    "before" if index >= 2 => Some(FailureState::NotAttempted),
                    "lost" if index == 2 || index == 3 => Some(FailureState::Unknown),
                    "lost" if index == 4 => Some(FailureState::NotAttempted),
                    _ => None,
                };
                if let Some(expected) = expected {
                    let error = result.unwrap_err();
                    assert_eq!(error.state(), expected, "{children}/{phase}/{index}");
                    assert_eq!(error.cause().index(), None);
                    assert!(error.progress().is_none());
                } else {
                    let (entries, cookie) = result.unwrap();
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].name, b"leaf");
                    assert_eq!(entries[0].attrs.len(), 4);
                    assert_eq!(cookie.value, 0);
                }
            }
            match phase {
                "healthy" => assert_eq!(requests, 1, "healthy READDIR must remain batched"),
                "status" | "lost" => assert_eq!(requests, 2, "no completed input replay"),
                "before" => assert_eq!(requests, 1, "the rejected wave was never sent"),
                "resource" => assert!(!client.resource_rejection_pending()),
                "request_bytes" => assert!((1..=5).contains(&requests)),
                _ => unreachable!(),
            }
            if let Some(fault) = fault {
                assert!(fault.is_consumed());
            }
            client.max_ops = original_ops;
            client.max_compound_bytes = original_bytes;
            for (index, dir) in directories.iter().enumerate() {
                client.remove(dir, "leaf").unwrap();
                client.remove(&directory, &format!("d{index}")).unwrap();
            }
            client.remove(&root, &name).unwrap();
        }
    }
}

#[test]
fn readdir_empty_and_whole_vector_rejection_do_not_dispatch() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut client = NfsClient::connect_minor(&server, 1).unwrap();
    let root = client.root().clone();
    let _ = crate::compound::thread_compound_stats();
    assert!(
        client
            .readdir_pages(&[], &[FATTR4_TYPE])
            .unwrap()
            .is_empty()
    );
    assert!(
        client
            .readdir_children(&[], &[FATTR4_TYPE])
            .unwrap()
            .is_empty()
    );
    client.max_ops = 2;
    assert!(
        client
            .readdir_pages(&[(root.clone(), DirCookie::default())], &[FATTR4_TYPE])
            .is_err()
    );
    assert!(
        client
            .readdir_children(&[(root, b"unused".to_vec())], &[FATTR4_TYPE])
            .is_err()
    );
    assert_eq!(crate::compound::thread_compound_stats().0, 0);
}

#[test]
fn readdir_response_budgets_follow_each_actual_wire_wave() {
    assert_eq!(readdir_response_limits(65536, 4, 32768), (4096, 16384));
    assert_eq!(readdir_response_limits(65536, 2, 32768), (8192, 32768));
    assert_eq!(readdir_response_limits(65536, 1, 32768), (8192, 32768));
    assert_eq!(readdir_response_limits(1024, 1, 4096), (256, 1024));
}

#[test]
fn filtered_pages_follow_verifiers_and_bound_continuations() {
    use crate::rpc::{RpcClient, test_support::readdir_filtered_peer};
    use std::{net::TcpListener, time::Duration};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for mode in ["scalar", "vector", "child"] {
        for outcome in [
            "success",
            "cycle",
            "status",
            "advancing-limit",
            "advancing-visible",
            "advancing-eof",
        ] {
            let mut client = NfsClient::connect_minor(&server, minor).unwrap();
            let root = client.root().clone();
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let peer = readdir_filtered_peer(listener, outcome);
            let rpc = RpcClient::connect_with_timeouts(
                &address.to_string(),
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .unwrap();
            let original = client.replace_test_rpc(rpc);
            let result = match mode {
                "scalar" => client
                    .readdir_bounded(&root, DirCookie::default(), &[FATTR4_TYPE], 4096)
                    .map_err(|error| {
                        vfsi_core::OpError::failed(vfsi_core::error_from_rpc(error, None), None)
                    }),
                "vector" => client
                    .readdir_pages_bounded(&[(root, DirCookie::default())], &[FATTR4_TYPE], 4096)
                    .unwrap()
                    .remove(0),
                _ => client
                    .readdir_children_bounded(&[(root, b"child".to_vec())], &[FATTR4_TYPE], 4096)
                    .unwrap()
                    .remove(0)
                    .map(|page| (page.entries, page.cookie)),
            };
            client.replace_test_rpc(original);
            let observed = peer.join().unwrap();
            client.shutdown().unwrap();
            if outcome.starts_with("advancing-") {
                assert_eq!(observed.len(), 16, "{mode}/{outcome}: bounded wire work");
                assert_eq!(observed[0], (0, [0; 8]));
                for (index, request) in observed.iter().enumerate().skip(1) {
                    assert_eq!(*request, (41 + index as u64, [0x7e; 8]));
                }
            } else {
                assert_eq!(observed, [(0, [0; 8]), (42, [0x7e; 8])], "{mode}/{outcome}");
            }
            match outcome {
                "success" | "advancing-visible" => {
                    let (entries, cookie) = result.unwrap();
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].name, b"two");
                    assert_eq!(cookie.value, 0);
                }
                "advancing-eof" => {
                    let (entries, cookie) = result.unwrap();
                    assert!(entries.is_empty());
                    assert_eq!(cookie.value, 0);
                }
                "advancing-limit" => {
                    let error = result.unwrap_err();
                    assert_eq!(
                        error.cause().transport_kind(),
                        Some(vfsi_core::TransportKind::InvalidReply)
                    );
                    assert!(error.cause().to_string().contains("filtered-page limit"));
                }
                "cycle" => assert!(result.unwrap_err().cause().is_transport()),
                _ => {
                    let error = result.unwrap_err();
                    assert_eq!(error.state(), FailureState::Failed);
                    assert_eq!(
                        error.cause().status(),
                        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_BAD_COOKIE))
                    );
                }
            }
        }
    }
}

#[test]
fn filtered_continuation_failures_preserve_visible_and_independent_siblings() {
    use crate::rpc::{RpcClient, test_support::readdir_filtered_peer};
    use std::{net::TcpListener, time::Duration};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    for outcome in ["siblings-status", "siblings-lost"] {
        let mut client = NfsClient::connect_minor(&server, 1).unwrap();
        let root = client.root().clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let peer = readdir_filtered_peer(listener, outcome);
        let rpc = RpcClient::connect_with_timeouts(
            &address.to_string(),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();
        let original = client.replace_test_rpc(rpc);
        // The peer serves each filtered sibling independently: an ordinary status
        // must not suppress the next input, unlike a lost transport reply.
        let results = client
            .readdir_pages_bounded(
                &[
                    (root.clone(), DirCookie::default()),
                    (root.clone(), DirCookie::default()),
                    (root, DirCookie::default()),
                ],
                &[FATTR4_TYPE],
                4096,
            )
            .unwrap();
        client.replace_test_rpc(original);
        let observed = peer.join().unwrap();
        client.shutdown().unwrap();
        let first = results[0].as_ref().unwrap_err();
        let last = results[2].as_ref().unwrap_err();
        assert_eq!(results[1].as_ref().unwrap().0[0].name, b"x");
        assert_eq!(last.state(), FailureState::Failed);
        if outcome == "siblings-status" {
            assert_eq!(observed.len(), 3);
            assert_eq!(first.state(), FailureState::Failed);
            assert_eq!(
                last.cause().status(),
                Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_BAD_COOKIE))
            );
        } else {
            assert_eq!(
                observed.len(),
                2,
                "no continuation dispatched after reply loss"
            );
            assert_eq!(first.state(), FailureState::Unknown);
            assert!(last.cause().is_transport());
        }
    }
}
