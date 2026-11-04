use proptest::prelude::*;

use super::*;

#[test]
fn bounded_runtime_drives_concurrent_callers_and_background_tasks() {
    let runtime = smb_runtime().unwrap();
    assert_eq!(runtime.metrics().num_workers(), 1);
    let (send, receive) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        send.send(42).unwrap();
    });
    // Background protocol work must run even outside block_on.
    assert_eq!(receive.recv_timeout(Duration::from_secs(5)).unwrap(), 42);
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
            });
        }
    });
}

#[test]
fn create_handle_survives_later_compound_waiter_errors() {
    enum Response {
        Create(FileId),
        SetInfo,
        Close(bool),
    }

    let parse_create = |response: &Response| match response {
        Response::Create(file_id) => Ok(*file_id),
        _ => panic!("expected CREATE response"),
    };
    let map_error = |error| VfError::transport(None, error);

    let set_info_failed = decode_open_compound_responses(
        vec![
            Ok(Response::Create(FileId::SENTINEL)),
            Err("SET_INFO waiter failed"),
            Ok(Response::Close(true)),
        ],
        3,
        parse_create,
        |response| matches!(response, Response::Close(true)),
        map_error,
    )
    .unwrap();
    assert_eq!(set_info_failed.file_id, FileId::SENTINEL);
    assert!(matches!(
        set_info_failed.operation_responses.as_slice(),
        [Err("SET_INFO waiter failed")]
    ));
    assert!(set_info_failed.close_succeeded);
    assert!(set_info_failed.cardinality_error.is_none());

    let close_failed = decode_open_compound_responses(
        vec![
            Ok(Response::Create(FileId::SENTINEL)),
            Ok(Response::SetInfo),
            Err("CLOSE waiter failed"),
        ],
        3,
        parse_create,
        |response| matches!(response, Response::Close(true)),
        map_error,
    )
    .unwrap();
    assert_eq!(close_failed.file_id, FileId::SENTINEL);
    assert!(matches!(
        close_failed.operation_responses.as_slice(),
        [Ok(Response::SetInfo)]
    ));
    assert!(!close_failed.close_succeeded);
    assert!(close_failed.cardinality_error.is_none());

    let mut deferred_close = None;
    let close_result = finish_compound_close(
        &mut deferred_close,
        close_failed.file_id,
        close_failed.close_succeeded,
        Err(VfError::transport(None, "injected fallback CLOSE failure")),
    );
    assert!(close_result.unwrap_err().is_transport());
    assert_eq!(deferred_close, Some(FileId::SENTINEL));
}

#[test]
fn create_handle_survives_truncated_compound_response() {
    enum Response {
        Create(FileId),
    }

    let truncated = decode_open_compound_responses(
        vec![
            Ok(Response::Create(FileId::SENTINEL)),
            Err("READ waiter failed"),
        ],
        3,
        |Response::Create(file_id)| Ok(*file_id),
        |_| false,
        |error| VfError::transport(None, error),
    )
    .unwrap();
    assert_eq!(truncated.file_id, FileId::SENTINEL);
    assert!(matches!(
        truncated.operation_responses.as_slice(),
        [Err("READ waiter failed")]
    ));
    assert!(!truncated.close_succeeded);
    assert!(truncated.cardinality_error.is_some());

    let mut deferred_close = None;
    let close_result = finish_compound_close(
        &mut deferred_close,
        truncated.file_id,
        truncated.close_succeeded,
        Err(VfError::transport(None, "injected fallback CLOSE failure")),
    );
    assert!(close_result.unwrap_err().is_transport());
    assert_eq!(deferred_close, Some(FileId::SENTINEL));
}

#[test]
fn missing_or_failed_compound_close_response_requires_fallback_close() {
    let waiter_error: Result<(), ()> = Err(());
    assert!(!response_succeeded(None::<&Result<(), ()>>, |_| true));
    assert!(!response_succeeded(Some(&waiter_error), |_| true));

    let status_failure: Result<(), ()> = Ok(());
    assert!(!response_succeeded(Some(&status_failure), |_| false));
    assert!(response_succeeded(Some(&status_failure), |_| true));
}

#[test]
fn delete_entry_request_opens_the_reparse_point_without_type_constraint() {
    let create = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(FileAccessMask::FILE_READ_DATA),
        file_attributes: 0,
        share_access: ShareAccess(ShareAccess::FILE_SHARE_READ),
        create_disposition: CreateDisposition::FileOpen,
        create_options: FILE_NON_DIRECTORY_FILE,
        name: "entry".to_owned(),
        create_contexts: Vec::new(),
    };
    let request = delete_entry_create_request(create);
    assert_eq!(
        request.desired_access,
        FileAccessMask::new(
            FileAccessMask::DELETE
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
        )
    );
    assert_eq!(request.create_options, FILE_OPEN_REPARSE_POINT);
}

fn offset_boundary() -> impl Strategy<Value = u64> {
    prop_oneof![
        0u64..=2048,
        (i64::MAX as u64 - 1024)..=(i64::MAX as u64 + 1024),
        (u64::MAX - 2048)..=u64::MAX,
    ]
}

proptest! {
    #[test]
    fn checked_offsets_match_u64_arithmetic_at_signed_and_unsigned_boundaries(
        base in offset_boundary(),
        delta in 0u64..=4096,
        index in 0usize..32,
    ) {
        match (base.checked_add(delta), checked_offset(base, delta, index)) {
            (Some(expected), Ok(actual)) => prop_assert_eq!(actual, expected),
            (None, Err(error)) => {
                prop_assert_eq!(error.index(), Some(index));
                prop_assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
            }
            (expected, actual) => prop_assert!(false, "expected {expected:?}, got {actual:?}"),
        }
    }
}

#[test]
fn unsupported_create_modes_are_rejected_before_io() {
    let flags = [
        libc::O_WRONLY | libc::O_CREAT,
        libc::O_WRONLY | libc::O_CREAT,
    ];
    let modes = [0o666, 0o600];
    let error = validate_open_modes(&flags, &modes).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.raw_code(), vfsi_core::VF_ERR_UNSUPPORTED);
    assert!(validate_create_mode(libc::O_WRONLY, 0o600, 0).is_ok());

    // Conventional defaults intentionally delegate permissions to the
    // server/share security descriptor, as documented at the crate root.
    assert!(validate_create_mode(libc::O_WRONLY | libc::O_CREAT, 0o666, 0).is_ok());
    assert!(
        validate_mkdir_modes(&[VfAttrs {
            masks: AttrMask::MODE,
            mode: 0o777,
            ..VfAttrs::default()
        }])
        .is_ok()
    );

    let dirs = [
        VfAttrs {
            masks: AttrMask::MODE,
            mode: 0o777,
            ..VfAttrs::default()
        },
        VfAttrs {
            masks: AttrMask::MODE,
            mode: 0o700,
            ..VfAttrs::default()
        },
    ];
    let error = validate_mkdir_modes(&dirs).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.raw_code(), vfsi_core::VF_ERR_UNSUPPORTED);
}

#[test]
fn failed_temporary_close_is_retained_for_deferred_cleanup() {
    let file_id = FileId::SENTINEL;
    let mut deferred = Vec::new();
    let result = retain_failed_close(
        &mut deferred,
        file_id,
        Err(VfError::transport(None, "injected close failure")),
    );
    assert!(result.unwrap_err().is_transport());
    assert_eq!(deferred, [file_id]);

    retain_failed_close(&mut deferred, file_id, Ok(())).unwrap();
    assert_eq!(deferred, [file_id]);
}

#[test]
fn vectorized_compound_close_failure_returns_handle_for_deferred_cleanup() {
    let file_id = FileId::SENTINEL;
    let mut deferred_close = None;
    let result = finish_compound_close(
        &mut deferred_close,
        file_id,
        false,
        Err(VfError::transport(None, "injected fallback CLOSE failure")),
    );
    assert!(result.unwrap_err().is_transport());
    assert_eq!(deferred_close, Some(file_id));

    let outcome: CompoundOutcome<Vec<u8>> = CompoundOutcome {
        result: Err(VfError::transport(None, "injected vector read failure")),
        deferred_close,
    };
    let mut owner_queue = Vec::new();
    queue_deferred_closes(&[outcome], &mut owner_queue);
    assert_eq!(owner_queue, [file_id]);

    let mut no_failed_close = None;
    finish_compound_close(&mut no_failed_close, file_id, true, Ok(())).unwrap();
    assert_eq!(no_failed_close, None);
}

#[test]
fn timestamp_updates_reject_protocol_controls_and_negative_filetimes() {
    for ticks in [0, u64::MAX, u64::MAX - 1, i64::MAX as u64 + 1] {
        let (seconds, nanos) = filetime_parts(FileTime(ticks));
        let error = SmbVecFs::filetime_value(seconds, nanos, 3).unwrap_err();
        assert_eq!(error.raw_code(), ERR_INVAL);
        assert_eq!(error.index(), Some(3));
    }
    for ticks in [1, 116_444_736_000_000_000, i64::MAX as u64] {
        let (seconds, nanos) = filetime_parts(FileTime(ticks));
        assert_eq!(SmbVecFs::filetime_value(seconds, nanos, 3).unwrap(), ticks);
    }
}

#[test]
fn pre_epoch_fractional_filetimes_round_trip() {
    let ticks = SmbVecFs::filetime_value(-1, 500_000_000, 0).unwrap();
    assert_eq!(filetime_parts(FileTime(ticks)), (-1, 500_000_000));
}

#[test]
fn dfs_wire_path_preserves_ipv6_server_name() {
    assert_eq!(
        format_dfs_wire_path("::1", "share", "dir\\file"),
        "::1\\share\\dir\\file"
    );
    assert_eq!(format_dfs_wire_path("server", "share", ""), "server\\share");
}

#[test]
fn server_address_defaults_to_port_445() {
    assert_eq!(normalize_server("samba.example"), "samba.example:445");
    assert_eq!(normalize_server("samba.example:1445"), "samba.example:1445");
    assert_eq!(normalize_server("[::1]"), "[::1]:445");
    assert_eq!(normalize_server("[::1]:445"), "[::1]:445");
}

#[test]
fn connection_deadlines_are_configurable_and_nonzero() {
    let defaults = SmbConnectOptions::default();
    assert_eq!(defaults.connect_timeout, Duration::from_secs(10));
    assert_eq!(defaults.request_timeout, Duration::from_secs(30));

    for options in [
        SmbConnectOptions {
            connect_timeout: Duration::ZERO,
            ..defaults
        },
        SmbConnectOptions {
            request_timeout: Duration::ZERO,
            ..defaults
        },
    ] {
        let error = SmbVecFs::connect_with_options("unused.invalid", "share", "", "", "", options)
            .err()
            .expect("zero timeout must fail before network I/O");
        assert_eq!(error.raw_code(), ERR_INVAL);
    }
}

#[test]
fn request_deadline_applies_to_the_initial_negotiate() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let (stop, stopped) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((_stream, _)) => {
                    // Accept TCP but never reply to NEGOTIATE.
                    let _ = stopped.recv_timeout(Duration::from_secs(3));
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept silent SMB peer: {error}"),
            }
        }
    });
    let start = std::time::Instant::now();
    let result = SmbVecFs::connect_with_options(
        &address,
        "share",
        "",
        "",
        "",
        SmbConnectOptions {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_millis(50),
            ..SmbConnectOptions::default()
        },
    );
    let elapsed = start.elapsed();
    let _ = stop.send(());
    peer.join().unwrap();
    let error = result
        .err()
        .expect("a silent server cannot negotiate successfully");
    assert!(error.is_transport());
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(
        elapsed < Duration::from_secs(2),
        "NEGOTIATE ignored request timeout: {elapsed:?}"
    );
}

#[test]
fn oversized_read_responses_are_rejected_with_invalid_reply_classification() {
    for (data, requested, index) in [(vec![1, 2, 3], 2, 0), (vec![4, 5, 6, 7], 3, 2)] {
        let mut body = smb2::pack::WriteCursor::new();
        smb2::pack::Pack::pack(
            &ReadResponse {
                data_offset: 0x50,
                data_remaining: 0,
                flags: 0,
                data,
            },
            &mut body,
        );
        let response = ReadResponse::unpack(&mut ReadCursor::new(body.as_bytes())).unwrap();
        let error = checked_read_data(response.data, requested, index).unwrap_err();
        assert_eq!(error.index(), Some(index));
        assert_eq!(
            error.transport_kind(),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
        assert!(error.to_string().contains("READ bytes"));
    }

    assert_eq!(checked_read_data(vec![1, 2], 2, 0).unwrap(), [1, 2]);
    assert!(checked_read_data(Vec::new(), 0, 0).unwrap().is_empty());
}

#[test]
fn credit_charge_rounds_up_and_never_returns_zero() {
    assert_eq!(credit_charge(0), 1);
    assert_eq!(credit_charge(1), 1);
    assert_eq!(credit_charge(65_536), 1);
    assert_eq!(credit_charge(65_537), 2);
}

#[test]
fn checked_offsets_preserve_request_index_on_overflow() {
    let error = checked_offset(u64::MAX, 1, 7).unwrap_err();
    assert_eq!(error.index(), Some(7));
    assert_eq!(error.raw_code(), libc::EOVERFLOW as u32);
}

#[test]
fn concurrent_writes_require_distinct_server_file_identities() {
    assert!(identities_are_independent(&[10, 20, 30]));
    assert!(!identities_are_independent(&[10, 20, 10]));
}

#[test]
fn smb_path_dependencies_include_case_aliases_and_serialize_unicode_batches() {
    assert!(!paths_are_independent(&[
        PathBuf::from("/a"),
        PathBuf::from("/A")
    ]));
    assert!(!paths_are_independent(&[
        PathBuf::from("/Dir/file"),
        PathBuf::from("/dir"),
    ]));
    assert!(!paths_are_independent(&[
        PathBuf::from("/\u{e9}"),
        PathBuf::from("/other"),
    ]));
    assert!(paths_are_independent(&[
        PathBuf::from("/a"),
        PathBuf::from("/b")
    ]));
    assert!(paths_are_independent(&[
        PathBuf::from("/dir/a"),
        PathBuf::from("/dir/b"),
    ]));
}

#[test]
fn handle_metadata_checks_lengths_and_decodes_only_reported_fields() {
    let mut basic = [0; 40];
    let mut standard = [0; 24];
    for (offset, ticks) in [(0, 1u64), (8, 2), (16, 3), (24, 4)] {
        basic[offset..offset + 8].copy_from_slice(&ticks.to_le_bytes());
    }
    standard[8..16].copy_from_slice(&123u64.to_le_bytes());
    basic[32..36].copy_from_slice(&0x21u32.to_le_bytes());
    for length in 0..basic.len() {
        assert!(
            handle_info(&basic[..length], &standard)
                .unwrap_err()
                .is_transport()
        );
    }
    for length in 0..standard.len() {
        assert!(
            handle_info(&basic, &standard[..length])
                .unwrap_err()
                .is_transport()
        );
    }
    let info = handle_info(&basic, &standard).unwrap();
    assert_eq!(
        (info.created.0, info.accessed.0, info.modified.0),
        (1, 2, 3)
    );
    assert_eq!(info.size, 123);
    assert_eq!(info.changed.0, 4);
    assert_eq!(info.attributes.0, 0x21);
    assert!(!info.is_directory);
    basic[32] = 0x10;
    assert!(handle_info(&basic, &standard).unwrap().is_directory);
    basic[32] = 0;
    standard[21] = 1;
    assert!(handle_info(&basic, &standard).unwrap().is_directory);
}

#[test]
fn smb_transport_failures_preserve_the_message() {
    let error = smb_error(SmbError::Disconnected, 3);
    assert_eq!(error.index(), Some(3));
    assert_eq!(error.raw_code(), crate::vecfs::VF_ERR_RPC);
    assert!(error.to_string().contains("Disconnected"));
    let error = smb_error(
        SmbError::SendFailed {
            command: Command::Write,
            source: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
        },
        7,
    );
    assert_eq!(error.index(), Some(7));
    assert!(error.is_transport());
    assert!(error.to_string().contains("Write"));
}

#[test]
fn dfs_shares_are_rejected_but_ordinary_shares_remain_usable() {
    check_share_routing(false, "ordinary").unwrap();
    let error = check_share_routing(true, "namespace").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(!error.is_transport());
    assert_eq!(error.path(), Some(Path::new("namespace")));
    assert!(
        error
            .to_string()
            .contains("SMB DFS referrals are unsupported")
    );
}

#[test]
fn referral_errors_are_unsupported_without_losing_the_request_index() {
    for error in [
        SmbError::Protocol {
            status: NtStatus::PATH_NOT_COVERED,
            command: Command::Create,
        },
        SmbError::DfsReferralRequired {
            path: "projects/file".into(),
        },
        SmbError::DfsTargetNotAllowed {
            path: "projects/file".into(),
            server: "primary".into(),
            target: "other".into(),
        },
        SmbError::DfsTargetUnreachable {
            path: "projects/file".into(),
            target_count: 1,
            source: Box::new(SmbError::Disconnected),
        },
    ] {
        let error = smb_error(error, 7);
        assert_eq!(error.index(), Some(7));
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(
            error.status(),
            Some(StatusCode::Smb(NtStatus::PATH_NOT_COVERED.0))
        );
        assert!(!error.is_transport());
        assert!(
            error
                .to_string()
                .contains("SMB DFS referrals are unsupported")
        );
    }
}

#[test]
fn unmapped_server_status_is_not_a_transport_failure() {
    let status = NtStatus(0xDEAD_BEEF);
    let error = smb_error(
        SmbError::Protocol {
            status,
            command: Command::Write,
        },
        2,
    );
    assert!(!error.is_transport());
    assert_eq!(error.index(), Some(2));
    assert_eq!(
        error.status(),
        Some(crate::vecfs::StatusCode::Smb(status.0))
    );
}
