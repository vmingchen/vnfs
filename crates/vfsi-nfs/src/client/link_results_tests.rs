use super::*;
use std::sync::Arc;
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_core::{FailureState, VfError};

#[test]
fn namespace_mutations_preserve_status_submission_and_uncertainty_boundaries() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for hard in [false, true] {
        for phase in ["status", "before", "lost", "resource", "oversize"] {
            let mut client = NfsClient::connect_minor(&server, minor).unwrap();
            let root = client.root().clone();
            let name = format!("vfsi-link-results-{}-{hard}-{phase}", std::process::id());
            let dir = client.mkdir(&root, &name).unwrap();
            let (source, stateid) = client
                .open(
                    &dir,
                    b"source",
                    OPEN4_SHARE_ACCESS_BOTH,
                    OpenCreate::Exclusive,
                )
                .unwrap();
            client.close(&source, &stateid).unwrap();
            let names: Vec<_> = (0..5).map(|index| format!("entry-{index}")).collect();
            if phase == "status" {
                client.symlink(&dir, &names[1], "collision").unwrap();
            }
            let mut creates: Vec<_> = names
                .iter()
                .map(|name| CreateOp {
                    dir: dir.clone(),
                    name: name.as_bytes().to_vec(),
                    ftype: nfs_ftype4_NF4LNK,
                    linkdata: Some(b"../target".to_vec()),
                    mode: None,
                })
                .collect();
            let mut links: Vec<_> = names
                .iter()
                .map(|name| LinkOp {
                    src: source.clone(),
                    dstdir: dir.clone(),
                    newname: name.as_bytes().to_vec(),
                })
                .collect();
            let max_ops = client.max_ops;
            if phase == "oversize" {
                client.set_max_compound_bytes(4096);
                creates[1].name = vec![b'x'; 8192];
                links[1].newname = vec![b'x'; 8192];
            }
            let point = match (hard, phase) {
                (false, "before") => Some(OpenFaultPoint::BeforeCreateCompound { first: 2 }),
                (true, "before") => Some(OpenFaultPoint::BeforeLinkCompound { first: 2 }),
                (false, "lost") => Some(OpenFaultPoint::AfterCreateCompoundReply { first: 2 }),
                (true, "lost") => Some(OpenFaultPoint::AfterLinkCompoundReply { first: 2 }),
                _ => None,
            };
            let fault = point.map(|point| {
                Arc::new(FaultScript::one(
                    point,
                    VfError::transport(None, "selected namespace mutation reply unavailable"),
                ))
            });
            if let Some(fault) = &fault {
                client.max_ops = if hard { 9 } else { 5 };
                client.set_fault_injector(fault.clone());
            }
            if phase == "resource" {
                client.inject_resource_rejection_once(if hard { b"linkv" } else { b"createv" });
            }
            let _ = crate::compound::thread_compound_stats();
            let results = if hard {
                client.link_many(&links)
            } else {
                client.create_many(&creates)
            }
            .unwrap();
            assert_eq!(results.len(), 5);
            let compounds = crate::compound::thread_compound_stats().0;
            match phase {
                "status" => {
                    for (index, result) in results.iter().enumerate() {
                        if index == 1 {
                            let error = result.as_ref().unwrap_err();
                            assert_eq!(error.state(), FailureState::Failed);
                            assert_eq!(error.cause().kind(), std::io::ErrorKind::AlreadyExists);
                            assert_eq!(error.cause().index(), None);
                        } else {
                            assert!(result.is_ok());
                        }
                    }
                    assert_eq!(compounds, 2);
                }
                "oversize" => {
                    assert!(results[0].is_ok());
                    assert!(results[2..].iter().all(Result::is_ok));
                    assert_eq!(
                        results[1].as_ref().unwrap_err().state(),
                        FailureState::NotAttempted
                    );
                    assert_eq!(
                        compounds, 2,
                        "healthy siblings keep batching around one oversized request"
                    );
                }
                "resource" => {
                    assert!(results.iter().all(Result::is_ok));
                    assert!(!client.resource_rejection_pending());
                }
                _ => {
                    assert!(fault.as_ref().unwrap().is_consumed());
                    assert!(results[..2].iter().all(Result::is_ok));
                    for (index, result) in results.iter().enumerate().skip(2) {
                        let error = result.as_ref().unwrap_err();
                        let expected = if phase == "lost" && index < 4 {
                            FailureState::Unknown
                        } else {
                            FailureState::NotAttempted
                        };
                        assert_eq!(error.state(), expected);
                        assert_eq!(error.cause().index(), None);
                        assert!(error.progress().is_none());
                    }
                    assert_eq!(compounds, if phase == "lost" { 2 } else { 1 });
                }
            }
            client.max_ops = max_ops;
            for (index, name) in names.iter().enumerate() {
                let present = phase == "oversize" && index != 1
                    || phase == "status"
                    || phase == "resource"
                    || phase != "oversize" && index < 2
                    || phase == "lost" && index < 4;
                let result = client.lookup(&dir, name.as_bytes());
                assert_eq!(
                    result.is_ok(),
                    present,
                    "server effects must match confirmed/submitted ranges: {hard}/{phase}/{index}"
                );
                if present {
                    client.remove(&dir, name).unwrap();
                }
            }
            client.max_ops = 1;
            let _ = crate::compound::thread_compound_stats();
            let rejected = if hard {
                client.link_many(&links)
            } else {
                client.create_many(&creates)
            };
            assert!(rejected.is_err());
            assert_eq!(crate::compound::thread_compound_stats().0, 0);
            client.max_ops = max_ops;
            client.remove(&dir, "source").unwrap();
            client.remove(&root, &name).unwrap();
        }
    }
}
