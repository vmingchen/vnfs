use std::path::Path;

use vfsi_core::{AttrMask, ErrorDomain, Metadata, StatusCode, VfAttrs, VfError};

#[test]
fn transport_failure_is_ambiguous_and_requires_reconciliation() {
    let error = VfError::transport(None, "connection reset").with_context("rename", "/target");
    assert_eq!(error.index(), None);
    assert_eq!(error.domain(), ErrorDomain::Transport);
    assert_eq!(error.status(), None);
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert!(error.to_string().contains("rename"));
    assert!(error.to_string().contains("/target"));
    assert_eq!(error.operation(), Some("rename"));
    assert_eq!(error.path(), Some(Path::new("/target")));
}

#[test]
fn portable_error_kinds_preserve_protocol_status_and_source() {
    use std::io::ErrorKind as K;
    for (error, kind) in [
        (VfError::nfs(2, 28), K::StorageFull),
        (VfError::nfs(2, 10004), K::Unsupported),
        (VfError::smb(2, 0xc000_0022), K::PermissionDenied),
        (VfError::smb(2, 0xc000_0034), K::NotFound),
        // Same numeric value in SMB is not an errno.
        (VfError::smb(2, 2), K::Other),
        (VfError::client(2, libc::ENOENT as u32), K::NotFound),
        (VfError::unsupported(2), K::Unsupported),
    ] {
        assert_eq!(error.kind(), kind);
        let converted: std::io::Error = error.clone().into();
        assert_eq!(converted.kind(), kind);
        assert_eq!(
            converted.get_ref().unwrap().downcast_ref::<VfError>(),
            Some(&error)
        );
    }
    let display = VfError::nfs(2, 28)
        .with_context("writev", "/file")
        .to_string();
    for context in ["writev", "/file", "Nfs", "28"] {
        assert!(display.contains(context));
    }
}

#[test]
fn rpc_transport_placeholder_is_not_exposed_as_request_zero() {
    let error = VfError::from_rpc_indexed(vfsi_core::RpcError::transport("connection reset"));
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
}

#[test]
fn index_mapping_preserves_unknown_transport_location() {
    let unknown = VfError::transport(None, "connection reset").map_index(|index| index + 10);
    assert_eq!(unknown.index(), None);

    let known = VfError::failure(2, libc::ENOENT as u32).map_index(|index| index + 10);
    assert_eq!(known.index(), Some(12));
}

#[test]
fn nfs_status_is_not_conflated_with_errno() {
    let error = VfError::from_rpc(vfsi_core::RpcError::op(3, 10044), Some(7));
    assert_eq!(error.domain(), ErrorDomain::Nfs);
    assert_eq!(error.status(), Some(StatusCode::Nfs(10044)));
    assert_eq!(error.index(), Some(7));
}

#[test]
fn status_without_a_known_index_is_not_reported_as_operation_zero() {
    // A compound-level status (for example NFS4ERR_MINOR_VERS_MISMATCH) has no
    // per-operation result, so `None` must stay unattributed rather than
    // silently becoming operation 0.
    let error = VfError::from_rpc(vfsi_core::RpcError::op(0, 10021), None);
    assert!(!error.is_transport());
    assert_eq!(error.index(), None);
    assert_eq!(error.domain(), ErrorDomain::Nfs);
    assert_eq!(error.status(), Some(StatusCode::Nfs(10021)));
    assert_eq!(error.err_no(), 10021);

    // Re-attributing turns it into a concrete operation failure.
    let attributed = error.with_index(5);
    assert_eq!(attributed.index(), Some(5));
    assert_eq!(attributed.status(), Some(StatusCode::Nfs(10021)));

    // An explicitly indexed status is still attributed directly.
    let indexed = VfError::from_rpc(vfsi_core::RpcError::op(0, 10021), Some(0));
    assert_eq!(indexed.index(), Some(0));
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
fn metadata_distinguishes_unavailable_fields_from_zero_and_false() {
    let absent = Metadata::from(VfAttrs::default());
    assert_eq!(absent.mode(), None);
    assert_eq!(absent.blocks(), None);
    assert_eq!(absent.device_id(), None);
    assert_eq!(absent.has_named_attributes(), None);

    let present = Metadata::from(VfAttrs {
        returned: AttrMask::MODE | AttrMask::BLOCKS | AttrMask::RDEV | AttrMask::NAMED_ATTR,
        mode: libc::S_IFREG,
        blocks: 0,
        rdev: 0,
        has_named_attr: false,
        ..VfAttrs::default()
    });
    assert_eq!(present.mode(), Some(libc::S_IFREG));
    assert_eq!(present.blocks(), Some(0));
    assert_eq!(present.device_id(), Some(0));
    assert_eq!(present.has_named_attributes(), Some(false));
}

#[test]
fn protocol_status_constructors_preserve_their_domains() {
    let nfs = VfError::nfs(4, 10_001);
    assert_eq!(nfs.index(), Some(4));
    assert_eq!(nfs.status(), Some(StatusCode::Nfs(10_001)));

    let smb = VfError::smb(2, 0xC000_0054);
    assert_eq!(smb.index(), Some(2));
    assert_eq!(smb.status(), Some(StatusCode::Smb(0xC000_0054)));
}
