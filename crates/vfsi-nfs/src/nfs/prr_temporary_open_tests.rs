//! Cleanup owns only the newest stateid for each temporary path-owner object.
use super::*;
use crate::client::{CloseOp, FileRef, PathWriteOp};

#[test]
fn multi_compound_path_cleanup_closes_latest_states_in_one_batch() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    let root = fs.nfs.root().clone();
    let name = format!("prr-temporary-open-{}-{minor}", std::process::id());
    let dir = fs.nfs.mkdir(&root, &name).unwrap();
    fs.nfs.set_max_compound_bytes(4096);
    let data = vec![42; fs.nfs.per_op_bytes() * 2 + 17];
    let outcome = fs
        .nfs
        .writev_path_compound(
            &[
                PathWriteOp {
                    file: FileRef::Path(format!("{name}/large").into_bytes()),
                    offset: 0,
                    data: &data,
                    create: true,
                    truncate: true,
                    stateid: None,
                },
                PathWriteOp {
                    file: FileRef::Path(format!("{name}/other").into_bytes()),
                    offset: 0,
                    data: b"other",
                    create: true,
                    truncate: true,
                    stateid: None,
                },
            ],
            false,
        )
        .unwrap();
    assert!(outcome.failed.is_none());
    assert!(
        outcome.opened.len() > 2,
        "must reopen the large file across compounds"
    );
    let mut seen = std::collections::HashSet::new();
    let latest: Vec<_> = outcome
        .opened
        .iter()
        .rev()
        .filter(|(fh, _)| seen.insert(fh.clone()))
        .map(|(fh, stateid)| CloseOp {
            fh: fh.clone(),
            stateid: *stateid,
        })
        .collect();
    let before = fs.nfs.confirmed_path_closes();
    let _ = crate::compound::thread_compound_stats();
    fs.close_path_opens(&outcome.opened);
    let compounds = crate::compound::thread_compound_stats().0;
    let confirmed = fs.nfs.confirmed_path_closes() - before;
    let second_closes: Vec<_> = latest
        .iter()
        .map(|close| fs.nfs.close_many_path(std::slice::from_ref(close)))
        .collect();
    fs.nfs.remove(&dir, "large").unwrap();
    fs.nfs.remove(&dir, "other").unwrap();
    fs.nfs.remove(&root, &name).unwrap();
    assert_eq!(
        confirmed, 2,
        "the latest OPEN for both objects must be closed"
    );
    assert_eq!(compounds, 1, "temporary cleanup must remain batched");
    for result in second_closes {
        let error = result.expect_err("helper must have closed the current stateid");
        assert_eq!(error.status, nfsstat4_NFS4ERR_BAD_STATEID);
    }
}
