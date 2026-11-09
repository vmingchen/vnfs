use super::*;
use std::sync::Arc;
use vfsi_core::{
    FailureState, TransportKind, VfError,
    internal::faults::{FaultScript, OpenFaultPoint},
};

#[test]
fn remove_results_preserve_chained_packing_status_and_submission_boundaries() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    for paths in [false, true] {
        for phase in [
            "healthy", "status", "before", "lost", "resource", "invalid", "oversize", "compact",
        ] {
            if paths && phase == "compact" {
                continue;
            }
            let mut client = NfsClient::connect_minor(&server, 1).unwrap();
            let root = client.root().clone();
            let name = format!("remove-results-{}-{paths}-{phase}", std::process::id());
            let directory = client.mkdir(&root, &name).unwrap();
            let count = if phase == "compact" { 20 } else { 5 };
            let originals: Vec<_> = (0..count).map(|index| format!("f{index}")).collect();
            for name in &originals {
                let (fh, state) = client
                    .open(
                        &directory,
                        name.as_bytes(),
                        OPEN4_SHARE_ACCESS_BOTH,
                        OpenCreate::Exclusive,
                    )
                    .unwrap();
                client.close(&fh, &state).unwrap();
            }
            let mut names: Vec<_> = originals
                .iter()
                .map(|name| name.as_bytes().to_vec())
                .collect();
            let mut full: Vec<_> = originals
                .iter()
                .map(|leaf| format!("{name}/{leaf}").into_bytes())
                .collect();
            if phase == "status" {
                names[1] = b"missing".to_vec();
                full[1] = format!("{name}/missing").into_bytes();
            }
            if phase == "invalid" {
                names[1] = b"invalid/name".to_vec();
                full[1] = b"/".to_vec();
            }
            let old_bytes = client.max_compound_bytes;
            if phase == "oversize" {
                names[1] = vec![b'x'; 2048];
                full[1] = format!("{name}/{}", "x".repeat(2048)).into_bytes();
                client.max_compound_bytes = client.rpc_envelope_reserve + 512;
            }
            if phase == "compact" {
                client.max_compound_bytes = client.rpc_envelope_reserve + 512;
            }
            let old_ops = client.max_ops;
            let first = if paths { 1 } else { 2 };
            let point = match phase {
                "before" => Some(OpenFaultPoint::BeforeRemoveCompound { first }),
                "lost" => Some(OpenFaultPoint::AfterRemoveCompoundReply { first }),
                _ => None,
            };
            let fault = point.map(|point| {
                Arc::new(FaultScript::one(
                    point,
                    VfError::transport_with_kind(
                        None,
                        TransportKind::Timeout,
                        "selected REMOVE reply unavailable",
                    ),
                ))
            });
            if let Some(fault) = &fault {
                client.max_ops = if paths { 7 } else { 4 };
                client.set_fault_injector(fault.clone());
            }
            if phase == "resource" {
                client.inject_resource_rejection_once(if paths { b"removev1" } else { b"removev" });
            }
            let _ = crate::compound::thread_compound_stats();
            let results = if paths {
                client.removev_path_compound(&full)
            } else {
                client.remove_many(&directory, &names)
            }
            .unwrap();
            let calls = crate::compound::thread_compound_stats().0;
            assert_eq!(results.len(), count);
            for (index, result) in results.iter().enumerate() {
                let state = match phase {
                    "status" if index == 1 => Some(FailureState::Failed),
                    "invalid" | "oversize" if index == 1 => Some(FailureState::NotAttempted),
                    "before" if index >= first => Some(FailureState::NotAttempted),
                    "lost" if index >= first && index < 2 * first => Some(FailureState::Unknown),
                    "lost" if index >= 2 * first => Some(FailureState::NotAttempted),
                    _ => None,
                };
                if let Some(state) = state {
                    let error = result.as_ref().unwrap_err();
                    assert_eq!(error.state(), state, "{paths}/{phase}/{index}");
                    assert_eq!(error.cause().index(), None);
                    assert!(error.progress().is_none());
                } else {
                    assert!(result.is_ok(), "{paths}/{phase}/{index}: {result:?}");
                }
            }
            match phase {
                "healthy" | "invalid" | "compact" => {
                    assert_eq!(calls, 1, "chained REMOVE packing must not regress")
                }
                "status" | "lost" => {
                    assert_eq!(calls, 2, "never replay a completed or ambiguous REMOVE")
                }
                "before" => assert_eq!(calls, 1),
                "resource" => assert!(!client.resource_rejection_pending()),
                "oversize" => assert!((1..=4).contains(&calls)),
                _ => unreachable!(),
            }
            if let Some(fault) = &fault {
                assert!(fault.is_consumed());
            }
            client.max_ops = old_ops;
            client.max_compound_bytes = old_bytes;
            for (index, leaf) in originals.iter().enumerate() {
                let removed = match phase {
                    "status" | "invalid" | "oversize" => index != 1,
                    "before" => index < first,
                    "lost" => index < 2 * first,
                    _ => true,
                };
                let observed = client.lookup(&directory, leaf.as_bytes());
                if removed {
                    assert_eq!(observed.unwrap_err().status, nfsstat4_NFS4ERR_NOENT);
                } else {
                    observed.unwrap();
                    client.remove(&directory, leaf).unwrap();
                }
            }
            client.remove(&root, &name).unwrap();
        }
    }
}

#[test]
fn remove_empty_invalid_and_predispatch_rejection_never_send_mutations() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    for paths in [false, true] {
        let mut client = NfsClient::connect_minor(&server, 1).unwrap();
        let root = client.root().clone();
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeRemoveCompound { first: 0 },
            VfError::errno(None, libc::EACCES as u32),
        ));
        client.set_fault_injector(fault.clone());
        let _ = crate::compound::thread_compound_stats();
        if paths {
            assert!(client.removev_path_compound(&[]).unwrap().is_empty());
            let invalid = client
                .removev_path_compound(&[b"/".to_vec(), b"bad\0name".to_vec()])
                .unwrap();
            assert!(
                invalid.iter().all(
                    |result| result.as_ref().unwrap_err().state() == FailureState::NotAttempted
                )
            );
        } else {
            assert!(client.remove_many(&root, &[]).unwrap().is_empty());
            let invalid = client.remove_many(&root, &[b"bad/name".to_vec()]).unwrap();
            assert!(
                invalid.iter().all(
                    |result| result.as_ref().unwrap_err().state() == FailureState::NotAttempted
                )
            );
        }
        assert!(!fault.is_consumed());
        let error = if paths {
            client.removev_path_compound(&[b"not-created".to_vec()])
        } else {
            client.remove_many(&root, &[b"not-created".to_vec()])
        }
        .unwrap_err();
        assert_eq!(
            error.status(),
            Some(vfsi_core::StatusCode::Errno(libc::EACCES as u32))
        );
        assert!(fault.is_consumed());
        assert_eq!(crate::compound::thread_compound_stats().0, 0);
    }
}
