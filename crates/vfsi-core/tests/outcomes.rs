use std::path::Path;

use vfsi_core::{AttrMask, ErrorDomain, RpcError, StatusCode, TransportKind, VfAttrs, VfError};

#[test]
fn rpc_conversion_preserves_messages_and_explicit_or_protocol_indices() {
    let transport = vfsi_core::error_from_rpc(RpcError::transport("connection refused"), 3);
    assert!(transport.is_transport());
    assert_eq!(transport.err_no(), vfsi_core::VF_ERR_RPC);
    assert_eq!(transport.index(), Some(3));
    assert!(transport.to_string().contains("connection refused"));
    let unknown = vfsi_core::error_from_rpc(RpcError::transport("server gone"), None);
    assert_eq!(unknown.index(), None);
    assert!(!unknown.to_string().contains("op "));
    let explicit = vfsi_core::error_from_rpc(RpcError::op(4, 2), 1);
    assert_eq!(explicit.index(), Some(1));
    assert_eq!(explicit.status(), Some(StatusCode::Nfs(2)));
    let indexed = vfsi_core::error_from_rpc_indexed(RpcError::op(4, 17));
    assert_eq!(indexed.index(), Some(4));
    assert_eq!(indexed.status(), Some(StatusCode::Nfs(17)));
    assert_eq!(indexed.with_index(9).index(), Some(9));
    assert_eq!(transport.with_index(5).index(), Some(5));
}

#[test]
fn descriptor_allocator_wraps_without_replacing_a_live_descriptor() {
    let mut next = i32::MAX;
    let mut files = std::collections::HashMap::from([(1, "live")]);
    let fd = vfsi_core::insert_fd(&mut next, &mut files, "new").unwrap();
    assert_eq!(fd, 2);
    assert_eq!(files.get(&1), Some(&"live"));
    assert_eq!(files.get(&2), Some(&"new"));
}

#[test]
fn transport_categories_survive_attribution_context_and_io_conversion() {
    use std::io::ErrorKind as K;
    for (transport, portable) in [
        (TransportKind::Timeout, K::TimedOut),
        (TransportKind::Connection, K::Other),
        (TransportKind::InvalidReply, K::InvalidData),
        (TransportKind::Authentication, K::PermissionDenied),
        (TransportKind::Other, K::Other),
    ] {
        let error = vfsi_core::error_from_rpc(
            RpcError::transport_with_kind(transport, "opaque backend message"),
            None,
        )
        .with_index(3)
        .with_context("vread_native", "/file");
        assert_eq!(error.transport_kind(), Some(transport));
        assert_eq!(error.index(), Some(3));
        assert_eq!(error.status(), None);
        assert_eq!(error.kind(), portable);
        let io: std::io::Error = error.clone().into();
        assert_eq!(io.kind(), portable);
        assert_eq!(
            io.get_ref().unwrap().downcast_ref::<VfError>(),
            Some(&error)
        );
    }
    let unknown = VfError::transport(None, "timeout: connection refused: invalid reply");
    assert_eq!(unknown.transport_kind(), Some(TransportKind::Other));
    assert_eq!(unknown.kind(), K::Other);
    assert_eq!(VfError::nfs(0, 2).transport_kind(), None);
    assert_eq!(
        RpcError::from_io(std::io::Error::from(K::TimedOut)).transport_kind,
        Some(TransportKind::Timeout)
    );
    assert_eq!(
        RpcError::from_io(std::io::Error::from(K::ConnectionRefused)).transport_kind,
        Some(TransportKind::Connection)
    );
}

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
        .with_context("vwrite_native", "/file")
        .to_string();
    for context in ["vwrite_native", "/file", "Nfs", "28"] {
        assert!(display.contains(context));
    }
}

#[test]
fn rpc_transport_placeholder_is_not_exposed_as_request_zero() {
    let error =
        vfsi_core::error_from_rpc_indexed(vfsi_core::RpcError::transport("connection reset"));
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
    let error = vfsi_core::error_from_rpc(vfsi_core::RpcError::op(3, 10044), Some(7));
    assert_eq!(error.domain(), ErrorDomain::Nfs);
    assert_eq!(error.status(), Some(StatusCode::Nfs(10044)));
    assert_eq!(error.index(), Some(7));
}

#[test]
fn status_without_a_known_index_is_not_reported_as_operation_zero() {
    // A compound-level status (for example NFS4ERR_MINOR_VERS_MISMATCH) has no
    // per-operation result, so `None` must stay unattributed rather than
    // silently becoming operation 0.
    let error = vfsi_core::error_from_rpc(vfsi_core::RpcError::op(0, 10021), None);
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
    let indexed = vfsi_core::error_from_rpc(vfsi_core::RpcError::op(0, 10021), Some(0));
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
    let metadata = vfsi_core::metadata_from_attrs(attributes);
    assert_eq!(
        metadata.accessed(),
        Some(std::time::UNIX_EPOCH - std::time::Duration::from_millis(500))
    );
}

#[test]
fn metadata_distinguishes_unavailable_fields_from_zero_and_false() {
    let absent = vfsi_core::metadata_from_attrs(VfAttrs::default());
    assert_eq!(absent.mode(), None);
    assert_eq!(absent.blocks(), None);
    assert_eq!(absent.device_id(), None);
    assert_eq!(absent.has_named_attributes(), None);

    let present = vfsi_core::metadata_from_attrs(VfAttrs {
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
fn metadata_exposes_returned_and_missing_attribute_masks() {
    let requested = AttrMask::MODE | AttrMask::UID | AttrMask::MTIME;
    let metadata = vfsi_core::metadata_from_attrs(VfAttrs {
        returned: AttrMask::MODE | AttrMask::MTIME,
        mode: libc::S_IFREG | 0o644,
        mtime_sec: 1,
        ..VfAttrs::default()
    });

    assert_eq!(
        metadata.returned_attributes(),
        AttrMask::MODE | AttrMask::MTIME
    );
    assert_eq!(metadata.missing_attributes(requested), AttrMask::UID);
    assert!(!metadata.has_attributes(requested));
    assert!(metadata.has_attributes(AttrMask::MODE | AttrMask::MTIME));
}

#[test]
fn metadata_missing_attributes_preserves_unknown_requested_bits() {
    let unknown = AttrMask::from_bits_retain(1 << 31);
    let requested = AttrMask::MODE | unknown;
    let metadata = vfsi_core::metadata_from_attrs(VfAttrs {
        returned: AttrMask::MODE,
        ..VfAttrs::default()
    });
    assert_eq!(metadata.missing_attributes(requested), unknown);
    assert!(!metadata.has_attributes(requested));
    assert!(!metadata.has_attributes(unknown));

    let returned = vfsi_core::metadata_from_attrs(VfAttrs {
        returned: requested,
        ..VfAttrs::default()
    });
    assert!(returned.missing_attributes(requested).is_empty());
    assert!(returned.has_attributes(requested));
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
