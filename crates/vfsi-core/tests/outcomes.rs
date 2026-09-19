use std::path::Path;

use vfsi_core::{
    AttrMask, ErrorDomain, Metadata, OutcomeCertainty, RetryClass, StatusCode, VfAttrs, VfError,
};

#[test]
fn transport_failure_is_ambiguous_and_requires_reconciliation() {
    let error = VfError::transport(None, "connection reset").with_context("rename", "/target");
    assert_eq!(error.index_opt(), None);
    assert_eq!(error.domain(), ErrorDomain::Transport);
    assert_eq!(error.status(), None);
    assert_eq!(error.certainty(), OutcomeCertainty::Indeterminate);
    assert_eq!(error.retry_class(), RetryClass::ReconcileFirst);
    assert_eq!(error.operation(), Some("rename"));
    assert_eq!(error.path(), Some(Path::new("/target")));
}

#[test]
fn rpc_transport_placeholder_is_not_exposed_as_request_zero() {
    let error = VfError::from_rpc_indexed(vfsi_core::RpcError::transport("connection reset"));
    assert!(error.is_transport());
    assert_eq!(error.index_opt(), None);
}

#[test]
fn index_mapping_preserves_unknown_transport_location() {
    let unknown = VfError::transport(None, "connection reset").map_index(|index| index + 10);
    assert_eq!(unknown.index_opt(), None);

    let known = VfError::failure(2, libc::ENOENT as u32).map_index(|index| index + 10);
    assert_eq!(known.index_opt(), Some(12));
}

#[test]
fn nfs_status_is_not_conflated_with_errno() {
    let error = VfError::from_rpc(vfsi_core::RpcError::op(3, 10044), Some(7));
    assert_eq!(error.domain(), ErrorDomain::Nfs);
    assert_eq!(error.status(), Some(StatusCode::Nfs(10044)));
    assert_eq!(error.index_opt(), Some(7));
}

#[test]
fn metadata_converts_fractional_pre_epoch_timestamps() {
    let attributes = VfAttrs {
        returned: AttrMask::ATIME,
        atime_sec: -1,
        atime_nsec: 500_000_000,
        ..VfAttrs::default()
    };
    let metadata = Metadata::from(attributes);
    assert_eq!(
        metadata.accessed(),
        Some(std::time::UNIX_EPOCH - std::time::Duration::from_millis(500))
    );
}
