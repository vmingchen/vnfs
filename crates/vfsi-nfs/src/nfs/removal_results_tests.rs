use super::*;
use std::sync::{Arc, Mutex};
use vfsi_core::{
    FailureState, OpError,
    internal::faults::{FaultInjector, FaultScript, OpenFaultPoint},
};

struct SelectedRemoveFault {
    after: bool,
    active: Mutex<bool>,
    names: Mutex<Vec<Vec<u8>>>,
}
impl FaultInjector for SelectedRemoveFault {
    fn check(&self, point: &OpenFaultPoint) -> VfResult<()> {
        match point {
            OpenFaultPoint::BeforeRemoveChunk { first_name } => {
                self.names.lock().unwrap().push(first_name.clone());
                let selected = first_name == b"fault";
                *self.active.lock().unwrap() = selected;
                if selected && !self.after {
                    return Err(VfError::transport(
                        None,
                        "selected REMOVE rejected before dispatch",
                    ));
                }
            }
            OpenFaultPoint::AfterRemoveCompoundReply { .. }
                if self.after && std::mem::take(&mut *self.active.lock().unwrap()) =>
            {
                return Err(VfError::transport(None, "selected REMOVE reply lost"));
            }
            _ => {}
        }
        Ok(())
    }
}

fn backend() -> Option<NfsVecFs> {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    Some(NfsVecFs::connect_minor(&server, 1).unwrap())
}
fn create(fs: &mut NfsVecFs, path: &Path) {
    let file = fs
        .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
        .unwrap();
    fs.write_raw_impl(&file, 0, b"data").unwrap();
    fs.close_impl(&file).unwrap();
}
fn task(fs: &mut NfsVecFs, path: &Path, root: usize) -> RmTask {
    let (parent, name) = fs.remove_parent_handle(path).unwrap();
    RmTask::RemoveOrEnter {
        parent,
        name: name.into_boxed_slice(),
        root,
        expand: true,
    }
}
fn pending(count: usize) -> Vec<RemoveRoot> {
    (0..count).map(|_| RemoveRoot::Pending).collect()
}

#[test]
fn recursive_root_results_keep_ordinary_failures_independent() {
    let Some(mut fs) = backend() else {
        return;
    };
    for continuing in [false, true] {
        let base = PathBuf::from(format!(
            "/root-remove-results-{}-{continuing}",
            std::process::id()
        ));
        let first = base.join("first");
        let missing = base.join("missing");
        let last = base.join("last");
        fs.ensure_dir_impl(&first.join("nested/deeper"), 0o755)
            .unwrap();
        create(&mut fs, &first.join("leaf"));
        create(&mut fs, &first.join("nested/deeper/leaf"));
        create(&mut fs, &last);
        let tasks = vec![
            task(&mut fs, &first, 0),
            task(&mut fs, &missing, 1),
            task(&mut fs, &last, 2),
        ];
        let results = fs.run_rm_tasks(
            tasks,
            pending(3),
            RemoveOptions::new().with_continue_on_error(continuing),
        );
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok() && results[2].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Failed);
        assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
        assert!(!fs.exists_impl(&first).unwrap());
        assert!(!fs.exists_impl(&last).unwrap());
        {
            let remove_roots: &[&std::path::Path] = &[&base];
            vfsi_core::internal::collect_vec_result(
                fs.remove_paths_impl(remove_roots, true),
                remove_roots.len(),
            )
            .map(|_| ())
        }
        .unwrap();
    }
}

#[test]
fn recursive_root_results_distinguish_complete_active_unknown_and_untouched() {
    let Some(mut fs) = backend() else {
        return;
    };
    for after in [false, true] {
        let base = PathBuf::from(format!("/root-remove-loss-{}-{after}", std::process::id()));
        let active = base.join("active");
        let fault = base.join("fault");
        let complete = base.join("complete");
        let untouched = base.join("untouched");
        fs.ensure_dir_impl(&active, 0o755).unwrap();
        create(&mut fs, &active.join("leaf"));
        for path in [&fault, &complete, &untouched] {
            create(&mut fs, path);
        }
        // Complete a file, then drain a tree whose final entry is queued;
        // fail the next root before the final ready-parent batch can run.
        let tasks = vec![
            task(&mut fs, &untouched, 3),
            task(&mut fs, &fault, 1),
            task(&mut fs, &active, 0),
            task(&mut fs, &complete, 2),
        ];
        let injector = Arc::new(SelectedRemoveFault {
            after,
            active: Mutex::new(false),
            names: Mutex::new(Vec::new()),
        });
        fs.set_fault_injector(injector.clone());
        let results = fs.run_rm_tasks(tasks, pending(4), RemoveOptions::new());
        assert_eq!(results.len(), 4);
        assert_eq!(
            results[0].as_ref().unwrap_err().state(),
            FailureState::Failed,
            "confirmed child removals do not imply root completion"
        );
        assert_eq!(
            results[1].as_ref().unwrap_err().state(),
            if after {
                FailureState::Unknown
            } else {
                FailureState::NotAttempted
            }
        );
        assert!(
            results[2].is_ok(),
            "a completed root is retained across fatal failure"
        );
        assert_eq!(
            results[3].as_ref().unwrap_err().state(),
            FailureState::NotAttempted
        );
        for error in results.iter().filter_map(|result| result.as_ref().err()) {
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        }
        assert!(fs.exists_impl(&active).unwrap());
        assert!(
            fs.listdir_impl(&active, AttrMask::empty(), 0, false)
                .unwrap()
                .is_empty()
        );
        assert!(!fs.exists_impl(&complete).unwrap());
        assert_eq!(fs.exists_impl(&fault).unwrap(), !after);
        assert!(fs.exists_impl(&untouched).unwrap());
        let names = injector.names.lock().unwrap();
        for name in [b"complete".as_slice(), b"fault"] {
            assert_eq!(
                names
                    .iter()
                    .filter(|observed| observed.as_slice() == name)
                    .count(),
                1,
                "completed and uncertain roots are never replayed"
            );
        }
        assert!(!names.iter().any(|name| name == b"untouched"));
        drop(names);
        fs.set_fault_injector(Arc::new(FaultScript::new([])));
        {
            let remove_roots: &[&std::path::Path] = &[&base];
            vfsi_core::internal::collect_vec_result(
                fs.remove_paths_impl(remove_roots, true),
                remove_roots.len(),
            )
            .map(|_| ())
        }
        .unwrap();
    }
}

#[test]
fn recursive_root_results_handle_empty_and_predispatch_rejection_without_io() {
    let Some(mut fs) = backend() else {
        return;
    };
    let _ = crate::compound::thread_compound_stats();
    assert!(
        fs.run_rm_tasks(Vec::new(), Vec::new(), RemoveOptions::new())
            .is_empty()
    );
    let results = fs.run_rm_tasks(
        Vec::new(),
        vec![
            RemoveRoot::Failed(OpError::not_attempted(VfError::transport(
                None,
                "preflight rejected",
            ))),
            RemoveRoot::Pending,
        ],
        RemoveOptions::new(),
    );
    assert!(
        results
            .iter()
            .all(|result| result.as_ref().unwrap_err().state() == FailureState::NotAttempted)
    );
    assert_eq!(crate::compound::thread_compound_stats().0, 0);
}
