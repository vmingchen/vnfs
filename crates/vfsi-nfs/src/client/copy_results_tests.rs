use super::*;
use std::sync::Arc;
use vfsi_core::{
    FailureState, TransportKind, VfError,
    internal::faults::{FaultScript, OpenFaultPoint},
};

fn copy_reply(count: u64) -> COPY4resok {
    COPY4resok {
        cr_response: write_response4 {
            wr_ids: 0,
            wr_callback_id: stateid4 {
                seqid: 0,
                other: [0; 12],
            },
            wr_count: count,
            wr_committed: stable_how4_FILE_SYNC4,
            wr_writeverf: [0; 8],
        },
        cr_consecutive: 1,
        cr_synchronous: 1,
    }
}

#[test]
fn copy_ack_validation_keeps_short_eof_and_rejects_untrackable_effects() {
    let mut op = CopyOp {
        src_fh: NfsFh::test_from_bytes([1]),
        dst_fh: NfsFh::test_from_bytes([2]),
        src_stateid: stateid4 {
            seqid: 0,
            other: [0; 12],
        },
        dst_stateid: stateid4 {
            seqid: 0,
            other: [0; 12],
        },
        src_offset: 4,
        dst_offset: 8,
        count: 3,
    };
    for count in [0, 2, 3] {
        for stable in [
            stable_how4_UNSTABLE4,
            stable_how4_DATA_SYNC4,
            stable_how4_FILE_SYNC4,
        ] {
            let mut reply = copy_reply(count);
            reply.cr_response.wr_committed = stable;
            assert_eq!(validate_copy_result(&op, &reply).unwrap(), count);
        }
    }
    for field in 0..6 {
        let mut reply = copy_reply(2);
        match field {
            0 => reply.cr_response.wr_ids = 1,
            1 => reply.cr_synchronous = 0,
            2 => reply.cr_consecutive = 0,
            3 => reply.cr_response.wr_count = 4,
            4 => reply.cr_response.wr_committed = stable_how4_FILE_SYNC4 + 1,
            5 => reply.cr_synchronous = 2,
            _ => unreachable!(),
        }
        let valid_reply = copy_reply(2);
        let results = completion::decode_fixed::<4, _>(
            3,
            13,
            0,
            |_| 0,
            |input| validate_copy_result(&op, if input == 1 { &reply } else { &valid_reply }),
        )
        .unwrap();
        assert!(results[0].is_ok() && results[2].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(
            error.cause().transport_kind(),
            Some(TransportKind::InvalidReply)
        );
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    for source in [false, true] {
        op.count = 0; // EOF copies still must not acknowledge offset wraparound.
        op.src_offset = if source { u64::MAX } else { 4 };
        op.dst_offset = if source { 8 } else { u64::MAX };
        assert_eq!(
            validate_copy_result(&op, &copy_reply(2))
                .unwrap_err()
                .state(),
            FailureState::Unknown
        );
    }
}

#[test]
fn live_copy_results_preserve_status_sparse_validation_and_submission_boundaries() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    for phase in [
        "healthy",
        "status",
        "before",
        "lost",
        "resource",
        "invalid",
        "invalid-lost",
    ] {
        let mut client = NfsClient::connect_minor(&server, 2).unwrap();
        let root = client.root().clone();
        let name = format!("vfsi-copy-results-{}-{phase}", std::process::id());
        let directory = client.mkdir(&root, &name).unwrap();
        let mut sources = Vec::new();
        let mut destinations = Vec::new();
        for index in 0..5 {
            let source = client
                .open(
                    &directory,
                    format!("src-{index}").as_bytes(),
                    OPEN4_SHARE_ACCESS_BOTH,
                    OpenCreate::Exclusive,
                )
                .unwrap();
            client.write(&source.0, &source.1, 0, b"abcdefgh").unwrap();
            sources.push(source);
            let destination = client
                .open(
                    &directory,
                    format!("dst-{index}").as_bytes(),
                    OPEN4_SHARE_ACCESS_BOTH,
                    OpenCreate::Exclusive,
                )
                .unwrap();
            client
                .write(&destination.0, &destination.1, 0, b"XXXXXXXX")
                .unwrap();
            destinations.push(destination);
        }
        let mut ops: Vec<_> = sources
            .iter()
            .zip(&destinations)
            .map(|(source, destination)| CopyOp {
                src_fh: source.0.clone(),
                src_stateid: source.1,
                dst_fh: destination.0.clone(),
                dst_stateid: destination.1,
                src_offset: 1,
                dst_offset: 2,
                count: 3,
            })
            .collect();
        if phase == "status" {
            ops[1].dst_stateid = stateid4 {
                seqid: 0,
                other: [!0; 12],
            };
        }
        if phase.starts_with("invalid") {
            ops[1].src_offset = u64::MAX;
        }
        let max_ops = client.max_ops;
        let point = match phase {
            "before" => Some(OpenFaultPoint::BeforeCopyCompound { first: 2 }),
            "lost" | "invalid-lost" => Some(OpenFaultPoint::AfterCopyCompoundReply { first: 2 }),
            _ => None,
        };
        if point.is_some() {
            client.max_ops = 9;
        } // Two COPYs plus SEQUENCE.
        let fault = Arc::new(if let Some(point) = &point {
            FaultScript::one(
                point.clone(),
                VfError::transport_with_kind(
                    None,
                    TransportKind::Timeout,
                    "selected COPY reply unavailable",
                ),
            )
        } else {
            FaultScript::new([])
        });
        client.set_fault_injector(fault.clone());
        if phase == "resource" {
            client.inject_resource_rejection_once(b"copyv");
        }
        let _ = crate::compound::thread_compound_stats();
        let results = client.copy_many(&ops).unwrap();
        let compounds = crate::compound::thread_compound_stats().0;
        assert_eq!(results.len(), 5);
        for (index, result) in results.iter().enumerate() {
            let expected = match phase {
                "status" if index == 1 => Some(FailureState::Failed),
                "invalid" | "invalid-lost" if index == 1 => Some(FailureState::NotAttempted),
                "before" if index >= 2 => Some(FailureState::NotAttempted),
                "lost" if index == 2 || index == 3 => Some(FailureState::Unknown),
                "lost" if index == 4 => Some(FailureState::NotAttempted),
                "invalid-lost" if index >= 3 => Some(FailureState::Unknown),
                _ => None,
            };
            if let Some(state) = expected {
                let error = result.as_ref().unwrap_err();
                assert_eq!(error.state(), state, "{phase}/{index}");
                assert_eq!(error.cause().index(), None);
                assert!(error.progress().is_none());
                if phase.starts_with("invalid") && index == 1 {
                    assert_eq!(
                        error.cause().status(),
                        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_INVAL))
                    );
                } else if phase == "status" {
                    assert_eq!(
                        error.cause().status(),
                        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_BAD_STATEID))
                    );
                } else {
                    assert_eq!(error.cause().transport_kind(), Some(TransportKind::Timeout));
                }
            } else {
                assert_eq!(*result.as_ref().unwrap(), 3, "{phase}/{index}");
            }
        }
        match phase {
            "healthy" | "invalid" | "before" => assert_eq!(compounds, 1),
            "status" | "lost" | "invalid-lost" => assert_eq!(compounds, 2),
            "resource" => assert!(!client.resource_rejection_pending()),
            _ => unreachable!(),
        }
        if let Some(point) = point {
            assert!(fault.is_consumed());
            assert_eq!(
                fault
                    .visited()
                    .iter()
                    .filter(|visited| **visited == point)
                    .count(),
                1
            );
        }
        if phase == "status" {
            let starts: Vec<_> = fault
                .visited()
                .iter()
                .filter_map(|point| {
                    if let OpenFaultPoint::BeforeCopyCompound { first } = point {
                        Some(*first)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(
                starts,
                [0, 2],
                "neither completed nor failed COPY is replayed"
            );
        }
        client.max_ops = max_ops;
        for index in 0..5 {
            let changed = match phase {
                "status" | "invalid" | "invalid-lost" => index != 1,
                "before" => index < 2,
                "lost" => index < 4,
                _ => true,
            };
            assert_eq!(
                client
                    .read(&destinations[index].0, &destinations[index].1, 0, 16)
                    .unwrap()
                    .0,
                if changed {
                    b"XXbcdXXX".as_slice()
                } else {
                    b"XXXXXXXX".as_slice()
                },
                "{phase}/{index}"
            );
            assert_eq!(
                client
                    .read(&sources[index].0, &sources[index].1, 0, 16)
                    .unwrap()
                    .0,
                b"abcdefgh"
            );
            for (file, kind) in [(&sources[index], "src"), (&destinations[index], "dst")] {
                client.close(&file.0, &file.1).unwrap();
                client
                    .remove(&directory, &format!("{kind}-{index}"))
                    .unwrap();
            }
        }
        client.remove(&root, &name).unwrap();
    }
}

#[test]
fn copy_pre_dispatch_error_domain_and_empty_validation_are_preserved() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut client = NfsClient::connect_minor(&server, 2).unwrap();
    let root = client.root().clone();
    let cause = VfError::errno(None, libc::EACCES as u32);
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCopyCompound { first: 0 },
        cause.clone(),
    ));
    client.set_fault_injector(fault.clone());
    let _ = crate::compound::thread_compound_stats();
    assert!(client.copy_many(&[]).unwrap().is_empty());
    let valid = CopyOp {
        src_fh: root,
        dst_fh: NfsFh::test_from_bytes([1, 2]),
        src_stateid: stateid4 {
            seqid: 0,
            other: [0; 12],
        },
        dst_stateid: stateid4 {
            seqid: 0,
            other: [0; 12],
        },
        src_offset: 0,
        dst_offset: 0,
        count: 1,
    };
    let rejected = CopyOp {
        src_fh: valid.src_fh.clone(),
        dst_fh: valid.src_fh.clone(),
        src_stateid: valid.src_stateid,
        dst_stateid: valid.dst_stateid,
        src_offset: 0,
        dst_offset: 0,
        count: 1,
    };
    let results = client.copy_many(&[rejected]).unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(!fault.is_consumed());
    let error = client.copy_many(&[valid]).unwrap_err();
    assert!(fault.is_consumed());
    assert_eq!(error.domain(), cause.domain());
    assert_eq!(error.raw_code(), libc::EACCES as u32);
    assert_eq!(crate::compound::thread_compound_stats().0, 0);
}
