use super::*;

#[test]
fn timeout_connection_auth_and_invalid_data_keep_typed_provenance() {
    for (source, kind) in [
        (SmbError::Timeout, vfsi_core::TransportKind::Timeout),
        (SmbError::Disconnected, vfsi_core::TransportKind::Connection),
        (
            SmbError::Auth {
                message: "denied".into(),
            },
            vfsi_core::TransportKind::Auth,
        ),
        (
            SmbError::InvalidData {
                message: "bad frame".into(),
            },
            vfsi_core::TransportKind::InvalidReply,
        ),
    ] {
        let error = smb_error(source, None);
        assert_eq!(error.transport_kind(), Some(kind));
        assert_eq!(error.index(), None);
        assert_eq!(error.status(), None);
    }
}

#[test]
fn known_server_statuses_keep_their_original_domain() {
    for (status, kind) in [
        (
            NtStatus::OBJECT_NAME_NOT_FOUND,
            std::io::ErrorKind::NotFound,
        ),
        (
            NtStatus::ACCESS_DENIED,
            std::io::ErrorKind::PermissionDenied,
        ),
        (
            NtStatus::SHARING_VIOLATION,
            std::io::ErrorKind::ResourceBusy,
        ),
        (NtStatus::NOT_IMPLEMENTED, std::io::ErrorKind::Unsupported),
        (
            NtStatus::INVALID_DEVICE_REQUEST,
            std::io::ErrorKind::Unsupported,
        ),
        (NtStatus::NOT_SUPPORTED, std::io::ErrorKind::Unsupported),
        (
            NtStatus::FILE_LOCK_CONFLICT,
            std::io::ErrorKind::ResourceBusy,
        ),
        (NtStatus::BAD_NETWORK_NAME, std::io::ErrorKind::NotFound),
        (
            NtStatus::INVALID_LOGON_HOURS,
            std::io::ErrorKind::PermissionDenied,
        ),
        (
            NtStatus::INVALID_WORKSTATION,
            std::io::ErrorKind::PermissionDenied,
        ),
        (
            NtStatus::ACCOUNT_EXPIRED,
            std::io::ErrorKind::PermissionDenied,
        ),
        (
            NtStatus::PASSWORD_MUST_CHANGE,
            std::io::ErrorKind::PermissionDenied,
        ),
    ] {
        let error = smb_error(
            SmbError::Protocol {
                status,
                command: Command::Create,
            },
            4,
        );
        assert_eq!(error.status(), Some(StatusCode::Smb(status.0)));
        assert_eq!(error.kind(), kind, "{status:?}");
        assert_eq!(error.index(), Some(4));
    }
}

#[test]
fn truncated_compounds_have_invalid_reply_provenance() {
    assert_eq!(
        compound_cardinality_error(1, 3).transport_kind(),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
}
