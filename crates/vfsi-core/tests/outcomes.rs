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
    assert_eq!(
        error.failed_item_certainty(),
        OutcomeCertainty::Indeterminate
    );
    assert_eq!(error.failed_item_retry_class(), RetryClass::ReconcileFirst);
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
fn status_without_a_known_index_is_not_reported_as_operation_zero() {
    // A compound-level status (for example NFS4ERR_MINOR_VERS_MISMATCH) has no
    // per-operation result, so `None` must stay unattributed rather than
    // silently becoming operation 0.
    let error = VfError::from_rpc(vfsi_core::RpcError::op(0, 10021), None);
    assert!(!error.is_transport());
    assert_eq!(error.index_opt(), None);
    assert_eq!(error.domain(), ErrorDomain::Nfs);
    assert_eq!(error.status(), Some(StatusCode::Nfs(10021)));
    assert_eq!(error.err_no(), 10021);

    // Re-attributing turns it into a concrete operation failure.
    let attributed = error.with_index(5);
    assert_eq!(attributed.index_opt(), Some(5));
    assert_eq!(attributed.status(), Some(StatusCode::Nfs(10021)));

    // An explicitly indexed status is still attributed directly.
    let indexed = VfError::from_rpc(vfsi_core::RpcError::op(0, 10021), Some(0));
    assert_eq!(indexed.index_opt(), Some(0));
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

#[test]
fn protocol_status_constructors_preserve_their_domains() {
    let nfs = VfError::nfs(4, 10_001);
    assert_eq!(nfs.index_opt(), Some(4));
    assert_eq!(nfs.status(), Some(StatusCode::Nfs(10_001)));

    let smb = VfError::smb(2, 0xC000_0054);
    assert_eq!(smb.index_opt(), Some(2));
    assert_eq!(smb.status(), Some(StatusCode::Smb(0xC000_0054)));
}
