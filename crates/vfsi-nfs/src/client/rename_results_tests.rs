use super::*;
use std::sync::Arc;
use vfsi_core::{
    FailureState, TransportKind, VfError,
    internal::faults::{FaultScript, OpenFaultPoint},
};

#[test]
fn rename_results_preserve_independent_names_and_submission_boundaries() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for paths in [false, true] {
        for phase in [
            "healthy", "status", "before", "lost", "resource", "invalid", "oversize",
        ] {
            if phase == "invalid" && !paths {
                continue;
            }
            let mut client = NfsClient::connect_minor(&server, minor).unwrap();
            let root = client.root().clone();
            let name = format!("vfsi-rename-results-{}-{paths}-{phase}", std::process::id());
            let directory = client.mkdir(&root, &name).unwrap();
            let source: Vec<_> = (0..5).map(|index| format!("old-{index}")).collect();
            let destination: Vec<_> = (0..5).map(|index| format!("new-{index}")).collect();
            let mut identities = Vec::new();
            for source in &source {
                let (fh, stateid) = client
                    .open(
                        &directory,
                        source.as_bytes(),
                        OPEN4_SHARE_ACCESS_BOTH,
                        OpenCreate::Exclusive,
                    )
                    .unwrap();
                client.close(&fh, &stateid).unwrap();
                identities.push(fh);
            }
            let mut ops: Vec<_> = source
                .iter()
                .zip(&destination)
                .map(|(source, destination)| RenameOp {
                    srcdir: directory.clone(),
                    oldname: source.as_bytes().to_vec(),
                    dstdir: directory.clone(),
                    newname: destination.as_bytes().to_vec(),
                })
                .collect();
            let mut pairs: Vec<_> = source
                .iter()
                .zip(&destination)
                .map(|(source, destination)| PathRenamePair {
                    src: format!("{name}/{source}").into_bytes(),
                    dst: format!("{name}/{destination}").into_bytes(),
                })
                .collect();
            if phase == "status" {
                ops[1].oldname = b"missing".to_vec();
                pairs[1].src = format!("{name}/missing").into_bytes();
            }
            if phase == "invalid" {
                pairs[1].dst = b"/".to_vec();
            }
            let max_bytes = client.max_compound_bytes;
            if phase == "oversize" {
                ops[1].newname = vec![b'x'; 2048];
                pairs[1].dst = format!("{name}/{}", "x".repeat(2048)).into_bytes();
                client.max_compound_bytes = client.rpc_envelope_reserve + 512;
            }
            let max_ops = client.max_ops;
            let first = if paths { 1 } else { 2 };
            let point = match phase {
                "before" => Some(OpenFaultPoint::BeforeRenameCompound { first }),
                "lost" => Some(OpenFaultPoint::AfterRenameCompoundReply { first }),
                _ => None,
            };
            let fault = point.map(|point| {
                Arc::new(FaultScript::one(
                    point,
                    VfError::transport_with_kind(
                        None,
                        TransportKind::Timeout,
                        "selected rename reply unavailable",
                    ),
                ))
            });
            if let Some(fault) = &fault {
                client.max_ops = 9;
                client.set_fault_injector(fault.clone());
            }
            if phase == "resource" {
                client.inject_resource_rejection_once(if paths { b"renamev1" } else { b"renamev" });
            }
            let _ = crate::compound::thread_compound_stats();
            let results = if paths {
                client.renamev_path_compound(&pairs)
            } else {
                client.rename_many(&ops)
            }
            .unwrap();
            let compounds = crate::compound::thread_compound_stats().0;
            assert_eq!(results.len(), 5);
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
                    if phase == "oversize" {
                        assert_eq!(
                            error.cause().status(),
                            Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_REQ_TOO_BIG))
                        );
                    }
                    if phase == "before" || phase == "lost" {
                        assert_eq!(error.cause().transport_kind(), Some(TransportKind::Timeout));
                    }
                } else {
                    assert!(result.is_ok(), "{paths}/{phase}/{index}: {result:?}");
                }
            }
            match phase {
                "healthy" | "invalid" => assert_eq!(
                    compounds, 1,
                    "healthy siblings retain shared-parent batching"
                ),
                "status" | "lost" => {
                    assert_eq!(compounds, 2, "completed or failed RENAME is never replayed")
                }
                "before" => assert_eq!(compounds, 1),
                "resource" => assert!(!client.resource_rejection_pending()),
                "oversize" => assert!(
                    (1..=4).contains(&compounds),
                    "oversized input is rejected locally while valid siblings remain bounded"
                ),
                _ => unreachable!(),
            }
            if let Some(fault) = &fault {
                assert!(fault.is_consumed());
            }
            client.max_ops = max_ops;
            client.max_compound_bytes = max_bytes;
            for (index, identity) in identities.iter().enumerate() {
                let changed = match phase {
                    "status" | "invalid" | "oversize" => index != 1,
                    "before" => index < first,
                    "lost" => index < 2 * first,
                    _ => true,
                };
                let (present, absent) = if changed {
                    (&destination[index], &source[index])
                } else {
                    (&source[index], &destination[index])
                };
                assert_eq!(
                    client.lookup(&directory, present.as_bytes()).unwrap(),
                    *identity
                );
                assert_eq!(
                    client
                        .lookup(&directory, absent.as_bytes())
                        .unwrap_err()
                        .status,
                    nfsstat4_NFS4ERR_NOENT
                );
                client.remove(&directory, present).unwrap();
            }
            client.remove(&root, &name).unwrap();
        }
    }
}

#[test]
fn rename_pre_dispatch_faults_preserve_error_domain_and_do_not_send_mutations() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for paths in [false, true] {
        let mut client = NfsClient::connect_minor(&server, minor).unwrap();
        let root = client.root().clone();
        let cause = VfError::errno(None, libc::EACCES as u32);
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeRenameCompound { first: 0 },
            cause.clone(),
        ));
        client.set_fault_injector(fault.clone());
        let _ = crate::compound::thread_compound_stats();
        let error = if paths {
            client
                .renamev_path_compound(&[PathRenamePair {
                    src: b"not-created".to_vec(),
                    dst: b"also-not-created".to_vec(),
                }])
                .unwrap_err()
        } else {
            client
                .rename_many(&[RenameOp {
                    srcdir: root.clone(),
                    oldname: b"not-created".to_vec(),
                    dstdir: root,
                    newname: b"also-not-created".to_vec(),
                }])
                .unwrap_err()
        };
        assert!(fault.is_consumed());
        assert_eq!(error.domain(), cause.domain());
        assert_eq!(error.raw_code(), libc::EACCES as u32);
        assert_eq!(error.index(), None);
        assert_eq!(crate::compound::thread_compound_stats().0, 0);
    }
}
