//! Final cause representation, exercised before the public alias cutover.
use std::{
    error::Error,
    fmt,
    io::{self, ErrorKind as K},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use vfsi_core::internal::errors::*;
use vfsi_core::{StatusCode, TransportKind, native_status, transport_kind};

#[derive(Debug)]
struct Opaque(Arc<AtomicUsize>);
impl fmt::Display for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("opaque original")
    }
}
impl Error for Opaque {}
impl Drop for Opaque {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn protocol_categories_preserve_namespace_and_never_become_os_errno() {
    for (status, kind) in [
        (StatusCode::Nfs(13), K::PermissionDenied),
        (StatusCode::Nfs(69), K::QuotaExceeded),
        (StatusCode::Nfs(10019), K::Unsupported),
        (StatusCode::Smb(0xc000_0043), K::ResourceBusy),
        (StatusCode::Smb(0xc000_0257), K::Unsupported),
        (StatusCode::Smb(0xfeed_beef), K::Other),
    ] {
        let error = from_status(status);
        assert_eq!(error.kind(), kind);
        assert_eq!(error.raw_os_error(), None);
        assert_eq!(native_status(&error), Some(status));
        assert_eq!(transport_kind(&error), None);
        let contextual = with_context(error, "vread", "/file");
        assert_eq!(contextual.kind(), kind);
        assert_eq!(native_status(&contextual), Some(status));
        assert_eq!(operation(&contextual), Some("vread"));
        assert_eq!(path(&contextual), Some(Path::new("/file")));
    }
}

#[test]
fn original_os_errno_survives_context_while_raw_os_error_stays_honest() {
    let error = from_status(StatusCode::Errno(libc::EACCES as u32));
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    let error = with_context(error, "vopen", "/denied");
    assert_eq!(error.raw_os_error(), None);
    assert_eq!(
        native_status(&error),
        Some(StatusCode::Errno(libc::EACCES as u32))
    );
    assert_eq!(to_errno(&error), libc::EACCES);
    let source = error
        .get_ref()
        .unwrap()
        .source()
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(source.raw_os_error(), Some(libc::EACCES));
}

#[test]
fn provenance_is_not_inferred_from_category_or_message() {
    let local = io::Error::new(K::TimedOut, "RPC timeout");
    assert_eq!(transport_kind(&local), None);
    let error = transport(TransportKind::Timeout, "local-looking deadline");
    assert_eq!(error.kind(), K::TimedOut);
    assert_eq!(transport_kind(&error), Some(TransportKind::Timeout));
    let error = with_context(error, "vread", "/file");
    assert_eq!(transport_kind(&error), Some(TransportKind::Timeout));
    assert_eq!(message(&error), Some("local-looking deadline"));
    assert_eq!(native_status(&error), None);
}

#[test]
fn context_updates_reuse_the_existing_payload_and_retain_opaque_source() {
    let drops = Arc::new(AtomicUsize::new(0));
    let error = io::Error::new(K::TimedOut, Opaque(drops.clone()));
    let mut error = with_context(error, "first", "/first");
    let pointer = error.get_ref().unwrap() as *const _ as *const ();
    for _ in 0..100 {
        error = with_message(with_context(error, "second", "/second"), "detail");
        assert_eq!(error.get_ref().unwrap() as *const _ as *const (), pointer);
    }
    let original = error
        .get_ref()
        .unwrap()
        .source()
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert!(original.get_ref().unwrap().is::<Opaque>());
    assert_eq!(error.kind(), K::TimedOut);
    assert_eq!(path(&error), Some(Path::new("/second")));
    assert_eq!(message(&error), Some("detail"));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn opaque_fanout_retains_original_source_until_all_owners_drop() {
    let drops = Arc::new(AtomicUsize::new(0));
    let original = io::Error::new(K::PermissionDenied, Opaque(drops.clone()));
    let original_pointer = original.get_ref().unwrap() as *const _ as *const ();
    let mut errors: Vec<_> = fanout(with_context(original, "vopen", "/group"), 3).collect();
    for error in &errors {
        assert_eq!(error.kind(), K::PermissionDenied);
        assert_eq!(path(error), Some(Path::new("/group")));
        let context = error
            .get_ref()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        let original = context
            .get_ref()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(
            original.get_ref().unwrap() as *const _ as *const (),
            original_pointer
        );
        assert_eq!(native_status(error), None);
    }
    let final_owner = errors.pop().unwrap();
    drop(errors);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(final_owner);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn singleton_fanout_moves_the_original_payload_without_wrapping() {
    let drops = Arc::new(AtomicUsize::new(0));
    let error = io::Error::other(Opaque(drops.clone()));
    let pointer = error.get_ref().unwrap() as *const _ as *const ();
    let mut errors = fanout(error, 1);
    assert_eq!(errors.len(), 1);
    let error = errors.next().unwrap();
    assert_eq!(error.get_ref().unwrap() as *const _ as *const (), pointer);
    assert!(errors.next().is_none());
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn empty_fanout_releases_the_unused_original() {
    let drops = Arc::new(AtomicUsize::new(0));
    let errors = fanout(io::Error::other(Opaque(drops.clone())), 0);
    assert_eq!(errors.len(), 0);
    drop(errors);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn known_native_fanout_preserves_exact_status_and_category() {
    for status in [
        StatusCode::Errno(libc::ENOENT as u32),
        StatusCode::Nfs(13),
        StatusCode::Smb(0xc000_0043),
    ] {
        let original = from_status(status);
        let kind = original.kind();
        for error in fanout(original, 4) {
            assert_eq!(native_status(&error), Some(status));
            assert_eq!(error.kind(), kind);
            assert_eq!(transport_kind(&error), None);
        }
    }
}

fn recategorized(error: io::Error) -> io::Error {
    io::Error::new(K::PermissionDenied, error.into_inner().unwrap())
}

#[test]
fn fanout_preserves_recategorized_native_and_shared_errors() {
    for status in [StatusCode::Nfs(2), StatusCode::Smb(0xc000_0034)] {
        for shared in [false, true] {
            for count in [1, 2, 4] {
                let original = from_status(status);
                let original = if shared {
                    from_shared(&shared_source(original))
                } else {
                    original
                };
                let original = recategorized(original);
                let outcomes: Vec<_> = fanout(original, count).collect();
                assert_eq!(outcomes.len(), count);
                for error in outcomes {
                    assert_eq!(error.kind(), K::PermissionDenied);
                    assert_eq!(native_status(&error), Some(status));
                    assert_eq!(transport_kind(&error), None);
                }
            }
        }
    }
}

#[test]
fn explicit_sharing_preserves_recategorized_causes_and_reuses_the_new_group() {
    for in_place in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let original = with_context(
            io::Error::new(K::TimedOut, Opaque(drops.clone())),
            "vread",
            "/shared",
        );
        let original = from_shared(&shared_source(original));
        let original = recategorized(original);
        let mut owner = Some(original);
        let source = if in_place {
            let original = owner.as_mut().unwrap();
            let source = share_in_place(original);
            assert_eq!(original.kind(), K::PermissionDenied);
            let payload = original.get_ref().unwrap() as *const _ as *const ();
            let second = share_in_place(original);
            assert!(Arc::ptr_eq(&source, &second));
            assert_eq!(
                original.get_ref().unwrap() as *const _ as *const (),
                payload
            );
            source
        } else {
            shared_source(owner.take().unwrap())
        };
        assert_eq!(source.kind(), K::PermissionDenied);
        for _ in 0..2 {
            let sibling = from_shared(&source);
            assert_eq!(sibling.kind(), K::PermissionDenied);
            assert_eq!(operation(&sibling), Some("vread"));
            assert_eq!(path(&sibling), Some(Path::new("/shared")));
            assert_eq!(message(&sibling), None);
            assert_eq!(native_status(&sibling), None);
            let reused = shared_source(sibling);
            assert!(Arc::ptr_eq(&source, &reused));
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(source);
        // In-place sharing still leaves the original slot owning its source.
        // In the consuming branch that slot has already moved into the group.
        drop(owner);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn recategorized_transport_keeps_provenance_across_sharing_and_fanout() {
    for in_place in [false, true] {
        let cause = transport(TransportKind::Timeout, "deadline");
        let shared = from_shared(&shared_source(cause));
        let mut cause = recategorized(shared);
        let source = if in_place {
            share_in_place(&mut cause)
        } else {
            shared_source(cause)
        };
        let mut siblings = fanout(from_shared(&source), 3);
        assert_eq!(siblings.len(), 3);
        for sibling in &mut siblings {
            assert_eq!(sibling.kind(), K::PermissionDenied);
            assert_eq!(transport_kind(&sibling), Some(TransportKind::Timeout));
            assert_eq!(native_status(&sibling), None);
            assert_eq!(message(&sibling), Some("deadline"));
        }
    }
}

#[test]
fn repeated_group_sharing_reuses_the_original_source() {
    let initial = with_context(from_status(StatusCode::Nfs(69)), "vwrite", "/file");
    let mut group = fanout(initial, 2);
    let error = group.next().unwrap();
    let pointer = error.get_ref().unwrap().source().unwrap() as *const _ as *const ();
    for error in fanout(error, 2) {
        assert_eq!(native_status(&error), Some(StatusCode::Nfs(69)));
        assert_eq!(path(&error), Some(Path::new("/file")));
        assert_eq!(
            error.get_ref().unwrap().source().unwrap() as *const _ as *const (),
            pointer
        );
    }
}

#[test]
fn protocol_rpc_conversion_does_not_import_wire_indices() {
    for index in [0, 17, usize::MAX] {
        let error = from_rpc(vfsi_core::RpcError::op(index, 13));
        assert_eq!(native_status(&error), Some(StatusCode::Nfs(13)));
        assert_eq!(error.kind(), K::PermissionDenied);
        assert!(!error.to_string().contains("op "));
    }
    let error = from_rpc(vfsi_core::RpcError::transport_with_kind(
        TransportKind::Auth,
        "denied",
    ));
    assert_eq!(transport_kind(&error), Some(TransportKind::Auth));
    assert_eq!(error.kind(), K::PermissionDenied);
}

#[test]
fn foreign_source_chains_are_not_mined_for_native_facts() {
    #[derive(Debug)]
    struct Foreign(io::Error);
    impl fmt::Display for Foreign {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(f)
        }
    }
    impl Error for Foreign {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }
    let error = io::Error::other(Foreign(from_status(StatusCode::Nfs(13))));
    assert_eq!(native_status(&error), None);
    let error = with_context(error, "foreign", "/file");
    assert_eq!(native_status(&error), None);
    assert_eq!(error.kind(), K::Other);
}

#[test]
fn standard_errors_and_their_payloads_are_send_sync() {
    fn require<T: Send + Sync>(_: &T) {}
    require(&from_status(StatusCode::Nfs(13)));
    require(&with_context(
        transport(TransportKind::Timeout, "deadline"),
        "vread",
        "/file",
    ));
    for error in fanout(io::Error::other(Opaque(Arc::new(AtomicUsize::new(0)))), 2) {
        require(&error);
    }
}

#[test]
fn shared_transport_diagnostics_survive_context_and_repeated_fanout() {
    for error in fanout(transport(TransportKind::Timeout, "original deadline"), 2) {
        assert_eq!(message(&error), Some("original deadline"));
        for error in fanout(with_context(error, "vread", "/file"), 2) {
            assert_eq!(message(&error), Some("original deadline"));
            assert_eq!(transport_kind(&error), Some(TransportKind::Timeout));
            assert_eq!(operation(&error), Some("vread"));
            assert_eq!(path(&error), Some(Path::new("/file")));
        }
    }
}

#[test]
fn sharing_an_operation_cause_keeps_progress_owned_and_does_not_nest_groups() {
    let drops = Arc::new(AtomicUsize::new(0));
    let data = vec![1, 2, 3];
    let pointer = data.as_ptr();
    let mut error = vfsi_core::OpError::unknown(
        io::Error::new(K::TimedOut, Opaque(drops.clone())),
        Some(vfsi_core::IoProgress::Read(
            vfsi_core::api::ReadOutput::owned(7, data, false),
        )),
    );
    let first = share_op_cause(&mut error);
    let payload = error.cause().get_ref().unwrap() as *const _ as *const ();
    let second = share_op_cause(&mut error);
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(
        error.cause().get_ref().unwrap() as *const _ as *const (),
        payload
    );
    assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
    let Some(vfsi_core::IoProgress::Read(read)) = error.progress() else {
        panic!("missing progress")
    };
    assert_eq!(read.data().unwrap().as_ptr(), pointer);
    let sibling = from_shared(&first);
    assert_eq!(sibling.kind(), K::TimedOut);
    assert_eq!(transport_kind(&sibling), None);
    drop(first);
    drop(second);
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(sibling);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn standard_io_interruption_suppression_keeps_the_original_cause() {
    let error = suppress_interrupted(io::Error::from_raw_os_error(libc::EINTR));
    assert_eq!(error.kind(), K::Other);
    assert_eq!(
        native_status(&error),
        Some(StatusCode::Errno(libc::EINTR as u32))
    );
    let original = error
        .get_ref()
        .unwrap()
        .source()
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(original.raw_os_error(), Some(libc::EINTR));
    let drops = Arc::new(AtomicUsize::new(0));
    let original = io::Error::new(K::PermissionDenied, Opaque(drops.clone()));
    let pointer = original.get_ref().unwrap() as *const _ as *const ();
    let error = suppress_interrupted(original);
    assert_eq!(error.get_ref().unwrap() as *const _ as *const (), pointer);
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
