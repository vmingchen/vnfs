use std::io::ErrorKind as K;
use vfsi_core::{ErrorDomain, StatusCode, TransportKind, VfError};

#[test]
fn io_sources_and_semantic_diagnostics_survive_cloning_and_context() {
    use std::error::Error;
    let error = VfError::from_io(None, std::io::Error::new(K::TimedOut, "local deadline"))
        .with_context("vread", "/file");
    assert_eq!(error.kind(), K::TimedOut);
    assert_eq!(error.domain(), ErrorDomain::Client);
    assert_eq!(error.status(), None);
    let clone = error.clone().with_index(4);
    let source = clone
        .source()
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert_eq!(source.kind(), K::TimedOut);
    assert_eq!(source.to_string(), "local deadline");
    let native = VfError::from_io(2, std::io::Error::from_raw_os_error(libc::EACCES));
    assert_eq!(
        native.status(),
        Some(StatusCode::Errno(libc::EACCES as u32))
    );
    let semantic = VfError::nfs(3, 13).with_message("denied by export policy");
    assert!(semantic.to_string().contains("denied by export policy"));
    assert_eq!(semantic.status(), Some(StatusCode::Nfs(13)));
}

#[test]
fn errno_translation_never_relabels_a_native_status() {
    for (error, errno) in [
        (VfError::nfs(None, 63), libc::ENAMETOOLONG),
        (VfError::nfs(None, 69), libc::EDQUOT),
        (VfError::nfs(None, 70), libc::ESTALE),
        (VfError::smb(None, 0xc000_0043), libc::EBUSY),
        (VfError::unsupported(None), libc::EOPNOTSUPP),
    ] {
        let before = error.status();
        assert_eq!(error.to_errno(), errno);
        assert_eq!(error.status(), before);
        assert_eq!(error.index(), None);
    }
}

#[cfg(target_pointer_width = "64")]
#[test]
fn status_only_results_keep_cold_diagnostics_out_of_line() {
    assert!(size_of::<VfError>() <= 48);
    assert!(size_of::<Result<Vec<u8>, VfError>>() <= 48);
}

#[test]
fn common_native_statuses_have_portable_categories_without_losing_status() {
    for (status, kind) in [
        (StatusCode::Nfs(69), K::QuotaExceeded),
        (StatusCode::Nfs(70), K::StaleNetworkFileHandle),
        (StatusCode::Smb(0xc000_0043), K::ResourceBusy),
        (StatusCode::Smb(0xc000_006d), K::PermissionDenied),
    ] {
        let error = VfError::from_status(None, status);
        assert_eq!(error.kind(), kind, "{status:?}");
        assert_eq!(error.status(), Some(status));
        assert_eq!(error.index(), None);
    }
}

#[test]
fn unsupported_is_a_client_category_not_a_fabricated_errno() {
    let error = VfError::unsupported(3);
    assert_eq!(error.kind(), K::Unsupported);
    assert_eq!(error.domain(), ErrorDomain::Client);
    assert_eq!(error.status(), None);
    assert!(!error.is_transport());
}

#[test]
fn rpc_transport_categories_do_not_change_when_remapping_indices() {
    let error = VfError::transport_with_kind(None, TransportKind::InvalidReply, "bad length")
        .map_index(|i| i + 1);
    assert_eq!(error.index(), None);
    assert_eq!(error.kind(), K::InvalidData);
}

#[test]
fn opaque_nonclone_source_is_retained_until_its_final_owner_drops() {
    use std::{
        error::Error,
        fmt,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    #[derive(Debug)]
    struct Opaque(Arc<AtomicUsize>);
    impl fmt::Display for Opaque {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("opaque cause")
        }
    }
    impl Error for Opaque {}
    impl Drop for Opaque {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let source = std::io::Error::new(K::TimedOut, Opaque(drops.clone()));
    let cause = VfError::from_io(None, source).with_context("vread", "/opaque");
    let error = vfsi_core::OpError::unknown(cause, None);
    let (cause, state, progress) = error.into_parts();
    assert_eq!(state, vfsi_core::FailureState::Unknown);
    assert!(progress.is_none());
    assert_eq!(cause.kind(), K::TimedOut);
    assert!(
        !cause.is_transport(),
        "an OS category is not RPC provenance"
    );
    let source = cause
        .source()
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert!(source.get_ref().unwrap().is::<Opaque>());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(cause);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn context_does_not_change_native_domain_or_transport_provenance() {
    for cause in [
        VfError::nfs(None, 13),
        VfError::smb(None, 0xc000_0022),
        VfError::errno(None, libc::EACCES as u32),
        VfError::transport_with_kind(None, TransportKind::Timeout, "RPC deadline"),
    ] {
        let facts = (cause.kind(), cause.status(), cause.transport_kind());
        let cause = cause
            .with_context("first", "/first")
            .with_context("second", "/second")
            .with_message("detail");
        assert_eq!(
            (cause.kind(), cause.status(), cause.transport_kind()),
            facts
        );
        assert_eq!(cause.path(), Some(std::path::Path::new("/second")));
        assert_eq!(cause.operation(), Some("second"));
    }
}
