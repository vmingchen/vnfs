use super::*;
use std::sync::Arc;
use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
use vfsi_core::{FailureState, OpError, VfError};

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
        assert_eq!(error.cause().index(), None);
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
        VfError::transport(None, "second READLINK reply lost"),
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
        VfError::transport(None, "second child-lookup reply lost"),
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
