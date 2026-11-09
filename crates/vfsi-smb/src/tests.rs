use proptest::prelude::*;

use super::*;

fn test_close_result(succeeded: bool) -> vfsi_core::OpResult<()> {
    if succeeded {
        Ok(())
    } else {
        Err(vfsi_core::OpError::failed(
            VfError::errno(None, libc::EIO as u32),
            None,
        ))
    }
}

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
        |response| test_close_result(matches!(response, Response::Close(true))),
        map_error,
    )
    .unwrap();
    assert_eq!(set_info_failed.file_id, FileId::SENTINEL);
    assert!(matches!(
        set_info_failed.operation_responses.as_slice(),
        [Err("SET_INFO waiter failed")]
    ));
    assert!(set_info_failed.close_result.is_ok());
    assert!(set_info_failed.cardinality_error.is_none());

    let close_failed = decode_open_compound_responses(
        vec![
            Ok(Response::Create(FileId::SENTINEL)),
            Ok(Response::SetInfo),
            Err("CLOSE waiter failed"),
        ],
        3,
        parse_create,
        |response| test_close_result(matches!(response, Response::Close(true))),
        map_error,
    )
    .unwrap();
    assert_eq!(close_failed.file_id, FileId::SENTINEL);
    assert!(matches!(
        close_failed.operation_responses.as_slice(),
        [Ok(Response::SetInfo)]
    ));
    assert!(close_failed.close_result.is_err());
    assert!(close_failed.cardinality_error.is_none());

    let mut deferred_close = None;
    let close_result = finish_compound_close(
        &mut deferred_close,
        close_failed.file_id,
        close_failed.close_result,
    );
    assert!(close_result.unwrap_err().is_transport());
    assert_eq!(deferred_close, Some(CloseState::NoReplay(FileId::SENTINEL)));
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
        |_| test_close_result(false),
        |error| VfError::transport(None, error),
    )
    .unwrap();
    assert_eq!(truncated.file_id, FileId::SENTINEL);
    assert!(matches!(
        truncated.operation_responses.as_slice(),
        [Err("READ waiter failed")]
    ));
    assert!(truncated.close_result.is_err());
    assert!(truncated.cardinality_error.is_some());

    let mut deferred_close = None;
    let close_result = finish_compound_close(
        &mut deferred_close,
        truncated.file_id,
        truncated.close_result,
    );
    assert!(close_result.unwrap_err().is_transport());
    assert_eq!(deferred_close, Some(CloseState::NoReplay(FileId::SENTINEL)));
}

#[test]
fn missing_or_failed_compound_close_response_does_not_prove_success() {
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
    let request = mutation_create_request(create);
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
    let mut deferred = None;
    let result = finish_compound_close(
        &mut deferred,
        file_id,
        Err(vfsi_core::OpError::unknown(
            VfError::transport(None, "injected close failure"),
            None,
        )),
    );
    assert!(result.unwrap_err().is_transport());
    assert_eq!(deferred, Some(CloseState::NoReplay(file_id)));

    finish_compound_close(&mut deferred, file_id, Ok(())).unwrap();
    assert_eq!(deferred, Some(CloseState::NoReplay(file_id)));
}

#[test]
fn vectorized_compound_close_failure_returns_handle_for_deferred_cleanup() {
    let file_id = FileId::SENTINEL;
    let mut deferred_close = None;
    let result = finish_compound_close(
        &mut deferred_close,
        file_id,
        Err(vfsi_core::OpError::unknown(
            VfError::transport(None, "injected fallback CLOSE failure"),
            None,
        )),
    );
    assert!(result.unwrap_err().is_transport());
    assert_eq!(deferred_close, Some(CloseState::NoReplay(file_id)));

    let outcome: CompoundOutcome<Vec<u8>> = CompoundOutcome {
        result: Err(VfError::transport(None, "injected vector read failure")),
        deferred_close,
    };
    let mut owner_queue = Vec::new();
    queue_deferred_closes(&[outcome], &mut owner_queue, |id| id);
    assert_eq!(owner_queue, [CloseState::NoReplay(file_id)]);

    let mut no_failed_close = None;
    finish_compound_close(&mut no_failed_close, file_id, Ok(())).unwrap();
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
    assert_eq!(error.raw_code(), vfsi_core::VF_ERR_RPC);
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
    assert_eq!(error.status(), Some(vfsi_core::StatusCode::Smb(status.0)));
}

#[test]
fn cleanup_errors_preserve_read_data_and_completion_certainty() {
    use vfsi_core::{FailureState, IoProgress, OpError};
    for transport in [false, true] {
        let data = vec![1, 2, 3];
        let pointer = data.as_ptr();
        let cause = if transport {
            VfError::transport(None, "lost CLOSE")
        } else {
            VfError::smb(None, 0xc000_0022)
        };
        let error = finish_read_result(Ok(data), Err(cause), None, 7, 4).unwrap_err();
        assert_eq!(
            error.state(),
            if transport {
                FailureState::Unknown
            } else {
                FailureState::Failed
            }
        );
        let (_, _, progress) = error.into_parts();
        let Some(IoProgress::Read(prefix)) = progress else {
            panic!("missing data")
        };
        assert_eq!(prefix.data().unwrap(), &[1, 2, 3]);
        assert_eq!(prefix.data().unwrap().as_ptr(), pointer);
        assert_eq!(prefix.offset(), 7);
        assert!(prefix.eof());
    }
    let error = finish_read_result(
        Ok(Vec::new()),
        Err(VfError::transport(None, "lost CLOSE")),
        None,
        4,
        9,
    )
    .unwrap_err();
    let Some(IoProgress::Read(prefix)) = error.progress() else {
        panic!("confirmed EOF was lost")
    };
    assert_eq!(prefix.bytes_read(), 0);
    assert!(prefix.eof());
    let read_error = OpError::failed(
        VfError::smb(None, 0xc000_0022),
        Some(IoProgress::Read(vfsi_core::api::ReadResult::owned(
            0,
            vec![7],
            false,
        ))),
    );
    let error = finish_read_result(
        Err(read_error),
        Err(VfError::transport(None, "lost CLOSE")),
        None,
        0,
        2,
    )
    .unwrap_err();
    assert_eq!(error.state(), FailureState::Unknown);
    assert_eq!(
        error.cause().status(),
        Some(vfsi_core::StatusCode::Smb(0xc000_0022))
    );
    assert!(error.progress().is_some());
}

#[cfg(feature = "test-faults")]
#[test]
fn live_raw_and_compound_reads_retain_prefix_after_later_chunk_failure() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    use vfsi_core::{FailureState, IoProgress};
    use vfsi_sync::test_support::borrow_writes;
    let (Ok(server), Ok(share)) = (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = SmbVecFs::connect(
        &server,
        &share,
        &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
        &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
        &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
    )
    .unwrap();
    let max_read = fs.client.params().unwrap().max_read_size as usize;
    assert!(max_read > 0 && max_read <= 16 * 1024 * 1024);
    let data: Vec<_> = (0..max_read + 17)
        .map(|index| (index % 251) as u8)
        .collect();
    for mode in 0..4 {
        let path = format!("/vfsi-rich-read-prefix-{}-{mode}", std::process::id());
        fs.vwrite_impl(&borrow_writes(&[WriteOp::from_path(
            &path,
            VfOffset::At(0),
            data.clone(),
        )
        .with_creation()
        .with_truncate()]))
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeReadChunk { chunk: 1 },
            VfError::transport(None, "injected before second READ"),
        ));
        fs.set_fault_injector(fault.clone());
        let error = match mode {
            0 => {
                let (id, _) = fs.raw_open(&path, libc::O_RDONLY).unwrap();
                let error = fs.raw_read(id, 0, data.len()).unwrap_err();
                fs.close_temporary_handle(id).unwrap();
                error
            }
            1 => fs.compound_read_path(&path, 0, data.len()).unwrap_err(),
            2 => {
                let file = fs
                    .open_raw_impl(Path::new(&path), libc::O_RDONLY, 0)
                    .unwrap();
                let error = fs
                    .read_one(&ReadOp::new(file.clone(), VfOffset::Cur, data.len()))
                    .unwrap_err();
                let tail = fs
                    .read_one(&ReadOp::new(file.clone(), VfOffset::Cur, 17))
                    .unwrap();
                assert_eq!(tail.offset, max_read as u64);
                assert_eq!(tail.data, data[max_read..]);
                fs.close_impl(&file).unwrap();
                error
            }
            _ => fs
                .read_one(&ReadOp::from_path(&path, VfOffset::At(0), data.len()))
                .unwrap_err(),
        };
        assert!(fault.is_consumed());
        assert_eq!(
            error.state(),
            FailureState::Failed,
            "no second READ was submitted"
        );
        assert!(error.cause().is_transport());
        let Some(IoProgress::Read(prefix)) = error.progress() else {
            panic!("missing prefix")
        };
        assert_eq!(prefix.offset(), 0);
        assert_eq!(prefix.data().unwrap(), &data[..max_read]);
        assert!(!prefix.eof());
        assert!(fs.deferred_closes.is_empty());
        fs.unlink_impl(Path::new(&path)).unwrap();
    }
}

#[test]
fn truncated_compound_keeps_validated_read_bytes_while_reporting_structure_error() {
    use vfsi_core::{FailureState, IoProgress};
    enum Reply {
        Create,
        Read(Vec<u8>),
    }
    let decoded = decode_open_compound_responses::<_, &str>(
        vec![Ok(Reply::Create), Ok(Reply::Read(vec![1, 2, 3]))],
        3,
        |_| Ok(FileId::SENTINEL),
        |_| test_close_result(false),
        |error| VfError::transport(None, error),
    )
    .unwrap();
    assert!(decoded.close_result.is_err());
    let Reply::Read(data) = decoded
        .operation_responses
        .into_iter()
        .next()
        .unwrap()
        .unwrap()
    else {
        panic!("READ payload missing");
    };
    let error = finish_read_result(Ok(data), Ok(()), decoded.cardinality_error, 9, 4).unwrap_err();
    assert_eq!(error.state(), FailureState::Unknown);
    assert!(error.cause().is_transport());
    let Some(IoProgress::Read(prefix)) = error.progress() else {
        panic!("confirmed bytes lost")
    };
    assert_eq!(prefix.offset(), 9);
    assert_eq!(prefix.data().unwrap(), &[1, 2, 3]);
}

#[cfg(all(feature = "test-faults", unix))]
#[test]
fn live_vector_read_results_preserve_missing_and_invalid_siblings() {
    use std::os::unix::ffi::OsStringExt;
    use vfsi_core::FailureState;
    use vfsi_sync::test_support::borrow_writes;
    let (Ok(server), Ok(share)) = (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = SmbVecFs::connect(
        &server,
        &share,
        &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
        &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
        &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
    )
    .unwrap();
    let path = format!("/vfsi-rich-read-vector-{}", std::process::id());
    fs.vwrite_impl(&borrow_writes(&[WriteOp::from_path(
        &path,
        VfOffset::At(0),
        b"abcdef".to_vec(),
    )
    .with_creation()
    .with_truncate()]))
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
    let reads = [
        ReadOp::from_path(&path, VfOffset::At(0), 2),
        ReadOp::from_path(&format!("{path}-missing"), VfOffset::At(0), 2),
        ReadOp::from_os_path(&invalid, VfOffset::At(0), 2),
        ReadOp::from_path(&path, VfOffset::At(4), 2),
        ReadOp::from_path(&path, VfOffset::At(u64::MAX), 2),
    ];
    let result = fs.read_results(&reads).unwrap();
    assert_eq!(result.len(), reads.len());
    assert_eq!(result[0].as_ref().unwrap().data, b"ab");
    assert_eq!(result[3].as_ref().unwrap().data, b"ef");
    assert_eq!(
        result[1].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        result[1].as_ref().unwrap_err().cause().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        result[2].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(
        result[4].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(fs.read_results(&[]).unwrap().is_empty());
    assert!(fs.deferred_closes.is_empty());
    fs.unlink_impl(Path::new(&path)).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_native_read_hook_preserves_per_input_outcomes() {
    let (Ok(server), Ok(share)) = (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = SmbVecFs::connect(
        &server,
        &share,
        &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
        &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
        &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
    )
    .unwrap();
    vfsi_sync::test_support::read_vector_outcomes(
        &mut fs,
        Path::new(&format!(
            "/vfsi-native-read-outcomes-{}",
            std::process::id()
        )),
    );
}

fn sync_test_backend() -> Option<SmbVecFs> {
    let (Ok(server), Ok(share)) = (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    Some(
        SmbVecFs::connect(
            &server,
            &share,
            &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
            &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
            &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
        )
        .unwrap(),
    )
}

fn sync_test_files(fs: &mut SmbVecFs, name: &str) -> (PathBuf, Vec<FileOperand>) {
    let path = PathBuf::from(format!("/vfsi-sync-{}-{name}", std::process::id()));
    let first = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
        .unwrap();
    let second = fs.open_raw_impl(&path, libc::O_RDWR, 0).unwrap();
    let third = fs.open_raw_impl(&path, libc::O_RDWR, 0).unwrap();
    (path, vec![first, second, third])
}

#[cfg(feature = "test-faults")]
#[test]
fn live_sync_status_failure_preserves_successful_siblings() {
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let (path, files) = sync_test_files(&mut fs, "closed-server-handle");
    let fd = files[1].fd().unwrap();
    let file_id = fs.open_files[&fd].file_id;
    fs.raw_close(file_id).unwrap();
    let outcomes = fs
        .vfsync_impl(&files, vfsi_core::api::SyncMode::All)
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Failed);
    assert!(!error.cause().is_transport());
    assert!(error.progress().is_none());
    // The test itself closed this server handle; retire that known-dead local
    // registration rather than treating it as an unconfirmed close owner.
    fs.open_files.remove(&fd).unwrap();
    assert!(
        fs.vclose_impl(&[files[0].clone(), files[2].clone()])
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    fs.unlink_impl(&path).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_sync_distinguishes_unsubmitted_from_lost_reply_without_replay() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    for after in [false, true] {
        let Some(mut fs) = sync_test_backend() else {
            return;
        };
        let (path, files) = sync_test_files(&mut fs, if after { "lost" } else { "unsent" });
        let point = if after {
            OpenFaultPoint::AfterSyncReply { index: 1 }
        } else {
            OpenFaultPoint::BeforeSyncDispatch { index: 1 }
        };
        let script = std::sync::Arc::new(FaultScript::one(
            point.clone(),
            VfError::transport(None, "injected synchronization reply loss"),
        ));
        fs.set_fault_injector(script.clone());
        let outcomes = fs
            .vfsync_impl(&files, vfsi_core::api::SyncMode::Data)
            .unwrap();
        assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
        let error = outcomes[1].as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if after {
                vfsi_core::FailureState::Unknown
            } else {
                vfsi_core::FailureState::NotAttempted
            }
        );
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
        assert!(script.is_consumed());
        assert_eq!(
            script
                .visited()
                .iter()
                .filter(|visited| **visited == point)
                .count(),
            1
        );
        assert!(
            fs.vclose_impl(&files)
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
        fs.unlink_impl(&path).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_close_status_failure_preserves_successful_siblings() {
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let (path, files) = sync_test_files(&mut fs, "close-status");
    let fd = files[1].fd().unwrap();
    fs.raw_close(fs.open_files[&fd].file_id).unwrap();
    let outcomes = fs.vclose_impl(&files).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::Failed
    );
    assert!(fs.open_files.is_empty());
    assert_eq!(fs.deferred_closes.len(), 1);
    let before = fs
        .tree
        .close_submissions
        .load(std::sync::atomic::Ordering::Relaxed);
    fs.drain_deferred_closes().unwrap();
    assert_eq!(
        fs.tree
            .close_submissions
            .load(std::sync::atomic::Ordering::Relaxed),
        before
    );
    fs.unlink_impl(&path).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_close_lost_reply_retains_only_uncertain_owner() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let (path, files) = sync_test_files(&mut fs, "close-lost");
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterCloseReply { index: 1 },
        VfError::transport(None, "lost CLOSE reply"),
    ));
    fs.set_fault_injector(script.clone());
    let outcomes = fs.vclose_impl(&files).unwrap();
    assert!(script.is_consumed());
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::Unknown
    );
    assert!(fs.open_files.is_empty());
    assert_eq!(fs.deferred_closes.len(), 1);
    assert!(!fs.deferred_closes[0].can_attempt());
    assert!(fs.read_raw_impl(&files[1], 0, 1).is_err());

    let replacement = fs.open_raw_impl(&path, libc::O_RDWR, 0o666).unwrap();
    fs.write_raw_impl(&replacement, 0, b"replacement").unwrap();
    // Model server FileId reuse deterministically: a replay of this quarantined
    // owner would close the healthy replacement handle.
    let replacement_id = fs.open_files[&replacement.fd().unwrap()].file_id;
    fs.deferred_closes[0].state = CloseState::NoReplay(replacement_id);
    let before = fs
        .tree
        .close_submissions
        .load(std::sync::atomic::Ordering::Relaxed);
    fs.drain_deferred_closes().unwrap();
    fs.drain_deferred_closes().unwrap();
    assert_eq!(
        fs.tree
            .close_submissions
            .load(std::sync::atomic::Ordering::Relaxed),
        before
    );
    assert_eq!(
        fs.read_raw_impl(&replacement, 0, 11).unwrap(),
        b"replacement"
    );
    // Shutdown closes the replacement once and disconnects its original tree;
    // it must not replay the numerically identical quarantined CLOSE.
    fs.shutdown_inner().unwrap();
    assert_eq!(
        fs.tree
            .close_submissions
            .load(std::sync::atomic::Ordering::Relaxed),
        before + 1
    );
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    fs.unlink_impl(&path).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_native_open_lost_reply_preserves_other_acquired_owners() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let paths: Vec<_> = (0..3)
        .map(|index| PathBuf::from(format!("/vfsi-open-reply-{}-{index}", std::process::id())))
        .collect();
    let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterOpenReply { index: 1 },
        VfError::transport(None, "lost OPEN reply"),
    ));
    fs.set_fault_injector(script.clone());
    let outcomes = fs
        .vopen_raw_impl(&refs, &[libc::O_CREAT | libc::O_RDWR; 3], &[0o666; 3])
        .unwrap();
    assert!(script.is_consumed());
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::Unknown
    );
    assert_eq!(fs.open_files.len(), 2);
    assert!(
        fs.deferred_closes.is_empty(),
        "the known test-only resource was cleaned"
    );
    let files: Vec<_> = outcomes.into_iter().filter_map(Result::ok).collect();
    for file in &files {
        fs.write_raw_impl(file, 0, b"live").unwrap();
        assert_eq!(fs.read_raw_impl(file, 0, 4).unwrap(), b"live");
    }
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    for path in paths {
        fs.unlink_impl(&path).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_native_open_aliases_continue_after_a_known_wire_failure() {
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let path = PathBuf::from(format!("/vfsi-open-aliases-{}", std::process::id()));
    let seed = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
        .unwrap();
    fs.write_raw_impl(&seed, 0, b"seed").unwrap();
    fs.close_impl(&seed).unwrap();
    let outcomes = fs
        .vopen_raw_impl(
            &[&path, &path],
            &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, libc::O_RDONLY],
            &[0o666; 2],
        )
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    let mut outcomes = outcomes.into_iter();
    let error = outcomes.next().unwrap().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    let file = outcomes.next().unwrap().unwrap();
    assert_eq!(fs.open_files.len(), 1);
    assert_eq!(fs.read_raw_impl(&file, 0, 4).unwrap(), b"seed");
    assert!(fs.vclose_impl(&[file]).unwrap()[0].is_ok());
    fs.unlink_impl(&path).unwrap();
}

fn write_test_frame(command: Command, status: NtStatus, count: Option<u32>) -> smb2::client::Frame {
    use smb2::pack::{Pack, WriteCursor};
    let mut header = smb2::msg::header::Header::new_request(command);
    header.status = status;
    let mut body = WriteCursor::new();
    if let Some(count) = count {
        WriteResponse {
            count,
            remaining: 0,
            write_channel_info_offset: 0,
            write_channel_info_length: 0,
        }
        .pack(&mut body);
    }
    smb2::client::Frame {
        header,
        body: body.into_inner(),
        raw: Vec::new(),
    }
}

#[test]
fn write_flush_decoder_retains_acknowledged_count_after_later_failures() {
    use vfsi_core::{FailureState, IoProgress};
    for mode in 0..3 {
        let mut responses = vec![Ok(write_test_frame(
            Command::Write,
            NtStatus::SUCCESS,
            Some(3),
        ))];
        match mode {
            0 => {}
            1 => responses.push(Err(SmbError::invalid_data("lost FLUSH reply"))),
            _ => responses.push(Ok(write_test_frame(
                Command::Flush,
                NtStatus::ACCESS_DENIED,
                None,
            ))),
        }
        let error = decode_write_flush(responses, 5).unwrap_err();
        assert_eq!(
            error.state(),
            if mode == 2 {
                FailureState::Failed
            } else {
                FailureState::Unknown
            }
        );
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 3 })
        );
        assert_eq!(error.cause().index(), None);
    }
    for count in [0, 2, 5] {
        assert_eq!(
            decode_write_flush(
                vec![
                    Ok(write_test_frame(
                        Command::Write,
                        NtStatus::SUCCESS,
                        Some(count)
                    )),
                    Ok(write_test_frame(Command::Flush, NtStatus::SUCCESS, None)),
                ],
                5
            )
            .unwrap(),
            count as usize,
            "short writes remain short"
        );
    }
    for (status, count, state) in [
        (NtStatus::SUCCESS, Some(6), FailureState::Unknown),
        (NtStatus::SUCCESS, None, FailureState::Unknown),
        (NtStatus::ACCESS_DENIED, None, FailureState::Failed),
    ] {
        let error = decode_write_flush(
            vec![
                Ok(write_test_frame(Command::Write, status, count)),
                Ok(write_test_frame(Command::Flush, NtStatus::SUCCESS, None)),
            ],
            5,
        )
        .unwrap_err();
        assert_eq!(error.state(), state);
        assert!(
            error.progress().is_none(),
            "unvalidated WRITE counts must not become progress"
        );
    }
}

#[test]
fn truncated_write_compound_keeps_validated_count_and_cleanup_ownership() {
    use vfsi_core::{FailureState, IoProgress};
    for include_flush in [false, true] {
        let mut responses = vec![
            Ok(write_test_frame(Command::Create, NtStatus::SUCCESS, None)),
            Ok(write_test_frame(Command::Write, NtStatus::SUCCESS, Some(3))),
        ];
        if include_flush {
            responses.push(Ok(write_test_frame(
                Command::Flush,
                NtStatus::SUCCESS,
                None,
            )));
        }
        let decoded = decode_open_compound_responses::<_, SmbError>(
            responses,
            4,
            |_| Ok(FileId::SENTINEL),
            |_| test_close_result(true),
            |error| smb_error(error, 0),
        )
        .unwrap();
        assert!(
            decoded.close_result.is_err(),
            "a short vector cannot prove CLOSE succeeded"
        );
        let result = decode_write_flush(decoded.operation_responses, 5);
        let mut deferred_close = None;
        let close =
            finish_compound_close(&mut deferred_close, decoded.file_id, decoded.close_result);
        let error = finish_write_result(result, close, decoded.cardinality_error).unwrap_err();
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 3 })
        );
        assert_eq!(deferred_close, Some(CloseState::NoReplay(FileId::SENTINEL)));
    }
}

#[test]
fn write_cleanup_retains_prefix_and_primary_error_without_claiming_an_untouched_open() {
    use vfsi_core::{FailureState, IoProgress, OpError};
    for transport in [false, true] {
        let close = if transport {
            VfError::transport(None, "close lost")
        } else {
            VfError::errno(None, libc::EIO as u32)
        };
        let error = finish_write_result(Ok(7), Err(close.clone()), None).unwrap_err();
        assert_eq!(
            error.state(),
            if transport {
                FailureState::Unknown
            } else {
                FailureState::Failed
            }
        );
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 7 })
        );
        let original = OpError::failed(
            VfError::errno(None, libc::ENOSPC as u32),
            Some(IoProgress::Write { bytes_written: 2 }),
        );
        let error = finish_write_result(Err(original), Err(close), None).unwrap_err();
        assert_eq!(error.cause().raw_code(), libc::ENOSPC as u32);
        assert_eq!(
            error.state(),
            if transport {
                FailureState::Unknown
            } else {
                FailureState::Failed
            }
        );
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: 2 })
        );
    }
    let error = finish_write_result(
        Err(OpError::not_attempted(VfError::errno(
            None,
            libc::EOVERFLOW as u32,
        ))),
        Ok(()),
        None,
    )
    .unwrap_err();
    assert_eq!(
        error.state(),
        FailureState::Failed,
        "CREATE already executed"
    );
    assert!(error.progress().is_none());
    assert_eq!(finish_write_result(Ok(5), Ok(()), None).unwrap(), 5);
}

#[cfg(feature = "test-faults")]
#[test]
fn live_raw_descriptor_and_path_writes_retain_prefixes_and_cursors_without_replay() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    use vfsi_core::{FailureState, IoProgress};
    let (Ok(server), Ok(share)) = (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = SmbVecFs::connect(
        &server,
        &share,
        &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
        &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
        &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
    )
    .unwrap();
    let max_write = fs.client.params().unwrap().max_write_size as usize;
    assert!(max_write > 0 && max_write <= 16 * 1024 * 1024);
    let data: Vec<_> = (0..max_write + 17)
        .map(|index| (index % 251) as u8)
        .collect();
    for mode in 0..3 {
        for boundary in 0..3 {
            for transport in [false, true] {
                let path = format!(
                    "/vfsi-rich-write-{}-{mode}-{boundary}-{transport}",
                    std::process::id()
                );
                let file = fs
                    .open_raw_impl(
                        Path::new(&path),
                        libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
                        0o666,
                    )
                    .unwrap();
                let point = match boundary {
                    0 => OpenFaultPoint::BeforeWriteChunk { chunk: 0 },
                    1 => OpenFaultPoint::BeforeWriteChunk { chunk: 1 },
                    _ => OpenFaultPoint::AfterWriteChunk { chunk: 0 },
                };
                let fault = Arc::new(FaultScript::one(
                    point,
                    if transport {
                        VfError::transport(None, "injected WRITE boundary failure")
                    } else {
                        VfError::errno(None, libc::ENOSPC as u32)
                    },
                ));
                fs.set_fault_injector(fault.clone());
                let error = match mode {
                    0 => {
                        let id = fs.open_files[&file.fd().unwrap()].file_id;
                        fs.raw_write(id, 0, &data).unwrap_err()
                    }
                    1 => fs
                        .write_one(WriteOp::new(&file, VfOffset::Cur, data.as_slice()))
                        .unwrap_err(),
                    _ => fs
                        .compound_write_path(&path, 0, &data, false, false)
                        .unwrap_err(),
                };
                assert!(fault.is_consumed());
                assert_eq!(
                    error.state(),
                    if boundary == 0 && mode != 2 {
                        FailureState::NotAttempted
                    } else {
                        FailureState::Failed
                    }
                );
                assert_eq!(error.cause().is_transport(), transport);
                assert_eq!(error.cause().index(), None);
                let confirmed = if boundary == 0 { 0 } else { max_write };
                assert_eq!(
                    error.progress(),
                    (confirmed != 0).then_some(&IoProgress::Write {
                        bytes_written: confirmed
                    })
                );
                assert_eq!(
                    fs.seek_raw_impl(&file, 0, SeekFrom::Cur).unwrap(),
                    if mode == 1 { confirmed as i64 } else { 0 }
                );
                assert_eq!(
                    fs.read_raw_impl(&file, 0, data.len()).unwrap(),
                    data[..confirmed]
                );
                assert!(fs.deferred_closes.is_empty());
                assert_eq!(
                    fault
                        .visited()
                        .iter()
                        .filter(|point| matches!(point, OpenFaultPoint::AfterWriteChunk { .. }))
                        .count(),
                    usize::from(boundary != 0),
                    "no replay and no second write"
                );
                fs.close_impl(&file).unwrap();
                fs.unlink_impl(Path::new(&path)).unwrap();
            }
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_vector_write_results_keep_successes_after_invalid_and_missing_inputs() {
    use vfsi_core::FailureState;
    let (Ok(server), Ok(share)) = (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = SmbVecFs::connect(
        &server,
        &share,
        &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
        &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
        &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
    )
    .unwrap();
    let paths: Vec<_> = (0..2)
        .map(|index| PathBuf::from(format!("/vfsi-write-vector-{}-{index}", std::process::id())))
        .collect();
    let files: Vec<_> = paths
        .iter()
        .map(|path| {
            fs.open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
                .unwrap()
        })
        .collect();
    let invalid = FileOperand::from_fd(i32::MAX);
    let missing_path = format!("/vfsi-write-missing-{}", std::process::id());
    let missing = FileOperand::from_path(&missing_path);
    let writes = [
        WriteOp::at(&files[0], 0, b"abc".as_slice()),
        WriteOp::at(&invalid, 0, b"bad".as_slice()),
        WriteOp::at(&missing, 0, b"bad".as_slice()),
        WriteOp::at(&files[1], 0, b"xyz".as_slice()),
    ];
    assert!(fs.write_results(&[]).unwrap().is_empty());
    let results = fs.write_results(&writes).unwrap();
    assert_eq!(results.len(), writes.len());
    for index in [0, 3] {
        assert_eq!(results[index].as_ref().unwrap().bytes_written, 3);
    }
    for (index, state) in [(1, FailureState::NotAttempted), (2, FailureState::Failed)] {
        let error = results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), state);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    assert_eq!(fs.read_raw_impl(&files[0], 0, 3).unwrap(), b"abc");
    assert_eq!(fs.read_raw_impl(&files[1], 0, 3).unwrap(), b"xyz");
    let existing_path = FileOperand::from_os_path(&paths[0]);
    let invalid_mutations = [
        WriteOp::at(&existing_path, u64::MAX, b"bad".as_slice())
            .with_creation()
            .with_truncate(),
        WriteOp::at(&missing, u64::MAX, b"bad".as_slice())
            .with_creation()
            .with_truncate(),
        WriteOp::at(&files[1], 0, b"END".as_slice()),
    ];
    let results = fs.write_results(&invalid_mutations).unwrap();
    for result in &results[..2] {
        let error = result.as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().raw_code(), libc::EOVERFLOW as u32);
        assert!(error.progress().is_none());
    }
    assert!(results[2].is_ok());
    assert_eq!(
        fs.read_raw_impl(&files[0], 0, 16).unwrap(),
        b"abc",
        "invalid write must not truncate"
    );
    assert_eq!(fs.read_raw_impl(&files[1], 0, 3).unwrap(), b"END");
    assert!(
        !fs.exists_impl(Path::new(&missing_path)).unwrap(),
        "invalid write must not create"
    );
    assert!(fs.deferred_closes.is_empty());
    assert!(fs.vclose_impl(&files).unwrap().iter().all(Result::is_ok));
    for path in &paths {
        fs.unlink_impl(path).unwrap();
    }
}

#[cfg(all(feature = "test-faults", unix))]
#[test]
fn live_concurrent_write_results_isolate_rejections_and_preserve_wire_failures() {
    use std::os::unix::fs::PermissionsExt;
    use vfsi_core::FailureState;
    let (Ok(server), Ok(share)) = (
        std::env::var("VFSI_SMB_SERVER"),
        std::env::var("VFSI_SMB_SHARE"),
    ) else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = SmbVecFs::connect(
        &server,
        &share,
        &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
        &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
        &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
    )
    .unwrap();
    let paths: Vec<_> = (0..3)
        .map(|index| {
            PathBuf::from(format!(
                "/vfsi-concurrent-write-{}-{index}",
                std::process::id()
            ))
        })
        .collect();
    let files: Vec<_> = paths
        .iter()
        .map(|path| {
            let file = fs
                .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
                .unwrap();
            fs.write_raw_impl(&file, 0, b"old").unwrap();
            file
        })
        .collect();
    let operands: Vec<_> = paths
        .iter()
        .map(|path| FileOperand::from_os_path(path))
        .collect();
    let invalid = FileOperand::from_path_bytes(&[0xff]);
    let writes = [
        WriteOp::at(&operands[0], 0, b"abc".as_slice()),
        WriteOp::at(&invalid, 0, b"bad".as_slice()),
        WriteOp::at(&operands[1], u64::MAX, b"bad".as_slice()),
        WriteOp::at(&operands[2], 0, b"xyz".as_slice()),
    ];
    let results = fs.write_results(&writes).unwrap();
    assert!(
        fs.last_writev_was_concurrent,
        "healthy paths retain concurrent dispatch"
    );
    assert_eq!(results.len(), writes.len());
    for index in [0, 3] {
        assert_eq!(results[index].as_ref().unwrap().bytes_written, 3);
        assert_eq!(results[index].as_ref().unwrap().file, *writes[index].file());
    }
    for (index, code) in [(1, libc::EILSEQ), (2, libc::EOVERFLOW)] {
        let error = results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().raw_code(), code as u32);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    assert_eq!(fs.read_raw_impl(&files[0], 0, 3).unwrap(), b"abc");
    assert_eq!(fs.read_raw_impl(&files[1], 0, 3).unwrap(), b"old");
    assert_eq!(fs.read_raw_impl(&files[2], 0, 3).unwrap(), b"xyz");
    assert!(fs.vclose_impl(&files).unwrap().iter().all(Result::is_ok));
    // The managed fixture exposes its backing tree for deterministic ACL failure.
    // External fixtures still exercise the rejection/concurrent-success cases above.
    if let Ok(root) = std::env::var("VFSI_SMB_LOCAL_ROOT") {
        let denied = Path::new(&root).join(paths[1].strip_prefix("/").unwrap());
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o444)).unwrap();
        let writes: Vec<_> = operands
            .iter()
            .map(|file| WriteOp::at(file, 0, b"NEW".as_slice()))
            .collect();
        let results = fs.write_results(&writes).unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(fs.last_writev_was_concurrent);
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok() && results[2].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Failed);
        assert!(matches!(
            error.cause().status(),
            Some(vfsi_core::StatusCode::Smb(_))
        ));
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
        for (index, path) in paths.iter().enumerate() {
            let file = fs.open_raw_impl(path, libc::O_RDONLY, 0).unwrap();
            assert_eq!(
                fs.read_raw_impl(&file, 0, 3).unwrap(),
                if index == 1 { b"old" } else { b"NEW" }
            );
            fs.close_impl(&file).unwrap();
        }
    }
    assert!(fs.deferred_closes.is_empty());
    for path in &paths {
        fs.unlink_impl(path).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_setattrs_isolates_unsupported_missing_and_closed_inputs() {
    use vfsi_core::FailureState;
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let prefix = format!("/vfsi-setattrs-{}", std::process::id());
    let paths: Vec<_> = (0..3)
        .map(|index| PathBuf::from(format!("{prefix}-{index}")))
        .collect();
    let mut files = Vec::new();
    for path in &paths {
        let file = fs
            .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
            .unwrap();
        fs.write_raw_impl(&file, 0, b"original").unwrap();
        files.push(file);
    }
    let missing = PathBuf::from(format!("{prefix}-missing"));
    let updates = [
        VfAttrs {
            file: files[0].clone(),
            masks: AttrMask::SIZE,
            size: 1,
            ..Default::default()
        },
        VfAttrs {
            file: files[1].clone(),
            masks: AttrMask::MODE,
            mode: 0o600,
            ..Default::default()
        },
        VfAttrs {
            file: FileOperand::from_os_path(&missing),
            masks: AttrMask::SIZE,
            size: 1,
            ..Default::default()
        },
        VfAttrs {
            file: files[2].clone(),
            masks: AttrMask::SIZE,
            size: 3,
            ..Default::default()
        },
    ];
    let results = fs.vsetattrs_raw_impl(&updates).unwrap();
    assert_eq!(results.len(), 4);
    assert!(results[0].is_ok() && results[3].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(
        results[2].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        results[2].as_ref().unwrap_err().cause().kind(),
        std::io::ErrorKind::NotFound
    );
    for result in [&results[1], &results[2]] {
        let error = result.as_ref().unwrap_err();
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
    }
    assert_eq!(fs.read_raw_impl(&files[0], 0, 8).unwrap(), b"o");
    assert_eq!(fs.read_raw_impl(&files[1], 0, 8).unwrap(), b"original");
    assert_eq!(fs.read_raw_impl(&files[2], 0, 8).unwrap(), b"ori");
    let nofollow = fs
        .vsetattrs_raw_nofollow_impl(&[
            VfAttrs {
                file: files[0].clone(),
                ..Default::default()
            },
            updates[0].clone(),
            VfAttrs {
                file: files[2].clone(),
                ..Default::default()
            },
        ])
        .unwrap();
    assert!(nofollow[0].is_ok() && nofollow[2].is_ok());
    assert_eq!(
        nofollow[1].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(fs.vclose_impl(&files).unwrap().iter().all(Result::is_ok));
    let closed = fs.vsetattrs_raw_impl(&updates[..1]).unwrap();
    assert_eq!(
        closed[0].as_ref().unwrap_err().cause().raw_code(),
        libc::EBADF as u32
    );
    for path in &paths {
        fs.unlink_impl(path).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_rename_preserves_independent_results_replacement_and_cleanup() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let prefix = format!("/vfsi-rename-outcomes-{}", std::process::id());
    let source: Vec<_> = (0..3)
        .map(|i| PathBuf::from(format!("{prefix}-src-{i}")))
        .collect();
    let target: Vec<_> = (0..5)
        .map(|i| PathBuf::from(format!("{prefix}-dst-{i}")))
        .collect();
    for (index, path) in source.iter().enumerate() {
        let file = fs
            .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
            .unwrap();
        fs.write_raw_impl(&file, 0, format!("source-{index}").as_bytes())
            .unwrap();
        fs.close_impl(&file).unwrap();
    }
    let old = fs
        .open_raw_impl(
            &target[0],
            libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
            0o666,
        )
        .unwrap();
    fs.write_raw_impl(&old, 0, b"replace-me").unwrap();
    fs.close_impl(&old).unwrap();
    fs.mkdir_raw_impl(&target[4], 0o777).unwrap();
    let fault = Arc::new(FaultScript::new([]));
    fs.set_fault_injector(fault.clone());
    assert!(fs.vrename_impl(&[]).unwrap().is_empty());
    let path = FileOperand::from_os_path;
    let results = fs
        .vrename_impl(&[
            (path(&source[0]), path(&target[0])),
            (
                FileOperand::from_path(&format!("{prefix}-missing")),
                path(&target[1]),
            ),
            (path(&source[2]), path(&target[2])),
            (FileOperand::from_fd(i32::MAX), path(&target[3])),
            (path(&source[1]), path(&target[4])),
        ])
        .unwrap();
    assert_eq!(results.len(), 5);
    assert!(results[0].is_ok() && results[2].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        results[1].as_ref().unwrap_err().cause().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        results[3].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(
        results[3].as_ref().unwrap_err().cause().raw_code(),
        libc::EBADF as u32
    );
    assert_eq!(
        results[4].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    for index in [1, 3, 4] {
        assert_eq!(results[index].as_ref().unwrap_err().cause().index(), None);
        assert!(results[index].as_ref().unwrap_err().progress().is_none());
    }
    assert_eq!(
        fault
            .visited()
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::BeforeRenameCompound { .. }))
            .count(),
        4
    );
    assert!(
        fs.deferred_closes.is_empty(),
        "failed SET_INFO must close its acquired temporary owner"
    );
    for index in [0, 2] {
        let file = fs.open_raw_impl(&target[index], libc::O_RDONLY, 0).unwrap();
        assert_eq!(
            fs.read_raw_impl(&file, 0, 32).unwrap(),
            format!("source-{index}").as_bytes()
        );
        fs.close_impl(&file).unwrap();
        assert!(!fs.exists_impl(&source[index]).unwrap());
        fs.unlink_impl(&target[index]).unwrap();
    }
    assert!(fs.exists_impl(&source[1]).unwrap());
    fs.unlink_impl(&source[1]).unwrap();
    fs.remove_impl(&target[4], false).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_rename_faults_distinguish_unsubmitted_from_unknown_without_replay() {
    use vfsi_core::{
        FailureState, TransportKind,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    for after in [false, true] {
        let Some(mut fs) = sync_test_backend() else {
            return;
        };
        let prefix = format!("/vfsi-rename-fault-{}-{after}", std::process::id());
        let sources: Vec<_> = (0..3)
            .map(|i| PathBuf::from(format!("{prefix}-src-{i}")))
            .collect();
        let targets: Vec<_> = (0..3)
            .map(|i| PathBuf::from(format!("{prefix}-dst-{i}")))
            .collect();
        for path in &sources {
            let file = fs
                .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
                .unwrap();
            fs.close_impl(&file).unwrap();
        }
        let point = if after {
            OpenFaultPoint::AfterRenameCompoundReply { first: 1 }
        } else {
            OpenFaultPoint::BeforeRenameCompound { first: 1 }
        };
        let fault = Arc::new(FaultScript::one(
            point.clone(),
            VfError::transport_with_kind(
                None,
                TransportKind::Timeout,
                "selected rename reply unavailable",
            ),
        ));
        fs.set_fault_injector(fault.clone());
        let pairs: Vec<_> = sources
            .iter()
            .zip(&targets)
            .map(|(a, b)| (FileOperand::from_os_path(a), FileOperand::from_os_path(b)))
            .collect();
        let results = fs.vrename_impl(&pairs).unwrap();
        assert!(results[0].is_ok() && results[2].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if after {
                FailureState::Unknown
            } else {
                FailureState::NotAttempted
            }
        );
        assert_eq!(error.cause().transport_kind(), Some(TransportKind::Timeout));
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
        assert!(fault.is_consumed());
        assert_eq!(
            fault
                .visited()
                .iter()
                .filter(|visited| **visited == point)
                .count(),
            1
        );
        assert!(fs.deferred_closes.is_empty());
        for index in 0..3 {
            let changed = after || index != 1;
            assert_eq!(fs.exists_impl(&sources[index]).unwrap(), !changed);
            assert_eq!(fs.exists_impl(&targets[index]).unwrap(), changed);
            fs.unlink_impl(if changed {
                &targets[index]
            } else {
                &sources[index]
            })
            .unwrap();
        }
    }
}

#[cfg(all(feature = "test-faults", feature = "server-copy"))]
#[test]
fn live_server_copy_keeps_acknowledged_waves_without_fallback_or_replay() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    for lost in [false, true] {
        let Some(mut fs) = sync_test_backend() else {
            return;
        };
        assert!(fs.server_copy_enabled);
        let root = PathBuf::from(format!("/copy-wave-{}-{lost}", std::process::id()));
        fs.ensure_dir_impl(&root, 0o777).unwrap();
        let sources: Vec<_> = (0..3).map(|i| root.join(format!("source-{i}"))).collect();
        let targets: Vec<_> = (0..3).map(|i| root.join(format!("target-{i}"))).collect();
        let data = vec![b'S'; 17 << 20];
        for (index, source) in sources.iter().enumerate() {
            let file = fs
                .open_raw_impl(source, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
                .unwrap();
            fs.write_raw_impl(&file, 0, if index == 1 { &data } else { b"small" })
                .unwrap();
            fs.close_impl(&file).unwrap();
        }
        let target = fs
            .open_raw_impl(
                &targets[1],
                libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
                0o666,
            )
            .unwrap();
        fs.write_raw_impl(&target, 0, &vec![b'T'; 18 << 20])
            .unwrap();
        fs.close_impl(&target).unwrap();
        let point = if lost {
            OpenFaultPoint::AfterSmbCopyReply { copied: 16 << 20 }
        } else {
            OpenFaultPoint::BeforeSmbCopyRequest { copied: 16 << 20 }
        };
        let cause = if lost {
            VfError::transport(None, "lost later COPYCHUNK reply")
        } else {
            VfError::errno(None, libc::EACCES as u32)
        };
        let fault = Arc::new(FaultScript::one(point.clone(), cause));
        fs.set_fault_injector(fault.clone());
        let pairs: Vec<_> = sources
            .iter()
            .zip(&targets)
            .map(|(source, target)| ExtentPair::from_os_paths(source, 0, target, 0, None))
            .collect();
        let outcomes = fs.vcopy_impl(&pairs, CopyOptions::new()).unwrap();
        assert!(fault.is_consumed());
        assert!(outcomes[0].is_ok());
        assert_eq!(
            outcomes[1].as_ref().unwrap_err().state(),
            if lost {
                FailureState::Unknown
            } else {
                FailureState::Failed
            }
        );
        if lost {
            assert_eq!(
                outcomes[2].as_ref().unwrap_err().state(),
                FailureState::NotAttempted
            );
        } else {
            assert!(outcomes[2].is_ok());
        }
        assert!(
            fs.server_copy_enabled,
            "failures after acknowledged bytes do not select fallback"
        );
        assert_eq!(
            fault
                .visited()
                .iter()
                .filter(|visited| **visited == point)
                .count(),
            1
        );
        assert!(fs.open_files.is_empty() && fs.deferred_closes.is_empty());
        let file = fs.open_raw_impl(&targets[1], libc::O_RDONLY, 0).unwrap();
        assert_eq!(fs.read_raw_impl(&file, 0, 4).unwrap(), b"SSSS");
        assert_eq!(
            fs.read_raw_impl(&file, 16 << 20, 4).unwrap(),
            if lost { b"SSSS" } else { b"TTTT" }
        );
        assert_eq!(
            fs.read_raw_impl(&file, 17 << 20, 4).unwrap(),
            b"TTTT",
            "no final truncation after failure"
        );
        fs.close_impl(&file).unwrap();
        for path in sources.iter().chain(&targets) {
            if fs.exists_impl(path).unwrap() {
                fs.unlink_impl(path).unwrap();
            }
        }
        fs.remove_impl(&root, false).unwrap();
    }
}

#[cfg(all(feature = "test-faults", feature = "server-copy"))]
#[test]
fn live_server_copy_surfaces_cleanup_failure_and_retains_exact_owner() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    for destination in [false, true] {
        let Some(mut fs) = sync_test_backend() else {
            return;
        };
        let source = PathBuf::from(format!("/copy-close-{}-{destination}", std::process::id()));
        let target = PathBuf::from(format!(
            "/copy-close-{}-{destination}-target",
            std::process::id()
        ));
        let file = fs
            .open_raw_impl(&source, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
            .unwrap();
        fs.write_raw_impl(&file, 0, b"copied").unwrap();
        fs.close_impl(&file).unwrap();
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforeCopyCleanup { destination },
            VfError::errno(None, libc::EACCES as u32),
        ));
        fs.set_fault_injector(fault.clone());
        let results = fs
            .vcopy_impl(
                &[ExtentPair::from_os_paths(&source, 0, &target, 0, None)],
                CopyOptions::new(),
            )
            .unwrap();
        assert_eq!(
            results[0].as_ref().unwrap_err().state(),
            FailureState::Failed
        );
        assert!(fault.is_consumed());
        assert!(fs.open_files.is_empty());
        assert_eq!(
            fs.deferred_closes.len(),
            1,
            "only the unconfirmed CLOSE remains owned"
        );
        fs.drain_deferred_closes().unwrap();
        assert!(fs.deferred_closes.is_empty());
        let file = fs.open_raw_impl(&target, libc::O_RDONLY, 0).unwrap();
        assert_eq!(fs.read_raw_impl(&file, 0, 32).unwrap(), b"copied");
        fs.close_impl(&file).unwrap();
        fs.unlink_impl(&source).unwrap();
        fs.unlink_impl(&target).unwrap();
    }
}

#[cfg(all(feature = "test-faults", feature = "server-copy"))]
#[test]
fn live_server_copy_finalizes_retained_destination_after_path_replacement() {
    use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};
    struct ReplaceDestination {
        original: PathBuf,
        saved: PathBuf,
        done: std::sync::Mutex<bool>,
    }
    impl FaultInjector for ReplaceDestination {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()> {
            if *point == (OpenFaultPoint::AfterSmbCopyReply { copied: 0 }) {
                let mut done = self.done.lock().unwrap();
                assert!(!*done, "one admitted COPY request, never replay");
                std::fs::rename(&self.original, &self.saved).unwrap();
                std::fs::write(&self.original, b"replacement").unwrap();
                *done = true;
            }
            Ok(())
        }
    }
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let Ok(backing) = std::env::var("VFSI_SMB_LOCAL_ROOT") else {
        assert_ne!(std::env::var("VFSI_SMB_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let root = PathBuf::from(format!("/copy-identity-{}", std::process::id()));
    fs.ensure_dir_impl(&root, 0o777).unwrap();
    let source = root.join("source");
    let target = root.join("target");
    let saved = root.join("saved");
    for (path, data) in [
        (&source, b"source".as_slice()),
        (&target, b"old destination tail".as_slice()),
    ] {
        let file = fs
            .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
            .unwrap();
        fs.write_raw_impl(&file, 0, data).unwrap();
        fs.close_impl(&file).unwrap();
    }
    let fault = Arc::new(ReplaceDestination {
        original: PathBuf::from(&backing).join(target.strip_prefix("/").unwrap()),
        saved: PathBuf::from(&backing).join(saved.strip_prefix("/").unwrap()),
        done: std::sync::Mutex::new(false),
    });
    fs.set_fault_injector(fault.clone());
    let results = fs
        .vcopy_impl(
            &[ExtentPair::from_os_paths(&source, 0, &target, 0, None)],
            CopyOptions::new(),
        )
        .unwrap();
    assert!(results[0].is_ok(), "{:?}", results[0]);
    assert!(*fault.done.lock().unwrap());
    assert_eq!(
        std::fs::read(&fault.original).unwrap(),
        b"replacement",
        "SETATTR must never re-resolve the replaced path"
    );
    assert_eq!(
        std::fs::read(&fault.saved).unwrap(),
        b"source",
        "truncate the retained destination after COPY"
    );
    assert!(fs.open_files.is_empty() && fs.deferred_closes.is_empty());
    for path in [&source, &target, &saved] {
        fs.unlink_impl(path).unwrap();
    }
    fs.remove_impl(&root, false).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_remove_faults_keep_independent_siblings_and_cleanup_owners() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    for after in [false, true] {
        let Some(mut fs) = sync_test_backend() else {
            return;
        };
        let paths: Vec<_> = (0..3)
            .map(|index| {
                PathBuf::from(format!(
                    "/remove-fault-{}-{after}-{index}",
                    std::process::id()
                ))
            })
            .collect();
        for path in &paths {
            let file = fs
                .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o666)
                .unwrap();
            fs.close_impl(&file).unwrap();
        }
        let point = if after {
            OpenFaultPoint::AfterRemoveCompoundReply { first: 1 }
        } else {
            OpenFaultPoint::BeforeRemoveCompound { first: 1 }
        };
        let fault = Arc::new(FaultScript::one(
            point.clone(),
            VfError::transport(None, "selected remove reply unavailable"),
        ));
        fs.set_fault_injector(fault.clone());
        let files: Vec<_> = paths
            .iter()
            .map(|path| FileOperand::from_os_path(path))
            .collect();
        let results = fs.vremove_impl(&files).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok() && results[2].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if after {
                FailureState::Unknown
            } else {
                FailureState::NotAttempted
            }
        );
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
        assert!(fault.is_consumed());
        assert_eq!(
            fault
                .visited()
                .iter()
                .filter(|visited| **visited == point)
                .count(),
            1
        );
        assert!(fs.open_files.is_empty() && fs.deferred_closes.is_empty());
        for (index, path) in paths.iter().enumerate() {
            let present = !after && index == 1;
            assert_eq!(fs.exists_impl(path).unwrap(), present);
            if present {
                fs.unlink_impl(path).unwrap();
            }
        }
    }
}

#[test]
fn compound_close_fallback_requires_definite_related_handle_rejection() {
    use vfsi_core::{FailureState, OpError};
    for status in [0xc0000008, 0xc000000d, 0xc0000128, 0xc0000022] {
        for state in [
            FailureState::NotAttempted,
            FailureState::Failed,
            FailureState::Unknown,
        ] {
            let cause = VfError::smb(None, status);
            let error = match state {
                FailureState::NotAttempted => OpError::not_attempted(cause),
                FailureState::Failed => OpError::failed(cause, None),
                FailureState::Unknown => OpError::unknown(cause, None),
            };
            assert_eq!(
                close_can_fallback(&Err(error)),
                state == FailureState::Failed && matches!(status, 0xc0000008 | 0xc000000d)
            );
        }
    }
    assert!(!close_can_fallback(&Ok(())));
}

#[cfg(feature = "test-faults")]
#[test]
fn live_unsubmitted_close_keeps_owner_until_one_cleanup_attempt() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let (path, files) = sync_test_files(&mut fs, "close-unsent-owner");
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseItem { index: 1 },
        VfError::transport(None, "CLOSE was not submitted"),
    ));
    fs.set_fault_injector(script.clone());
    let outcomes = fs.vclose_impl(&files).unwrap();
    assert!(script.is_consumed());
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::NotAttempted
    );
    assert_eq!(fs.open_files.len(), 1);
    assert!(fs.deferred_closes.is_empty());
    assert!(fs.read_raw_impl(&files[1], 0, 1).is_ok());
    fs.close_deferred(&files[1]).unwrap();
    assert!(fs.open_files.is_empty());
    let before = fs
        .tree
        .close_submissions
        .load(std::sync::atomic::Ordering::Relaxed);
    fs.drain_deferred_closes().unwrap();
    assert_eq!(
        fs.tree
            .close_submissions
            .load(std::sync::atomic::Ordering::Relaxed),
        before
    );
    fs.unlink_impl(&path).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_close_owners_never_cross_a_replacement_session() {
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let (path, files) = sync_test_files(&mut fs, "close-old-session");
    let old_tree = fs.tree.clone();
    for (index, ready) in [(1, true), (2, false)] {
        let open = fs.open_files.remove(&files[index].fd().unwrap()).unwrap();
        let state = if ready {
            CloseState::Ready(open.file_id)
        } else {
            CloseState::NoReplay(open.file_id)
        };
        fs.deferred_closes
            .push(DeferredClose::new(open.tree, state));
    }
    let before = old_tree
        .close_submissions
        .load(std::sync::atomic::Ordering::Relaxed);
    fs.client.connection().mark_dead();
    // Fresh directory opens use the existing SDK reconnect path.
    let (_, cursor) = fs
        .read_directory_page(Path::new("/"), AttrMask::MODE, None, 32)
        .unwrap();
    drop(cursor);
    assert!(!old_tree.is_current());
    let replacement = fs.open_raw_impl(&path, libc::O_RDWR, 0o666).unwrap();
    fs.write_raw_impl(&replacement, 0, b"replacement").unwrap();
    fs.drain_deferred_closes().unwrap();
    assert!(fs.deferred_closes.is_empty());
    assert!(fs.vclose_impl(&[files[0].clone()]).unwrap()[0].is_ok());
    assert_eq!(
        old_tree
            .close_submissions
            .load(std::sync::atomic::Ordering::Relaxed),
        before
    );
    assert_eq!(
        fs.read_raw_impl(&replacement, 0, 11).unwrap(),
        b"replacement"
    );
    fs.close_impl(&replacement).unwrap();
    fs.unlink_impl(&path).unwrap();
}

#[test]
fn wrong_command_close_replies_are_unknown_and_cannot_trigger_fallback() {
    for status in [NtStatus::SUCCESS, NtStatus::INVALID_PARAMETER] {
        let frame = write_test_frame(Command::Write, status, None);
        let cause = require_status(&frame, Command::Close, 0).unwrap_err();
        let result = Err(wire_failure(cause, None));
        assert_eq!(
            result.as_ref().unwrap_err().state(),
            vfsi_core::FailureState::Unknown
        );
        assert!(!close_can_fallback(&result));
        let mut owner = None;
        finish_compound_close(&mut owner, FileId::SENTINEL, result).unwrap_err();
        assert_eq!(owner, Some(CloseState::NoReplay(FileId::SENTINEL)));
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_unsupported_open_mode_does_not_suppress_valid_siblings() {
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let paths: Vec<_> = (0..3)
        .map(|index| PathBuf::from(format!("/vfsi-open-mode-{}-{index}", std::process::id())))
        .collect();
    let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
    let outcomes = fs
        .vopen_raw_impl(
            &refs,
            &[libc::O_CREAT | libc::O_RDWR; 3],
            &[0o666, 0o600, 0o666],
        )
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    let error = outcomes[1].as_ref().unwrap_err();
    assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
    assert_eq!(error.cause().raw_code(), vfsi_core::VF_ERR_UNSUPPORTED);
    assert_eq!(error.cause().index(), None);
    assert_eq!(fs.open_files.len(), 2);
    assert_eq!(
        fs.client_stat(paths[1].to_str().unwrap().trim_start_matches('/'))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    let files: Vec<_> = outcomes.into_iter().filter_map(Result::ok).collect();
    for file in &files {
        fs.write_raw_impl(file, 0, b"healthy").unwrap();
        assert_eq!(fs.read_raw_impl(file, 0, 7).unwrap(), b"healthy");
    }
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    for index in [0, 2] {
        fs.unlink_impl(&paths[index]).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_stale_descriptors_reject_reused_ids_and_keep_current_siblings() {
    use vfsi_core::FailureState;
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let old_path = PathBuf::from(format!("/vfsi-stale-owner-{}", std::process::id()));
    let new_path = PathBuf::from(format!("/vfsi-live-owner-{}", std::process::id()));
    let stale = fs
        .open_raw_impl(
            &old_path,
            libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
            0o666,
        )
        .unwrap();
    fs.write_raw_impl(&stale, 0, b"old data").unwrap();
    fs.client.connection().mark_dead();
    let (_, mut cursor) = fs
        .read_directory_page(Path::new("/"), AttrMask::MODE, None, 32)
        .unwrap();
    // Complete the reader synchronously: a Drop-enqueued CLOSE must not race
    // the zero-RPC assertions below.
    while let Some(next) = cursor {
        (_, cursor) = fs
            .read_directory_page(Path::new("/"), AttrMask::MODE, Some(next), 32)
            .unwrap();
    }
    let live = fs
        .open_raw_impl(
            &new_path,
            libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
            0o666,
        )
        .unwrap();
    fs.write_raw_impl(&live, 0, b"replacement").unwrap();
    // Deterministically model reuse across sessions. The numeric ID alone is
    // insufficient: the old owner must never address this replacement open.
    let id = fs.open_files[&live.fd().unwrap()].file_id;
    fs.open_files.get_mut(&stale.fd().unwrap()).unwrap().file_id = id;
    let before = fs.client.diagnostics().primary.metrics.requests_sent;
    assert!(fs.read_raw_impl(&stale, 0, 4).is_err());
    assert!(fs.write_raw_impl(&stale, 0, b"EVIL").is_err());
    assert!(fs.seek_raw_impl(&stale, 0, SeekFrom::End).is_err());
    let mut stale_attrs = [VfAttrs {
        file: stale.clone(),
        masks: AttrMask::SIZE,
        ..Default::default()
    }];
    let attrs = fs.vgetattrs_impl(&mut stale_attrs).unwrap();
    let updates = fs
        .vsetattrs_raw_impl(&[VfAttrs {
            file: stale.clone(),
            masks: AttrMask::SIZE,
            size: 1,
            ..Default::default()
        }])
        .unwrap();
    let sync = fs
        .vfsync_impl(std::slice::from_ref(&stale), vfsi_core::api::SyncMode::Data)
        .unwrap();
    for result in [&attrs[0], &updates[0], &sync[0]] {
        let error = result.as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert!(error.cause().is_transport());
        assert!(error.progress().is_none());
    }
    assert_eq!(
        fs.client.diagnostics().primary.metrics.requests_sent,
        before,
        "stale descriptor operations must not reach the replacement session"
    );
    assert_eq!(fs.read_raw_impl(&live, 0, 11).unwrap(), b"replacement");

    let reads = fs
        .vread_impl(&[
            ReadOp {
                file: stale.clone(),
                offset: VfOffset::At(0),
                length: 4,
            },
            ReadOp {
                file: live.clone(),
                offset: VfOffset::At(0),
                length: 11,
            },
        ])
        .unwrap();
    assert_eq!(
        reads[0].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(reads[1].as_ref().unwrap().data, b"replacement");
    let writes = fs
        .vwrite_impl(&[
            WriteOp::new(&stale, VfOffset::At(0), b"EVIL".as_slice()),
            WriteOp::new(&live, VfOffset::At(0), b"replacement".as_slice()),
        ])
        .unwrap();
    assert_eq!(
        writes[0].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(writes[1].is_ok());
    let mut attrs = [
        VfAttrs {
            file: stale.clone(),
            masks: AttrMask::SIZE,
            ..Default::default()
        },
        VfAttrs {
            file: live.clone(),
            masks: AttrMask::SIZE,
            ..Default::default()
        },
    ];
    let results = fs.vgetattrs_impl(&mut attrs).unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(results[1].is_ok());
    assert_eq!(attrs[1].size, 11);
    let results = fs
        .vsetattrs_raw_impl(&[
            VfAttrs {
                file: stale.clone(),
                masks: AttrMask::SIZE,
                size: 1,
                ..Default::default()
            },
            VfAttrs {
                file: live.clone(),
                masks: AttrMask::SIZE,
                size: 5,
                ..Default::default()
            },
        ])
        .unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(
        results[1].is_ok(),
        "a local stale owner must not stop a live sibling"
    );
    assert_eq!(fs.read_raw_impl(&live, 0, 11).unwrap(), b"repla");
    let results = fs
        .vfsync_impl(
            &[stale.clone(), live.clone()],
            vfsi_core::api::SyncMode::Data,
        )
        .unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert!(results[1].is_ok());
    assert!(
        fs.vclose_impl(&[stale, live])
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    fs.unlink_impl(&old_path).unwrap();
    fs.unlink_impl(&new_path).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_path_read_does_not_splice_a_replacement_after_losing_its_session() {
    use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};
    struct ReplaceAfterPrefix {
        path: PathBuf,
        connection: Connection,
        fired: std::sync::atomic::AtomicBool,
    }
    impl FaultInjector for ReplaceAfterPrefix {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()> {
            if *point == (OpenFaultPoint::BeforeReadChunk { chunk: 1 })
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let replacement = self.path.with_extension("replacement");
                std::fs::write(&replacement, vec![b'B'; 17 + 16 * 1024 * 1024]).unwrap();
                std::fs::rename(replacement, &self.path).unwrap();
                self.connection.mark_dead();
                return Err(VfError::transport(
                    None,
                    "session lost after confirmed read prefix",
                ));
            }
            Ok(())
        }
    }
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let backing =
        std::env::var("VFSI_SMB_LOCAL_ROOT").expect("live test needs fixture backing root");
    let path = format!("/vfsi-replaced-read-{}", std::process::id());
    let local = PathBuf::from(backing).join(path.trim_start_matches('/'));
    let chunk = fs.client.params().unwrap().max_read_size as usize;
    assert!(chunk > 0 && chunk <= 16 * 1024 * 1024);
    std::fs::write(&local, vec![b'A'; chunk + 17]).unwrap();
    let fault = Arc::new(ReplaceAfterPrefix {
        path: local.clone(),
        connection: fs.client.connection().clone(),
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    fs.set_fault_injector(fault.clone());
    let error = fs
        .read_one(&ReadOp::from_path(&path, VfOffset::At(0), chunk + 17))
        .expect_err("a replacement pathname cannot supply the original file's suffix");
    assert!(fault.fired.load(std::sync::atomic::Ordering::SeqCst));
    assert!(error.cause().is_transport());
    let Some(vfsi_core::IoProgress::Read(prefix)) = error.progress() else {
        panic!("original confirmed read prefix must remain available")
    };
    assert_eq!(prefix.offset(), 0);
    assert_eq!(prefix.data().unwrap(), &vec![b'A'; chunk]);
    assert!(!prefix.eof());
    assert_eq!(std::fs::read(&local).unwrap()[0], b'B');
    std::fs::remove_file(local).unwrap();
}

#[test]
fn live_local_metadata_rejections_are_unattempted_without_wire_io() {
    use std::os::unix::ffi::OsStrExt;
    let Some(mut fs) = sync_test_backend() else {
        return;
    };
    let (path, files) = sync_test_files(&mut fs, "metadata-preflight");
    fs.write_raw_impl(&files[0], 0, b"protected").unwrap();
    for operand in [
        FileOperand::from_os_path(Path::new(std::ffi::OsStr::from_bytes(b"/bad-\xff"))),
        FileOperand::Saved,
    ] {
        let before = fs.client.diagnostics().primary.metrics.requests_sent;
        let mut attrs = [VfAttrs {
            file: operand.clone(),
            masks: AttrMask::SIZE,
            ..Default::default()
        }];
        let queried = fs.vgetattrs_impl(&mut attrs).unwrap();
        assert_eq!(
            queried[0].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::NotAttempted
        );
        let changed = fs.vsetattrs_raw_impl(&attrs).unwrap();
        assert_eq!(
            changed[0].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::NotAttempted
        );
        assert_eq!(
            fs.client.diagnostics().primary.metrics.requests_sent,
            before
        );
        let mut mixed = [
            attrs[0].clone(),
            VfAttrs {
                file: files[0].clone(),
                masks: AttrMask::SIZE,
                ..Default::default()
            },
        ];
        let outcomes = fs.vgetattrs_impl(&mut mixed).unwrap();
        assert_eq!(
            outcomes[0].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::NotAttempted
        );
        assert!(outcomes[1].is_ok());
    }
    let before = fs.client.diagnostics().primary.metrics.requests_sent;
    let invalid = VfAttrs {
        file: files[0].clone(),
        masks: AttrMask::SIZE | AttrMask::MTIME,
        size: 0,
        mtime_nsec: 1_000_000_000,
        ..Default::default()
    };
    let outcomes = fs.vsetattrs_raw_impl(&[invalid]).unwrap();
    assert_eq!(
        outcomes[0].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::NotAttempted
    );
    assert_eq!(
        fs.client.diagnostics().primary.metrics.requests_sent,
        before
    );
    assert!(
        !fs.read_raw_impl(&files[0], 0, 32).unwrap().is_empty(),
        "invalid timestamp must not truncate"
    );
    for file in files {
        fs.close_impl(&file).unwrap();
    }
    fs.unlink_impl(&path).unwrap();
}
