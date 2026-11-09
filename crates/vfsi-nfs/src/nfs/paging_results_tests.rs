use super::*;
use std::sync::Arc;
use vfsi_core::{
    FailureState,
    internal::faults::{FaultScript, OpenFaultPoint},
};

fn backend() -> Option<NfsVecFs> {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    let mut fs = NfsVecFs::connect_minor(&server, 1).unwrap();
    fs.auto_reconnect = false;
    Some(fs)
}
fn create(fs: &mut NfsVecFs, path: &Path) {
    let file = fs
        .open_raw_impl(path, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    fs.close_impl(&file).unwrap();
}

#[test]
fn native_page_results_keep_siblings_and_anchored_children_after_rename() {
    let Some(mut fs) = backend() else {
        return;
    };
    let base = PathBuf::from(format!("/native-page-results-{}", std::process::id()));
    let first = base.join("first");
    let child = first.join("child");
    let last = base.join("last");
    fs.ensure_dir_impl(&child, 0o755).unwrap();
    fs.ensure_dir_impl(&last, 0o755).unwrap();
    create(&mut fs, &child.join("payload"));
    for index in 0..3 {
        create(&mut fs, &last.join(format!("file{index}")));
    }
    let missing = base.join("missing");
    let results = fs
        .vlistdir_pages_impl(
            &[&first, &missing, &last],
            AttrMask::MODE,
            vec![None, None, None],
            1,
            10,
            true,
        )
        .unwrap();
    assert_eq!(results.len(), 3);
    let mut results = results.into_iter();
    let (entries, _, mut children) = results.next().unwrap().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].ftype, VfType::Directory);
    let seed = children.remove(0).expect("anchored child");
    let error = results.next().unwrap().err().expect("failed page");
    assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
    assert_eq!(error.cause().index(), None);
    let (_, next, _) = results.next().unwrap().unwrap();
    let next = next.expect("retain buffered sibling entries");
    let moved = base.join("moved");
    fs.rename_impl(&first, &moved).unwrap();
    fs.ensure_dir_impl(&first, 0o755).unwrap();
    create(&mut fs, &first.join("keep"));
    let bad = base.join("bad");
    let _ = crate::compound::thread_compound_stats();
    let results = fs
        .vlistdir_pages_impl(
            &[&child, &bad, &last],
            AttrMask::MODE,
            vec![Some(seed), Some(DirPageCursor::new(())), Some(next)],
            1,
            10,
            false,
        )
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].as_ref().unwrap().0[0]
            .file
            .path()
            .unwrap()
            .file_name()
            .unwrap(),
        "payload"
    );
    assert_eq!(
        results[1].as_ref().err().expect("failed page").state(),
        FailureState::NotAttempted
    );
    assert!(results[2].is_ok());
    assert_eq!(
        crate::compound::thread_compound_stats().0,
        1,
        "only the anchored child requires a READDIR; buffered siblings do not refetch"
    );
    assert!(fs.exists_impl(&moved.join("child/payload")).unwrap());
    assert!(fs.exists_impl(&first.join("keep")).unwrap());
    fs.remove_impl(&base, true).unwrap();
}

#[test]
fn native_page_results_preserve_buffered_success_after_child_reply_loss() {
    let Some(mut fs) = backend() else {
        return;
    };
    let base = PathBuf::from(format!("/native-page-loss-{}", std::process::id()));
    let parent = base.join("parent");
    let buffered = base.join("buffered");
    let fresh = base.join("fresh");
    for path in [&parent.join("a"), &parent.join("b"), &buffered, &fresh] {
        fs.ensure_dir_impl(path, 0o755).unwrap();
    }
    create(&mut fs, &parent.join("a/leaf"));
    create(&mut fs, &parent.join("b/leaf"));
    for index in 0..3 {
        create(&mut fs, &buffered.join(format!("file{index}")));
    }
    let mut initial = fs
        .vlistdir_pages_impl(
            &[&parent, &buffered],
            AttrMask::MODE,
            vec![None, None],
            2,
            10,
            true,
        )
        .unwrap()
        .into_iter();
    let (entries, _, children) = initial.next().unwrap().unwrap();
    let (_, buffered_next, _) = initial.next().unwrap().unwrap();
    let mut cursors: Vec<_> = children;
    let mut paths: Vec<_> = entries
        .iter()
        .map(|entry| entry.file.path().unwrap().to_owned())
        .collect();
    paths.push(buffered.clone());
    paths.push(fresh);
    cursors.push(buffered_next);
    cursors.push(None);
    let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterReadDirCompoundReply {
            children: true,
            first: 0,
            count: 2,
        },
        VfError::transport(None, "anchored child READDIR reply lost"),
    ));
    fs.set_fault_injector(fault.clone());
    let results = fs
        .vlistdir_pages_impl(&refs, AttrMask::MODE, cursors, 1, 10, true)
        .unwrap();
    assert_eq!(results.len(), 4);
    for result in &results[..2] {
        assert_eq!(
            result.as_ref().err().expect("failed page").state(),
            FailureState::Unknown
        );
    }
    assert!(
        results[2].is_ok(),
        "a validated buffered page survives another RPC's loss"
    );
    assert_eq!(
        results[3].as_ref().err().expect("failed page").state(),
        FailureState::Failed,
        "fresh metadata completed before the fatal child reply; its READDIR stayed unsent"
    );
    assert!(fault.is_consumed());
    assert_eq!(
        fault
            .visited()
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::BeforeReadDirCompound { .. }))
            .count(),
        1,
        "neither ambiguous child reads nor an untouched READDIR wave are replayed"
    );
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    fs.remove_impl(&base, true).unwrap();
}

#[test]
fn fresh_page_recovery_retries_only_failed_and_untouched_directories() {
    let Some(mut fs) = backend() else {
        return;
    };
    fs.set_auto_reconnect(true);
    let base = PathBuf::from(format!("/native-page-recovery-{}", std::process::id()));
    let paths: Vec<_> = (0..5).map(|index| base.join(format!("d{index}"))).collect();
    for path in &paths {
        fs.ensure_dir_impl(path, 0o755).unwrap();
        create(&mut fs, &path.join("leaf"));
    }
    let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    fs.nfs.max_ops = 5;
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterReadDirCompoundReply {
            children: false,
            first: 2,
            count: 2,
        },
        VfError::transport_with_kind(
            None,
            vfsi_core::TransportKind::Connection,
            "fresh READDIR reply lost",
        ),
    ));
    fs.set_fault_injector(fault.clone());
    let results = fs
        .vlistdir_pages_impl(
            &refs,
            AttrMask::MODE,
            (0..5).map(|_| None).collect(),
            1,
            10,
            true,
        )
        .unwrap();
    assert_eq!(results.len(), 5);
    assert!(results.iter().all(Result::is_ok));
    let attempts: usize = fault
        .visited()
        .iter()
        .filter_map(|point| match point {
            OpenFaultPoint::BeforeReadDirCompound {
                children: false,
                count,
                ..
            } => Some(*count),
            _ => None,
        })
        .sum();
    assert_eq!(
        attempts, 7,
        "two known successes are retained; retry two ambiguous plus one untouched input"
    );
    assert!(fault.is_consumed());
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    fs.remove_impl(&base, true).unwrap();
}
