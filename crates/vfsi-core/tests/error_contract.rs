use std::io::ErrorKind as K;
use vfsi_core::{StatusCode, TransportKind};

#[test]
fn io_sources_and_semantic_diagnostics_survive_moving_and_context() {
    use std::error::Error;
    let error = vfsi_core::internal::errors::with_context(
        std::io::Error::new(K::TimedOut, "local deadline"),
        "vread",
        "/file",
    );
    assert_eq!(error.kind(), K::TimedOut);
    assert_eq!(vfsi_core::transport_kind(&error), None);
    assert_eq!(vfsi_core::native_status(&(error)), None);
    let moved = error;
    let source = moved
        .source()
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert_eq!(source.kind(), K::TimedOut);
    assert_eq!(source.to_string(), "local deadline");
    let native = std::io::Error::from_raw_os_error(libc::EACCES);
    assert_eq!(
        vfsi_core::native_status(&(native)),
        Some(StatusCode::Errno(libc::EACCES as u32))
    );
    let semantic = vfsi_core::internal::errors::with_message(
        vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(13)),
        "denied by export policy",
    );
    assert!(semantic.to_string().contains("denied by export policy"));
    assert_eq!(
        vfsi_core::native_status(&(semantic)),
        Some(StatusCode::Nfs(13))
    );
}

#[test]
fn errno_translation_never_relabels_a_native_status() {
    for (error, errno) in [
        (
            vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(63)),
            libc::ENAMETOOLONG,
        ),
        (
            vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(69)),
            libc::EDQUOT,
        ),
        (
            vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(70)),
            libc::ESTALE,
        ),
        (
            vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Smb(0xc000_0043)),
            libc::EBUSY,
        ),
        (
            std::io::Error::from(std::io::ErrorKind::Unsupported),
            libc::EOPNOTSUPP,
        ),
    ] {
        let before = vfsi_core::native_status(&(error));
        assert_eq!(vfsi_core::internal::errors::to_errno(&(error)), errno);
        assert_eq!(vfsi_core::native_status(&(error)), before);
    }
}

#[cfg(target_pointer_width = "64")]
#[test]
fn status_only_results_keep_cold_diagnostics_out_of_line() {
    assert!(size_of::<std::io::Error>() <= 8);
    assert!(size_of::<Result<Vec<u8>, std::io::Error>>() <= 32);
}

#[test]
fn common_native_statuses_have_portable_categories_without_losing_status() {
    for (status, kind) in [
        (StatusCode::Nfs(69), K::QuotaExceeded),
        (StatusCode::Nfs(70), K::StaleNetworkFileHandle),
        (StatusCode::Smb(0xc000_0043), K::ResourceBusy),
        (StatusCode::Smb(0xc000_006d), K::PermissionDenied),
    ] {
        let error = vfsi_core::internal::errors::from_status(status);
        assert_eq!(error.kind(), kind, "{status:?}");
        assert_eq!(vfsi_core::native_status(&(error)), Some(status));
    }
}

#[test]
fn unsupported_is_a_client_category_not_a_fabricated_errno() {
    let error = std::io::Error::from(std::io::ErrorKind::Unsupported);
    assert_eq!(error.kind(), K::Unsupported);
    assert_eq!(vfsi_core::transport_kind(&error), None);
    assert_eq!(vfsi_core::native_status(&(error)), None);
    assert!(!vfsi_core::transport_kind(&(error)).is_some());
}

#[test]
fn rpc_transport_categories_do_not_change_when_remapping_indices() {
    let error = vfsi_core::IndexedError::unattributed(vfsi_core::internal::errors::transport(
        TransportKind::InvalidReply,
        "bad length",
    ))
    .map_index(|i| i + 1);
    assert_eq!(error.index(), None);
    assert_eq!(error.cause().kind(), K::InvalidData);
}

#[test]
fn rpc_io_conversion_preserves_typed_transport_classification() {
    use vfsi_core::{RpcError, internal::errors};

    for kind in [
        TransportKind::Auth,
        TransportKind::InvalidReply,
        TransportKind::Connection,
    ] {
        let cause = errors::with_context(
            errors::transport(kind, "transport detail"),
            "vread",
            "/file",
        );
        let category = cause.kind();
        let converted = errors::from_rpc(RpcError::from_io(cause));
        assert_eq!(vfsi_core::transport_kind(&converted), Some(kind));
        assert_eq!(converted.kind(), category);
        assert!(converted.to_string().contains("transport detail"));
    }
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
    let cause = vfsi_core::internal::errors::with_context(source, "vread", "/opaque");
    let error = vfsi_core::OpError::unknown(cause, None);
    let (cause, state, progress) = error.into_parts();
    assert_eq!(state, vfsi_core::FailureState::Unknown);
    assert!(progress.is_none());
    assert_eq!(cause.kind(), K::TimedOut);
    assert!(
        !vfsi_core::transport_kind(&(cause)).is_some(),
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
        vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(13)),
        vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Smb(0xc000_0022)),
        std::io::Error::from_raw_os_error(libc::EACCES),
        vfsi_core::internal::errors::transport(TransportKind::Timeout, "RPC deadline"),
    ] {
        let facts = (
            cause.kind(),
            vfsi_core::native_status(&(cause)),
            vfsi_core::transport_kind(&(cause)),
        );
        let cause = vfsi_core::internal::errors::with_message(
            vfsi_core::internal::errors::with_context(
                vfsi_core::internal::errors::with_context(cause, "first", "/first"),
                "second",
                "/second",
            ),
            "detail",
        );
        assert_eq!(
            (
                cause.kind(),
                vfsi_core::native_status(&(cause)),
                vfsi_core::transport_kind(&(cause))
            ),
            facts
        );
        assert_eq!(
            vfsi_core::internal::errors::path(&(cause)),
            Some(std::path::Path::new("/second"))
        );
        assert_eq!(
            vfsi_core::internal::errors::operation(&(cause)),
            Some("second")
        );
    }
}
