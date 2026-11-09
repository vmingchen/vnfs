use super::*;
use std::sync::Arc;
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_core::{FailureState, OpError, VfError};

#[test]
fn setattr_vectors_preserve_status_and_submission_boundaries_without_replay() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for paths in [false, true] {
        for phase in ["status", "setattr", "before", "lost", "resource"] {
            let mut client = NfsClient::connect_minor(&server, minor).unwrap();
            let root = client.root().clone();
            let directory_name = format!(
                "vfsi-setattr-results-{}-{paths}-{phase}",
                std::process::id()
            );
            let directory = client.mkdir(&root, &directory_name).unwrap();
            let names: Vec<_> = (0..5).map(|index| format!("file-{index}")).collect();
            let mut opened = Vec::new();
            for name in &names {
                let (fh, stateid) = client
                    .open(
                        &directory,
                        name.as_bytes(),
                        OPEN4_SHARE_ACCESS_BOTH,
                        OpenCreate::Exclusive,
                    )
                    .unwrap();
                client.write(&fh, &stateid, 0, b"original").unwrap();
                opened.push((fh, stateid));
            }
            let mut descriptor_ops: Vec<_> = opened
                .iter()
                .map(|(fh, _)| SetattrOp {
                    fh: fh.clone(),
                    mode: None,
                    size: Some(2),
                    uid: None,
                    gid: None,
                    atime: None,
                    mtime: None,
                })
                .collect();
            let mut path_ops: Vec<_> = names
                .iter()
                .map(|name| PathSetattrOp {
                    file: FileRef::Path(format!("{directory_name}/{name}").into_bytes()),
                    mode: None,
                    size: Some(2),
                    uid: None,
                    gid: None,
                    atime: None,
                    mtime: None,
                    check_type: true,
                })
                .collect();
            if phase == "status" {
                descriptor_ops[1].fh = NfsFh::test_from_bytes([1, 2, 3]);
                path_ops[1].file = FileRef::Path(format!("{directory_name}/missing").into_bytes());
            }
            if phase == "setattr" {
                // Lookup succeeds, but mutating a directory's size fails at SETATTR.
                descriptor_ops[1].fh = directory.clone();
                path_ops[1].file = FileRef::Path(directory_name.as_bytes().to_vec());
            }
            let max_ops = client.max_ops;
            let point = match phase {
                "before" => Some(OpenFaultPoint::BeforeSetattrCompound {
                    first: if paths { 1 } else { 2 },
                }),
                "lost" => Some(OpenFaultPoint::AfterSetattrCompoundReply {
                    first: if paths { 1 } else { 2 },
                }),
                _ => None,
            };
            let fault = point.map(|point| {
                Arc::new(FaultScript::one(
                    point,
                    VfError::transport_with_kind(
                        None,
                        vfsi_core::TransportKind::Timeout,
                        "selected SETATTR unavailable",
                    ),
                ))
            });
            if let Some(fault) = &fault {
                client.max_ops = if paths { 8 } else { 5 };
                client.set_fault_injector(fault.clone());
            }
            if phase == "resource" {
                client.inject_resource_rejection_once(if paths {
                    b"setattrv1"
                } else {
                    b"setattrv"
                });
            }
            let _ = crate::compound::thread_compound_stats();
            let results = if paths {
                client.setattr_path_compound(&path_ops).map(|results| {
                    results
                        .into_iter()
                        .map(|result| {
                            result.map(|kind| {
                                assert_eq!(kind, Some(nfs_ftype4_NF4REG));
                            })
                        })
                        .collect::<Vec<_>>()
                })
            } else {
                client.setattr_many(&descriptor_ops)
            }
            .unwrap();
            let compounds = crate::compound::thread_compound_stats().0;
            assert_eq!(results.len(), 5);
            match phase {
                "status" | "setattr" => {
                    for (index, result) in results.iter().enumerate() {
                        if index == 1 {
                            let error = result.as_ref().unwrap_err();
                            assert_eq!(error.state(), FailureState::Failed);
                            assert_eq!(error.cause().index(), None);
                        } else {
                            assert!(result.is_ok());
                        }
                    }
                    assert_eq!(
                        compounds, 2,
                        "only failed compound and untouched suffix are sent"
                    );
                }
                "resource" => {
                    assert!(results.iter().all(Result::is_ok));
                    assert!(!client.resource_rejection_pending());
                }
                _ => {
                    assert!(fault.as_ref().unwrap().is_consumed());
                    let first = if paths { 1 } else { 2 };
                    assert!(results[..first].iter().all(Result::is_ok));
                    for (index, result) in results.iter().enumerate().skip(first) {
                        let error = result.as_ref().unwrap_err();
                        let expected = if phase == "lost" && index < 2 * first {
                            FailureState::Unknown
                        } else {
                            FailureState::NotAttempted
                        };
                        assert_eq!(error.state(), expected, "{paths}/{phase}/{index}");
                        assert_eq!(
                            error.cause().transport_kind(),
                            Some(vfsi_core::TransportKind::Timeout)
                        );
                        assert_eq!(error.cause().index(), None);
                        assert!(error.progress().is_none());
                    }
                    assert_eq!(compounds, if phase == "before" { 1 } else { 2 });
                }
            }
            client.max_ops = max_ops;
            for (index, (fh, stateid)) in opened.into_iter().enumerate() {
                let changed = match phase {
                    "status" | "setattr" => index != 1,
                    "resource" => true,
                    "before" => index < if paths { 1 } else { 2 },
                    "lost" => index < if paths { 2 } else { 4 },
                    _ => unreachable!(),
                };
                assert_eq!(
                    client.read(&fh, &stateid, 0, 16).unwrap().0.len(),
                    if changed { 2 } else { 8 },
                    "server effect {paths}/{phase}/{index}"
                );
                client.close(&fh, &stateid).unwrap();
                client.remove(&directory, &names[index]).unwrap();
            }
            client.remove(&root, &directory_name).unwrap();
        }
    }
}

#[test]
fn legacy_setattr_rpc_collection_preserves_status_and_transport_classification() {
    let error = collect_rpc_results(
        Ok(vec![
            Ok(()),
            Err(OpError::failed(
                VfError::nfs(None, nfsstat4_NFS4ERR_ACCESS),
                None,
            )),
            Ok(()),
        ]),
        3,
    )
    .unwrap_err();
    assert_eq!(error.op_index, 1);
    assert_eq!(error.status, nfsstat4_NFS4ERR_ACCESS);
    let error = collect_rpc_results::<()>(
        Err(VfError::transport_with_kind(
            None,
            vfsi_core::TransportKind::Auth,
            "authentication failed",
        )),
        3,
    )
    .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.transport_kind, Some(vfsi_core::TransportKind::Auth));
}

#[test]
fn setattr_pre_dispatch_errors_keep_their_original_domain() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut client = NfsClient::connect_minor(&server, minor).unwrap();
    let root = client.root().clone();
    for path in [false, true] {
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeSetattrCompound { first: 0 },
            VfError::errno(None, libc::EACCES as u32),
        ));
        client.set_fault_injector(fault.clone());
        let _ = crate::compound::thread_compound_stats();
        let cause = if path {
            client
                .setattr_path_compound(&[PathSetattrOp {
                    file: FileRef::Handle(root.clone()),
                    mode: None,
                    size: None,
                    uid: None,
                    gid: None,
                    atime: None,
                    mtime: None,
                    check_type: false,
                }])
                .unwrap_err()
        } else {
            client
                .setattr_many(&[SetattrOp {
                    fh: root.clone(),
                    mode: None,
                    size: None,
                    uid: None,
                    gid: None,
                    atime: None,
                    mtime: None,
                }])
                .unwrap_err()
        };
        assert!(fault.is_consumed());
        assert_eq!(
            cause.domain(),
            VfError::errno(None, libc::EACCES as u32).domain()
        );
        assert_eq!(cause.raw_code(), libc::EACCES as u32);
        assert_eq!(cause.index(), None);
        assert_eq!(crate::compound::thread_compound_stats().0, 0);
    }
}
