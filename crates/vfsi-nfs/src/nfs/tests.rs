use super::*;

#[test]
fn write_wave_batches_only_disjoint_positional_ranges() {
    let mut wave = WriteWaveAccess::new(10..20, true);
    for range in [30..40, 0..10, 20..30, 40..u64::MAX] {
        assert!(wave.admit(range, true));
    }
    for range in [10..20, 5..11, 19..21, 35..36, 39..41] {
        assert!(!wave.admit(range, true));
    }
    // Empty ranges neither overlap nor erase an existing interval.
    for range in [10..10, 30..30, 0..0, u64::MAX..u64::MAX] {
        assert!(wave.admit(range, true));
    }
    assert!(!wave.admit(31..32, true));
    assert!(!wave.admit(0..1, false));
    let mut dependent = WriteWaveAccess::new(0..1, false);
    assert!(!dependent.admit(10..11, true));
    assert!(!dependent.admit(0..0, true));
    let mut empty = WriteWaveAccess::new(15..15, true);
    assert!(empty.admit(10..20, true));
    assert!(!empty.admit(15..16, true));
}

proptest::proptest! {
    #[test]
    fn write_wave_interval_index_matches_pairwise_oracle(
        first in (0u16..256, 0u16..64),
        requests in proptest::collection::vec((0u16..256, 0u16..64), 0..128),
    ) {
        let first = u64::from(first.0)..u64::from(first.0) + u64::from(first.1);
        let mut wave = WriteWaveAccess::new(first.clone(), true);
        let mut accepted = vec![first];
        for (start, length) in requests {
            let range = u64::from(start)..u64::from(start) + u64::from(length);
            let independent = range.is_empty() || accepted.iter().all(|prior| {
                prior.is_empty() || range.start >= prior.end || prior.start >= range.end
            });
            proptest::prop_assert_eq!(wave.admit(range.clone(), true), independent);
            if independent {
                accepted.push(range);
            }
        }
    }
}

#[test]
fn read_only_mounts_reject_all_mutating_open_flags() {
    assert!(!open_flags_mutate(libc::O_RDONLY | libc::O_CLOEXEC));
    for flags in [
        libc::O_WRONLY,
        libc::O_RDWR,
        libc::O_CREAT,
        libc::O_TRUNC,
        libc::O_APPEND,
    ] {
        assert!(open_flags_mutate(flags));
    }
    assert!(check_mount_write(true, 0).is_ok());
    assert_eq!(
        check_mount_write(true, 1).unwrap_err().err_no(),
        libc::EROFS as u32
    );
    assert!(check_mount_write(false, 1).is_ok());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_read_only_mount_retains_restriction_after_native_reconnect() {
    let mount = std::env::var("VFSI_NFS_TEST_MOUNT_RO")
        .expect("VFSI_NFS_TEST_MOUNT_RO is required for this ignored integration test");
    let rw_mount = std::env::var("VFSI_NFS_TEST_MOUNT")
        .expect("read-only test needs a writable fixture mount");
    let name = format!(".vfsi-reconnect-read-only-{}", std::process::id());
    let fixture = Path::new(&rw_mount).join(&name);
    std::fs::create_dir(&fixture).unwrap();
    let mut backend = NfsClientBuilder::from_mount(Path::new(&mount).join(&name))
        .unwrap()
        .connect()
        .unwrap();
    assert!(backend.read_only);
    backend.reconnect().unwrap();
    assert!(backend.read_only);
    assert!(backend.mount_source.is_some());
    assert_eq!(
        backend
            .open_path_impl(
                VfPathBase::Abs,
                Path::new("/.mount-forbidden"),
                libc::O_CREAT | libc::O_WRONLY,
                0o600
            )
            .unwrap_err()
            .err_no(),
        libc::EROFS as u32
    );
    drop(backend);
    std::fs::remove_dir(fixture).unwrap();
}

#[test]
fn removal_scan_continues_cookies_and_verifies_changed_passes_once() {
    // These are transition-helper assertions, not a simulation of the
    // production remover. A continuation must never restart mid-pass.
    for cookie in [1, 2, 64, u64::MAX] {
        for changed in [false, true] {
            assert_eq!(next_rm_scan(cookie, changed), Some((cookie, changed)));
        }
    }
    assert_eq!(next_rm_scan(0, true), Some((0, false)));
    assert_eq!(next_rm_scan(0, false), None);
}

#[test]
fn abs_path_is_namespace_relative_and_server_path_prepends_the_export_root() {
    let root = Path::new("/export/data");
    // Absolute application paths are relative to the export namespace,
    // not the server path (the bug fixed here included the export root).
    assert_eq!(
        namespace_path(Path::new(""), Path::new("/foo")),
        PathBuf::from("foo")
    );
    assert_eq!(
        server_path_for(root, Path::new(""), Path::new("/foo")),
        PathBuf::from("/export/data/foo")
    );
    // Relative paths resolve against the namespace-relative cwd.
    assert_eq!(
        namespace_path(Path::new("sub"), Path::new("f")),
        PathBuf::from("sub/f")
    );
    assert_eq!(
        server_path_for(root, Path::new("sub"), Path::new("f")),
        PathBuf::from("/export/data/sub/f")
    );
    // The application root itself is empty relative to the namespace and
    // maps to the export root when resolved on the server.
    assert_eq!(
        namespace_path(Path::new(""), Path::new("/")),
        PathBuf::new()
    );
    assert_eq!(
        server_path_for(root, Path::new(""), Path::new("/")),
        PathBuf::from("/export/data")
    );
    // `..` cannot escape the namespace root.
    assert_eq!(
        namespace_path(Path::new(""), Path::new("../../etc")),
        PathBuf::from("etc")
    );
}

#[test]
fn malformed_type_attribute_is_an_error_not_a_regular_type() {
    // An empty or truncated FATTR4_TYPE payload must not be silently
    // treated as "not a symlink" (which would skip following a link).
    assert!(type_is_symlink(&[]).is_err());
    assert!(type_is_symlink(&[0, 0, 0]).is_err());

    assert!(type_is_symlink(&nfs_ftype4_NF4LNK.to_be_bytes()).unwrap());
    assert!(!type_is_symlink(&nfs_ftype4_NF4REG.to_be_bytes()).unwrap());
}

#[test]
fn descriptor_chunk_errors_report_the_original_vector_owner() {
    // The first caller was split into two wire chunks; chunk 1 still
    // belongs to caller 0, rather than caller 1.
    let owners = [0, 0, 1];
    let error = remap_descriptor_chunk_error(VfError::failure(1, ERR_EBADF), 0, &owners);
    assert_eq!(error.index(), Some(0));
}

#[test]
fn read_all_round_remaps_failures_and_rejects_zero_progress() {
    let active = [2, 5];
    let remapped = remap_active_error(VfError::failure(0, ERR_IO), &active);
    assert_eq!(remapped.index(), Some(2));
    let remapped = remap_active_error(VfError::failure(1, ERR_IO), &active);
    assert_eq!(remapped.index(), Some(5));

    let mut out = vec![Vec::new(); 6];
    let mut offsets = vec![0; 6];
    let stalled = [ReadResult {
        file: VfFile::from_path("/f"),
        offset: 7,
        data: Vec::new(),
        eof: false,
    }];
    let error = merge_read_allv_round(&[5], &stalled, &mut out, &mut offsets, &mut 0, usize::MAX)
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(5));
}

#[test]
fn read_all_round_preserves_original_positions_after_cohort_shrinks() {
    let mut out = vec![Vec::new(); 3];
    let mut offsets = vec![0; 3];
    let results = [
        ReadResult {
            file: VfFile::from_path("/one"),
            offset: 0,
            data: b"a".to_vec(),
            eof: true,
        },
        ReadResult {
            file: VfFile::from_path("/two"),
            offset: 4,
            data: b"bc".to_vec(),
            eof: false,
        },
    ];
    let next = merge_read_allv_round(
        &[1, 2],
        &results,
        &mut out,
        &mut offsets,
        &mut 0,
        usize::MAX,
    )
    .unwrap();
    assert_eq!(next, [2]);
    assert_eq!(out[1], b"a");
    assert_eq!(out[2], b"bc");
    assert_eq!(offsets[2], 6);
}

#[test]
fn read_all_batches_never_request_beyond_the_remaining_allocation_budget() {
    let active = [0, 1, 2, 3];
    let (cohort, window) = bounded_read_allv_batch(&active, 3, 1 << 20, 1 << 20);
    assert_eq!(cohort, [0, 1, 2]);
    assert!(cohort.len() * window <= 3);

    let (cohort, window) = bounded_read_allv_batch(&active, 0, 1 << 20, 1 << 20);
    assert_eq!(cohort, [0]);
    assert_eq!(window, 1);
}

#[test]
fn walk_page_decoder_stops_at_entry_and_path_budgets() {
    let page = [
        crate::client::DirEntry {
            name: b"one".to_vec(),
            cookie: 1,
            attrs: Vec::new(),
        },
        crate::client::DirEntry {
            name: b"two".to_vec(),
            cookie: 0,
            attrs: Vec::new(),
        },
    ];
    let mut count = 0;
    let options = ListDirOptions::new().recursive(true).max_entries(1);
    let mut budget =
        vfsi_core::internal::TraversalBudget::new(options.entry_limit(), options.path_byte_limit());
    let mut output = Vec::new();
    let error = append_bounded_walk_page(
        Path::new("/root"),
        Path::new("/root"),
        AttrMask::empty(),
        &[],
        &page,
        options,
        &mut count,
        &mut budget,
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(count, 1);
    assert_eq!(output.len(), 1);

    count = 0;
    let options = ListDirOptions::new().recursive(true).max_path_bytes(1);
    budget =
        vfsi_core::internal::TraversalBudget::new(options.entry_limit(), options.path_byte_limit());
    output.clear();
    let error = append_bounded_walk_page(
        Path::new("/root"),
        Path::new("/root"),
        AttrMask::empty(),
        &[],
        &page[..1],
        options,
        &mut count,
        &mut budget,
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(count, 0);
    assert!(output.is_empty());
}

#[test]
fn malformed_readdir_attributes_are_transport_errors() {
    let error = parse_attr_list(&[FATTR4_SIZE], &[0, 0, 0]).unwrap_err();
    assert!(error.is_transport());

    // XDR strings occupy a four-byte-aligned field. A payload without
    // its required padding must not be accepted at the end of an attrlist.
    let mut unpadded = 1u32.to_be_bytes().to_vec();
    unpadded.push(b'x');
    let error = parse_attr_list(&[FATTR4_OWNER], &unpadded).unwrap_err();
    assert!(error.is_transport());

    let mut trailing = 7u64.to_be_bytes().to_vec();
    trailing.push(0);
    let error = parse_attr_list(&[FATTR4_SIZE], &trailing).unwrap_err();
    assert!(error.is_transport());
}

#[test]
fn recovery_only_retries_transport_and_recoverable_session_statuses() {
    assert!(NfsVecFs::needs_recovery(&VfError::transport(None, "reset")));
    for status in [
        nfsstat4_NFS4ERR_EXPIRED,
        nfsstat4_NFS4ERR_GRACE,
        nfsstat4_NFS4ERR_STALE_CLIENTID,
        nfsstat4_NFS4ERR_STALE_STATEID,
        nfsstat4_NFS4ERR_BAD_STATEID,
        nfsstat4_NFS4ERR_BADSESSION,
        nfsstat4_NFS4ERR_DEADSESSION,
    ] {
        assert!(NfsVecFs::needs_recovery(&VfError::failure(0, status)));
    }
    assert!(!NfsVecFs::needs_recovery(&VfError::failure(
        0,
        nfsstat4_NFS4ERR_NOENT,
    )));
}

#[test]
fn recovery_reopen_flags_cannot_repeat_creation_or_truncation() {
    let original = libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC;
    let reopened = non_destructive_reopen_flags(original);
    assert_eq!(reopened & (libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC), 0);
    assert_ne!(reopened & libc::O_APPEND, 0);
    assert_eq!(reopened & libc::O_ACCMODE, libc::O_RDWR);
}

#[test]
fn default_recovery_window_can_span_a_conventional_server_grace_period() {
    let policy = NfsRecoveryPolicy::default();
    assert!(policy.reconnect_attempts > 1);
    assert!(policy.max_elapsed >= Duration::from_secs(90));
    assert!(policy.initial_backoff <= policy.max_backoff);
}

#[test]
fn connection_options_preserve_auth_sys_as_the_compatible_default() {
    let options = NfsConnectOptions::default();
    assert_eq!(options.minorversion, None);
    assert_eq!(options.connect_timeout, Duration::from_secs(10));
    assert_eq!(options.request_timeout, Duration::from_secs(5));
    assert_eq!(options.authentication, NfsAuthentication::AuthSys);
    assert_eq!(options.root, Path::new("/"));
    assert!(options.reconnects_automatically());
    assert_eq!(options.max_compound_bytes, None);
}

#[test]
fn builder_collects_connection_and_runtime_configuration() {
    let policy = NfsRecoveryPolicy::new().reconnect_attempts(3);
    let builder = NfsClientBuilder::new("server:2049")
        .root("/export/app")
        .minor_version(Some(1))
        .client_owner(b"production-client-17".to_vec())
        .request_timeout(Duration::from_secs(7))
        .recovery_policy(policy)
        .auto_reconnect(false)
        .max_compound_bytes(64 * 1024);
    assert_eq!(builder.host, "server:2049");
    assert_eq!(builder.options.root, Path::new("/export/app"));
    assert_eq!(builder.options.minorversion, Some(1));
    assert_eq!(
        builder.options.client_owner.as_deref(),
        Some(b"production-client-17".as_slice())
    );
    assert_eq!(builder.options.request_timeout, Duration::from_secs(7));
    assert_eq!(builder.options.recovery_policy, policy);
    assert!(!builder.options.reconnects_automatically());
    assert_eq!(
        builder.options.max_compound_bytes,
        std::num::NonZeroUsize::new(64 * 1024)
    );
}

#[test]
fn invalid_client_owner_fails_before_network_io() {
    for owner in [Vec::new(), vec![b'x'; 1025]] {
        let error = NfsClientBuilder::new("unreachable.invalid")
            .client_owner(owner)
            .connect()
            .err()
            .expect("invalid client owner must be rejected");
        assert_eq!(error.domain(), ErrorDomain::Client);
        assert_eq!(error.err_no(), ERR_INVAL);
        assert_eq!(error.operation(), Some("connect"));
    }
}

#[test]
fn client_pool_validates_size_and_gives_explicit_owners_distinct_ids() {
    for size in [0, 65] {
        let error = NfsClientBuilder::new("unreachable.invalid")
            .connect_pool(size)
            .err()
            .expect("invalid pool size must be rejected before network I/O");
        assert_eq!(error.domain(), ErrorDomain::Client);
        assert_eq!(error.err_no(), ERR_INVAL);
        assert_eq!(error.operation(), Some("connect_pool"));
    }
    let builder = NfsClientBuilder::new("unused").client_owner(vec![b'x'; 1024]);
    let first = builder.pool_member(1, 0).options.client_owner.unwrap();
    let second = builder.pool_member(1, 1).options.client_owner.unwrap();
    let next_pool = builder.pool_member(2, 0).options.client_owner.unwrap();
    assert_ne!(first, second);
    assert_ne!(first, next_pool);
    assert!(first.len() <= 1024);
}
