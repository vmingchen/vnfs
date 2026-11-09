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
                let ops: Vec<_> = handles.into_iter().map(|fh| (fh, 0)).collect();
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
                    assert_eq!(cookie, 0);
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
            .readdir_pages(&[(root.clone(), 0)], &[FATTR4_TYPE])
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
