use std::path::Path;
use vfsi_core::internal::errors;
use vfsi_core::{native_status, transport_kind};

use vfsi_core::{AttrMask, IndexedError, RpcError, StatusCode, TransportKind, VfAttrs};

#[test]
fn rpc_conversion_preserves_messages_and_explicit_or_protocol_indices() {
    let transport = IndexedError::at(
        3,
        errors::from_rpc(RpcError::transport("connection refused")),
    );
    assert_eq!(
        transport_kind(transport.cause()),
        Some(TransportKind::Other)
    );
    assert_eq!(transport.index(), Some(3));
    assert!(transport.to_string().contains("connection refused"));
    let unknown = IndexedError::unattributed(errors::from_rpc(RpcError::transport("server gone")));
    assert_eq!(unknown.index(), None);
    assert!(!unknown.to_string().contains("input 0"));
    let explicit = IndexedError::at(1, errors::from_rpc(RpcError::op(4, 2)));
    assert_eq!(explicit.index(), Some(1));
    assert_eq!(native_status(explicit.cause()), Some(StatusCode::Nfs(2)));
    let rpc = RpcError::op(4, 17);
    let indexed = IndexedError::at(rpc.op_index, errors::from_rpc(rpc));
    assert_eq!(indexed.index(), Some(4));
    assert_eq!(native_status(indexed.cause()), Some(StatusCode::Nfs(17)));
    assert_eq!(indexed.map_index(|_| 9).index(), Some(9));
    assert_eq!(transport.map_index(|_| 5).index(), Some(5));
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
        (TransportKind::Auth, K::PermissionDenied),
        (TransportKind::Other, K::Other),
    ] {
        let error = IndexedError::at(
            3,
            errors::from_rpc(RpcError::transport_with_kind(
                transport,
                "opaque backend message",
            )),
        )
        .with_context("vread_native", "/file");
        assert_eq!(transport_kind(error.cause()), Some(transport));
        assert_eq!(error.index(), Some(3));
        assert_eq!(native_status(error.cause()), None);
        assert_eq!(error.cause().kind(), portable);
        let io: std::io::Error = error.into_cause();
        assert_eq!(io.kind(), portable);
        assert_eq!(transport_kind(&io), Some(transport));
        assert_eq!(errors::operation(&io), Some("vread_native"));
        assert_eq!(errors::path(&io), Some(Path::new("/file")));
        assert_eq!(errors::message(&io), Some("opaque backend message"));
    }
    let unknown = errors::transport(
        TransportKind::Other,
        "timeout: connection refused: invalid reply",
    );
    assert_eq!(transport_kind(&unknown), Some(TransportKind::Other));
    assert_eq!(unknown.kind(), K::Other);
    assert_eq!(
        transport_kind(&errors::from_status(StatusCode::Nfs(2))),
        None
    );
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
    let cause = errors::with_context(
        errors::transport(TransportKind::Other, "connection reset"),
        "rename",
        "/target",
    );
    let error = vfsi_core::OpError::unknown(cause, None);
    assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
    let cause = error.cause();
    assert_eq!(transport_kind(cause), Some(TransportKind::Other));
    assert_eq!(native_status(cause), None);
    assert_eq!(cause.kind(), std::io::ErrorKind::Other);
    assert!(cause.to_string().contains("rename"));
    assert!(cause.to_string().contains("/target"));
    assert_eq!(errors::operation(cause), Some("rename"));
    assert_eq!(errors::path(cause), Some(Path::new("/target")));
}

#[test]
fn portable_error_kinds_preserve_protocol_status_and_source() {
    use std::io::ErrorKind as K;
    for (error, kind, status) in [
        (
            errors::from_status(StatusCode::Nfs(28)),
            K::StorageFull,
            Some(StatusCode::Nfs(28)),
        ),
        (
            errors::from_status(StatusCode::Nfs(10004)),
            K::Unsupported,
            Some(StatusCode::Nfs(10004)),
        ),
        (
            errors::from_status(StatusCode::Smb(0xc000_0022)),
            K::PermissionDenied,
            Some(StatusCode::Smb(0xc000_0022)),
        ),
        (
            errors::from_status(StatusCode::Smb(0xc000_0034)),
            K::NotFound,
            Some(StatusCode::Smb(0xc000_0034)),
        ),
        // Same numeric value in SMB is not an errno.
        (
            errors::from_status(StatusCode::Smb(2)),
            K::Other,
            Some(StatusCode::Smb(2)),
        ),
        (
            std::io::Error::from_raw_os_error(libc::ENOENT),
            K::NotFound,
            Some(StatusCode::Errno(libc::ENOENT as u32)),
        ),
        (std::io::Error::from(K::Unsupported), K::Unsupported, None),
    ] {
        assert_eq!(error.kind(), kind);
        let address = error
            .get_ref()
            .map(|source| source as *const (dyn std::error::Error + Send + Sync));
        let converted: std::io::Error = IndexedError::at(2, error).into_cause();
        assert_eq!(converted.kind(), kind);
        assert_eq!(native_status(&converted), status);
        assert_eq!(
            converted
                .get_ref()
                .map(|source| source as *const (dyn std::error::Error + Send + Sync)),
            address
        );
    }
    let display = errors::with_context(
        errors::from_status(StatusCode::Nfs(28)),
        "vwrite_native",
        "/file",
    )
    .to_string();
    for context in ["vwrite_native", "/file", "Nfs", "28"] {
        assert!(display.contains(context));
    }
}

#[test]
fn protocol_cross_device_errors_keep_their_category_and_status_through_standard_io() {
    for cause in [
        errors::from_status(StatusCode::Nfs(18)),
        errors::from_rpc(RpcError::op(7, 18)),
        errors::from_status(StatusCode::Smb(0xc000_00d4)),
    ] {
        let error = IndexedError::at(2, cause).with_context("vrename", "/source");
        let status = native_status(error.cause());
        assert_eq!(error.cause().kind(), std::io::ErrorKind::CrossesDevices);
        assert_eq!(transport_kind(error.cause()), None);
        assert_eq!(error.index(), Some(2));
        let io: std::io::Error = error.into_cause();
        assert_eq!(io.kind(), std::io::ErrorKind::CrossesDevices);
        assert_eq!(native_status(&io), status);
        assert_eq!(errors::path(&io), Some(Path::new("/source")));
    }
    assert_eq!(
        errors::from_status(StatusCode::Smb(18)).kind(),
        std::io::ErrorKind::Other
    );
    assert_eq!(
        errors::from_status(StatusCode::Nfs(0xc000_00d4)).kind(),
        std::io::ErrorKind::Other
    );
}

#[test]
fn rpc_transport_placeholder_is_not_exposed_as_request_zero() {
    let error =
        IndexedError::unattributed(errors::from_rpc(RpcError::transport("connection reset")));
    assert_eq!(transport_kind(error.cause()), Some(TransportKind::Other));
    assert_eq!(error.index(), None);
    assert!(!error.to_string().contains("input 0"));
}

#[test]
fn unsupported_referrals_preserve_protocol_status_indices_and_context() {
    let moved = 10019;
    let path_not_covered = 0xc000_0257;
    let rpc = RpcError::op(4, moved);
    assert!(
        rpc.to_string()
            .contains("referrals and filesystem migration are unsupported")
    );
    for (cause, index, status, detail) in [
        (
            errors::from_status(StatusCode::Nfs(moved)),
            Some(3),
            StatusCode::Nfs(moved),
            "NFSv4",
        ),
        (
            errors::from_rpc(rpc.clone()),
            Some(3),
            StatusCode::Nfs(moved),
            "NFSv4",
        ),
        (errors::from_rpc(rpc), None, StatusCode::Nfs(moved), "NFSv4"),
        (
            errors::from_status(StatusCode::Smb(path_not_covered)),
            Some(3),
            StatusCode::Smb(path_not_covered),
            "SMB DFS",
        ),
    ] {
        let error = match index {
            Some(index) => IndexedError::at(index, cause),
            None => IndexedError::unattributed(cause),
        };
        let error = error
            .map_index(|i| i + 2)
            .with_context("vremove", "/projects/file");
        assert_eq!(error.index(), index.map(|i| i + 2));
        assert_eq!(native_status(error.cause()), Some(status));
        assert_eq!(error.cause().kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(transport_kind(error.cause()), None);
        let message = error.to_string();
        for text in [
            detail,
            "unsupported",
            "connect directly",
            "vremove",
            "/projects/file",
        ] {
            assert!(message.contains(text), "missing {text:?}: {message}");
        }
        let io: std::io::Error = error.into_cause();
        assert_eq!(io.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(native_status(&io), Some(status));
        assert_eq!(errors::path(&io), Some(Path::new("/projects/file")));
    }
    assert!(
        !std::io::Error::from_raw_os_error(moved as i32)
            .to_string()
            .contains("referrals")
    );
    assert!(
        !errors::from_status(StatusCode::Smb(moved))
            .to_string()
            .contains("referrals")
    );
}

#[test]
fn index_mapping_preserves_unknown_transport_location() {
    let unknown =
        IndexedError::unattributed(errors::transport(TransportKind::Other, "connection reset"))
            .map_index(|index| index + 10);
    assert_eq!(unknown.index(), None);
    let known = IndexedError::at(2, std::io::Error::from_raw_os_error(libc::ENOENT))
        .map_index(|index| index + 10);
    assert_eq!(known.index(), Some(12));
    assert_eq!(known.cause().raw_os_error(), Some(libc::ENOENT));
}

#[test]
fn nfs_status_is_not_conflated_with_errno() {
    let error = IndexedError::at(7, errors::from_rpc(RpcError::op(3, 10044)));
    assert_eq!(native_status(error.cause()), Some(StatusCode::Nfs(10044)));
    assert_eq!(error.cause().raw_os_error(), None);
    assert_eq!(error.index(), Some(7));
}

#[test]
fn status_without_a_known_index_is_not_reported_as_operation_zero() {
    // A whole-compound rejection has no logical input, including RPC offset 0.
    let error = IndexedError::unattributed(errors::from_rpc(RpcError::op(0, 10021)));
    assert_eq!(transport_kind(error.cause()), None);
    assert_eq!(error.index(), None);
    assert_eq!(native_status(error.cause()), Some(StatusCode::Nfs(10021)));
    let attributed = IndexedError::at(5, error.into_cause());
    assert_eq!(attributed.index(), Some(5));
    assert_eq!(
        native_status(attributed.cause()),
        Some(StatusCode::Nfs(10021))
    );
    let indexed = IndexedError::at(0, errors::from_rpc(RpcError::op(0, 10021)));
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
fn metadata_exposes_present_and_absent_masks() {
    let requested = AttrMask::MODE | AttrMask::UID | AttrMask::MTIME;
    let metadata = vfsi_core::metadata_from_attrs(VfAttrs {
        returned: AttrMask::MODE | AttrMask::MTIME,
        mode: libc::S_IFREG | 0o644,
        mtime_sec: 1,
        ..VfAttrs::default()
    });

    assert_eq!(metadata.present_mask(), AttrMask::MODE | AttrMask::MTIME);
    assert_eq!(metadata.absent_mask(requested), AttrMask::UID);
    assert!(!metadata.contains(requested));
    assert!(metadata.contains(AttrMask::MODE | AttrMask::MTIME));
    assert!(metadata.contains(AttrMask::empty()));
    assert!(metadata.absent_mask(AttrMask::empty()).is_empty());

    let absent = vfsi_core::metadata_from_attrs(VfAttrs::default());
    assert!(absent.present_mask().is_empty());
    assert_eq!(absent.absent_mask(requested), requested);
    assert!(!absent.contains(requested));
    assert!(absent.contains(AttrMask::empty()));
}

#[test]
fn metadata_absent_mask_preserves_unknown_requested_bits() {
    let unknown = AttrMask::from_bits_retain(1 << 31);
    let requested = AttrMask::MODE | unknown;
    let metadata = vfsi_core::metadata_from_attrs(VfAttrs {
        returned: AttrMask::MODE,
        ..VfAttrs::default()
    });
    assert_eq!(metadata.absent_mask(requested), unknown);
    assert!(!metadata.contains(requested));
    assert!(!metadata.contains(unknown));

    let returned = vfsi_core::metadata_from_attrs(VfAttrs {
        returned: requested,
        ..VfAttrs::default()
    });
    assert_eq!(returned.present_mask(), requested);
    assert!(returned.absent_mask(requested).is_empty());
    assert!(returned.contains(requested));
}

#[test]
fn protocol_status_constructors_preserve_their_domains() {
    let nfs = IndexedError::at(4, errors::from_status(StatusCode::Nfs(10_001)));
    assert_eq!(nfs.index(), Some(4));
    assert_eq!(native_status(nfs.cause()), Some(StatusCode::Nfs(10_001)));
    let smb = IndexedError::at(2, errors::from_status(StatusCode::Smb(0xC000_0054)));
    assert_eq!(smb.index(), Some(2));
    assert_eq!(
        native_status(smb.cause()),
        Some(StatusCode::Smb(0xC000_0054))
    );
}

#[test]
fn opaque_errors_keep_typed_status_context_and_optional_indices_consistent() {
    for status in [
        StatusCode::Errno(0),
        StatusCode::Errno(2),
        StatusCode::Nfs(0),
        StatusCode::Nfs(2),
        StatusCode::Nfs(u32::MAX),
        StatusCode::Smb(0),
        StatusCode::Smb(2),
        StatusCode::Smb(u32::MAX),
    ] {
        for index in [None, Some(0), Some(7)] {
            let cause = errors::with_context(errors::from_status(status), "read", "/file");
            assert_eq!(native_status(&cause), Some(status));
            assert_eq!(transport_kind(&cause), None);
            assert_eq!(errors::message(&cause), None);
            let error = match index {
                Some(index) => IndexedError::at(index, cause),
                None => IndexedError::unattributed(cause),
            };
            assert_eq!(error.index(), index);
            let mapped = error.map_index(|index| index + 10);
            assert_eq!(mapped.index(), index.map(|index| index + 10));
            assert_eq!(native_status(mapped.cause()), Some(status));
            assert_eq!(errors::operation(mapped.cause()), Some("read"));
            assert_eq!(errors::path(mapped.cause()), Some(Path::new("/file")));
            let attributed = IndexedError::at(42, mapped.into_cause());
            assert_eq!(attributed.index(), Some(42));
            assert_eq!(native_status(attributed.cause()), Some(status));
        }
    }
    let mut called = false;
    let unknown =
        IndexedError::unattributed(errors::from_status(StatusCode::Nfs(2))).map_index(|index| {
            called = true;
            index
        });
    assert!(!called);
    assert_eq!(unknown.index(), None);
    assert_eq!(
        std::io::Error::from_raw_os_error(2).raw_os_error(),
        errors::from_status(StatusCode::Errno(2)).raw_os_error()
    );
    for status in [StatusCode::Nfs(2), StatusCode::Smb(2)] {
        assert_eq!(native_status(&errors::from_status(status)), Some(status));
    }
}

#[test]
fn opaque_error_display_and_transport_diagnostics_preserve_context() {
    for index in [None, Some(3)] {
        let cause = errors::with_context(
            errors::transport(TransportKind::Timeout, "wire diagnostic"),
            "read",
            "/file",
        );
        assert_eq!(errors::message(&cause), Some("wire diagnostic"));
        assert_eq!(transport_kind(&cause), Some(TransportKind::Timeout));
        assert_eq!(native_status(&cause), None);
        let error = match index {
            Some(index) => IndexedError::at(index, cause),
            None => IndexedError::unattributed(cause),
        };
        for text in ["read", "/file", "wire diagnostic"] {
            assert!(error.to_string().contains(text));
        }
        assert_eq!(error.to_string().contains("input 3"), index.is_some());
        assert!(!error.cause().to_string().contains("input 3"));
        let mapped = error.map_index(|index| index + 2);
        assert_eq!(mapped.index(), index.map(|index| index + 2));
        assert_eq!(errors::message(mapped.cause()), Some("wire diagnostic"));
        assert_eq!(transport_kind(mapped.cause()), Some(TransportKind::Timeout));
    }
    let bare = errors::transport(TransportKind::Other, "wire");
    assert!(bare.to_string().contains("wire"));
    let known = IndexedError::at(0, bare);
    assert!(known.to_string().contains("input 0"));
    assert!(!known.cause().to_string().contains("input 0"));
    let description = std::io::Error::from_raw_os_error(2).to_string();
    for index in [Some(3), None] {
        let cause =
            errors::with_context(errors::from_status(StatusCode::Errno(2)), "read", "/file");
        let error = match index {
            Some(index) => IndexedError::at(index, cause),
            None => IndexedError::unattributed(cause),
        };
        for text in ["read", "/file", description.as_str()] {
            assert!(error.to_string().contains(text));
        }
        assert_eq!(native_status(error.cause()), Some(StatusCode::Errno(2)));
    }
}

#[test]
fn owned_native_reads_reject_foreign_results_and_misassociated_progress() {
    use vfsi_core::{
        FailureState, FileOperand, IoProgress, OpError, ReadIntoResult, ReadOp, VfOffset,
    };
    let reads = [
        ReadOp::from_fd(10, VfOffset::At(1), 2),
        ReadOp::from_fd(11, VfOffset::At(2), 2),
        ReadOp::from_fd(12, VfOffset::At(3), 2),
        ReadOp::from_fd(13, VfOffset::At(4), 2),
        ReadOp::from_fd(14, VfOffset::Cur, 2),
        ReadOp::from_fd(15, VfOffset::End, 2),
    ];
    let results = vfsi_core::internal::read_with_owned_buffers(&reads, |_, buffers| {
        buffers[3][0] = b'x';
        buffers[5][0] = b'y';
        let ok = |fd, offset| {
            Ok(ReadIntoResult {
                file: FileOperand::from_fd(fd),
                offset,
                bytes_read: 1,
                eof: false,
            })
        };
        let failed = |offset| {
            Err(OpError::failed(
                std::io::Error::from_raw_os_error(libc::EIO),
                Some(IoProgress::Read(vfsi_core::api::ReadOutput::buffered(
                    offset, 1, false,
                ))),
            ))
        };
        Ok(vec![
            ok(99, 1),
            ok(11, 10),
            failed(19),
            ok(13, 4),
            failed(u64::MAX),
            failed(8),
        ])
    })
    .unwrap();
    for index in [0, 1, 2, 4] {
        let error = results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(
            transport_kind(error.cause()),
            Some(TransportKind::InvalidReply)
        );
        assert!(error.progress().is_none());
    }
    assert_eq!(results[3].as_ref().unwrap().data, b"x");
    let Some(IoProgress::Read(progress)) = results[5].as_ref().unwrap_err().progress() else {
        panic!("missing valid relative read progress");
    };
    assert_eq!(progress.offset(), 8);
    assert_eq!(progress.data(), Some(b"y".as_slice()));
}
