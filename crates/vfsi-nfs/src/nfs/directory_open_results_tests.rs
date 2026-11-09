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
    Some(NfsVecFs::connect_minor(&server, 1).unwrap())
}

#[test]
fn directory_open_export_root_uses_the_existing_identity() {
    let Some(mut fs) = backend() else {
        return;
    };
    fs.connection.root = PathBuf::from("/");
    let _ = crate::compound::thread_compound_stats();
    let dirs = fs.vopen_dirs_impl(&[Path::new("/")]).unwrap();
    assert_eq!(dirs.len(), 1);
    let dir = dirs.into_iter().next().unwrap().unwrap();
    assert_eq!(crate::compound::thread_compound_stats().0, 0);
    fs.close_dir_impl(&dir).unwrap();
    assert!(fs.open_dirs.is_empty());
}

#[test]
fn directory_open_results_batch_siblings_and_preserve_each_registered_owner() {
    let Some(mut fs) = backend() else {
        return;
    };
    let base = PathBuf::from(format!("/directory-open-results-{}", std::process::id()));
    fs.ensure_dir_impl(&base, 0o755).unwrap();
    let paths: Vec<_> = (0..12)
        .map(|index| base.join(format!("d{index:02}")))
        .collect();
    for path in &paths {
        fs.ensure_dir_impl(path, 0o755).unwrap();
    }
    let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let _ = crate::compound::thread_compound_stats();
    assert!(fs.vopen_dirs_impl(&[]).unwrap().is_empty());
    assert_eq!(crate::compound::thread_compound_stats().0, 0);
    let results = fs.vopen_dirs_impl(&refs).unwrap();
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(fs.open_dirs.len(), paths.len());
    let requests = crate::compound::thread_compound_stats().0;
    assert!(
        requests <= 3,
        "directory acquisition must remain batched: {requests}"
    );
    for result in results {
        fs.close_dir_impl(&result.unwrap()).unwrap();
    }
    let missing = base.join("missing");
    let link = base.join("link");
    fs.symlink_raw_impl(Path::new("d00"), &link).unwrap();
    let file_path = base.join("file");
    let file = fs
        .open_raw_impl(&file_path, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    fs.close_impl(&file).unwrap();
    let results = fs
        .vopen_dirs_impl(&[&paths[0], &missing, &paths[1], &link, &file_path])
        .unwrap();
    assert_eq!(results.len(), 5);
    assert!(results[0].is_ok() && results[2].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().cause().kind(),
        std::io::ErrorKind::NotFound
    );
    for index in [3, 4] {
        assert_eq!(
            results[index].as_ref().unwrap_err().cause().kind(),
            std::io::ErrorKind::NotADirectory
        );
    }
    for result in results.into_iter().flatten() {
        fs.close_dir_impl(&result).unwrap();
    }
    assert!(fs.open_dirs.is_empty());
    fs.remove_impl(&base, true).unwrap();
}

#[test]
fn directory_open_reply_loss_keeps_known_owners_and_does_not_register_uncertain_slots() {
    let Some(mut fs) = backend() else {
        return;
    };
    let base = PathBuf::from(format!("/directory-open-loss-{}", std::process::id()));
    fs.ensure_dir_impl(&base, 0o755).unwrap();
    let paths: Vec<_> = (0..5).map(|index| base.join(format!("d{index}"))).collect();
    for path in &paths {
        fs.ensure_dir_impl(path, 0o755).unwrap();
    }
    let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let max_ops = fs.nfs.max_ops;
    fs.nfs.max_ops = 9; // Two four-op LOOKUP/type requests per compound.
    fs.set_fault_injector(Arc::new(FaultScript::new([(
        OpenFaultPoint::AfterLookupChunkReply { first: 2 },
        VfError::transport(None, "directory LOOKUP reply lost"),
    )])));
    let results = fs.vopen_dirs_impl(&refs).unwrap();
    assert_eq!(results.len(), paths.len());
    assert!(results[0].is_ok() && results[1].is_ok());
    assert_eq!(fs.open_dirs.len(), 2);
    for result in &results[2..4] {
        assert_eq!(result.as_ref().unwrap_err().state(), FailureState::Unknown);
    }
    assert_eq!(
        results[4].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    for result in results.into_iter().flatten() {
        fs.close_dir_impl(&result).unwrap();
    }
    assert!(fs.open_dirs.is_empty());
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    fs.nfs.max_ops = max_ops;
    fs.remove_impl(&base, true).unwrap();
}

#[test]
#[ignore = "manual paired directory acquisition performance control"]
fn directory_open_paired_batching_control() {
    let Some(mut fs) = backend() else {
        return;
    };
    let base = PathBuf::from(format!("/directory-open-benchmark-{}", std::process::id()));
    fs.ensure_dir_impl(&base, 0o755).unwrap();
    let paths: Vec<_> = (0..24)
        .map(|index| base.join(format!("d{index:02}")))
        .collect();
    for path in &paths {
        fs.ensure_dir_impl(path, 0o755).unwrap();
    }
    for mixed in [false, true] {
        let mut inputs = paths.clone();
        if mixed {
            inputs[12] = base.join("missing");
        }
        let refs: Vec<_> = inputs.iter().map(PathBuf::as_path).collect();
        for sample in 0..3 {
            for vector in if sample % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let _ = crate::compound::thread_compound_stats();
                let start = std::time::Instant::now();
                let outcomes = if vector {
                    fs.vopen_dirs_impl(&refs).unwrap()
                } else {
                    refs.iter()
                        .map(|path| {
                            fs.open_dir_impl(path)
                                .map_err(|cause| vfsi_core::OpError::failed(cause, None))
                        })
                        .collect()
                };
                for (index, result) in outcomes.into_iter().enumerate() {
                    if mixed && index == 12 {
                        assert_eq!(
                            result.unwrap_err().cause().kind(),
                            std::io::ErrorKind::NotFound
                        );
                    } else {
                        fs.close_dir_impl(&result.unwrap()).unwrap();
                    }
                }
                let elapsed = start.elapsed();
                let (requests, operations, _, _) = crate::compound::thread_compound_stats();
                assert!(fs.open_dirs.is_empty());
                println!(
                    "directory-open mixed={mixed} vector={vector} sample={sample} wall_us={} requests={requests} operations={operations}",
                    elapsed.as_micros()
                );
            }
        }
    }
    fs.remove_impl(&base, true).unwrap();
}
