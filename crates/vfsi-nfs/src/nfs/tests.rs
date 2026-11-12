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
        check_mount_write(true, 1).unwrap_err().raw_code(),
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
            .raw_code(),
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
        let cookie = DirCookie {
            value: cookie,
            verifier: [7; 8],
        };
        for changed in [false, true] {
            assert_eq!(next_rm_scan(cookie, changed), Some((cookie, changed)));
        }
    }
    let eof = DirCookie {
        value: 0,
        verifier: [7; 8],
    };
    assert_eq!(next_rm_scan(eof, true), Some((DirCookie::default(), false)));
    assert_eq!(next_rm_scan(eof, false), None);
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
    let error = remap_descriptor_chunk_error(VfError::errno(1, ERR_EBADF), 0, &owners);
    assert_eq!(error.index(), Some(0));
}

#[test]
fn active_round_errors_remap_to_original_inputs() {
    let active = [2, 5];
    assert_eq!(
        remap_active_error(VfError::errno(0, ERR_IO), &active).index(),
        Some(2)
    );
    assert_eq!(
        remap_active_error(VfError::errno(1, ERR_IO), &active).index(),
        Some(5)
    );
    // Zero-progress and prefix retention now exercise the real shared driver
    // in vfsi-sync's whole_file_zero_progress_keeps_original_slot regression.
}

#[test]
fn active_round_errors_preserve_positions_after_cohort_shrinks() {
    let active = [1, 2];
    assert_eq!(
        remap_active_error(VfError::errno(1, ERR_IO), &active).index(),
        Some(2)
    );
    assert_eq!(
        remap_active_error(VfError::transport(None, "lost reply"), &active).index(),
        None
    );
    // Actual read data/offset scheduling is covered by the shared driver's
    // whole_file_shrinking_cohorts_keep_data_and_original_positions test.
}

#[test]
fn read_all_batches_never_request_beyond_the_remaining_allocation_budget() {
    let active = [0, 1, 2, 3];
    let (cohort, window) = bounded_read_allv_batch(active.len(), 3, 1 << 20, 1 << 20);
    assert_eq!(cohort, 3);
    assert!(cohort * window <= 3);

    let (cohort, window) = bounded_read_allv_batch(active.len(), 0, 1 << 20, 1 << 20);
    assert_eq!(cohort, 1);
    assert_eq!(window, 1);
}

#[test]
fn walk_page_decoder_stops_at_entry_and_path_budgets() {
    let page = [
        crate::client::DirEntry {
            name: b"one".to_vec(),
            cookie: 1,
            attrmask: bitmap4 {
                bitmap4_len: 1,
                map: [1 << FATTR4_TYPE, 0, 0],
            },
            attrs: nfs_ftype4_NF4REG.to_be_bytes().to_vec(),
        },
        crate::client::DirEntry {
            name: b"two".to_vec(),
            cookie: 0,
            attrmask: bitmap4 {
                bitmap4_len: 1,
                map: [1 << FATTR4_TYPE, 0, 0],
            },
            attrs: nfs_ftype4_NF4REG.to_be_bytes().to_vec(),
        },
    ];
    let mut count = 0;
    let options = ListDirOptions::new()
        .with_recursive(true)
        .with_max_entries(1);
    let mut budget = vfsi_core::internal::TraversalBudget::new(
        options.max_entries_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
        options.max_path_bytes_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
    );
    let mut output = Vec::new();
    let error = append_bounded_walk_page(
        Path::new("/root"),
        Path::new("/root"),
        AttrMask::empty(),
        &page,
        options,
        &mut count,
        &mut budget,
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    assert_eq!(count, 1);
    assert_eq!(output.len(), 1);

    count = 0;
    let options = ListDirOptions::new()
        .with_recursive(true)
        .with_max_path_bytes(1);
    budget = vfsi_core::internal::TraversalBudget::new(
        options.max_entries_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_ENTRIES),
        options.max_path_bytes_or(vfsi_core::api::DEFAULT_DIRECTORY_MAX_PATH_BYTES),
    );
    output.clear();
    let error = append_bounded_walk_page(
        Path::new("/root"),
        Path::new("/root"),
        AttrMask::empty(),
        &page[..1],
        options,
        &mut count,
        &mut budget,
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
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
    for kind in [
        vfsi_core::TransportKind::Auth,
        vfsi_core::TransportKind::InvalidReply,
    ] {
        assert!(!NfsVecFs::needs_recovery(&VfError::transport_with_kind(
            None, kind, "terminal"
        )));
    }
    for status in [
        nfsstat4_NFS4ERR_EXPIRED,
        nfsstat4_NFS4ERR_GRACE,
        nfsstat4_NFS4ERR_STALE_CLIENTID,
        nfsstat4_NFS4ERR_STALE_STATEID,
        nfsstat4_NFS4ERR_BAD_STATEID,
        nfsstat4_NFS4ERR_BADSESSION,
        nfsstat4_NFS4ERR_DEADSESSION,
    ] {
        assert!(NfsVecFs::needs_recovery(&VfError::nfs(0, status)));
        assert!(!NfsVecFs::needs_recovery(&VfError::errno(0, status)));
    }
    assert!(!NfsVecFs::needs_recovery(&VfError::errno(
        0,
        nfsstat4_NFS4ERR_NOENT,
    )));
    let moved = VfError::nfs(3, nfsstat4_NFS4ERR_MOVED);
    assert_eq!(moved.kind(), std::io::ErrorKind::Unsupported);
    assert!(!NfsVecFs::needs_recovery(&moved));
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
    assert_eq!(options.authentication, NfsAuth::AuthSys);
    assert_eq!(options.root, Path::new("/"));
    assert!(options.auto_reconnect());
    assert_eq!(options.max_compound_bytes, None);
}

#[test]
fn fixed_text_builder_owns_compact_endpoints_and_preserves_input_compatibility() {
    use std::mem::{size_of, size_of_val};

    let mut source = String::with_capacity(256);
    source.push_str("[2001:db8::7]:2049");
    let expected = source.clone();
    let builders = [
        NfsClientBuilder::new(source.clone()),
        NfsClientBuilder::new(&source),
        NfsClientBuilder::new(source.as_str()),
        NfsClientBuilder::new(source.clone().into_boxed_str()),
    ];
    source.clear();
    for builder in builders {
        assert_eq!(builder.host.as_ref(), expected);
        assert_eq!(size_of_val(&builder.host), size_of::<Box<str>>());
        assert_eq!(
            size_of::<String>() - size_of_val(&builder.host),
            size_of::<usize>()
        );
        let cloned = builder.clone();
        assert_eq!(cloned.host, builder.host);
        assert_ne!(cloned.host.as_ptr(), builder.host.as_ptr());
        drop(builder);
        assert_eq!(cloned.host.as_ref(), expected);
    }
    // Validation errors retain the endpoint as their public path context.
    let error = NfsClientBuilder::new(expected.clone())
        .connect_pool(0)
        .err()
        .unwrap();
    assert_eq!(error.path(), Some(Path::new(&expected)));
}

#[test]
fn builder_collects_connection_and_runtime_configuration() {
    let policy = NfsRecoveryPolicy::new()
        .with_reconnect_attempts(std::num::NonZeroUsize::new(3).expect("nonzero tuning value"));
    let builder = NfsClientBuilder::new("server:2049")
        .root("/export/app")
        .minor_version(Some(1))
        .client_owner(b"production-client-17".to_vec())
        .request_timeout(Duration::from_secs(7))
        .recovery_policy(policy)
        .auto_reconnect(false)
        .max_compound_bytes(64 * 1024);
    assert_eq!(builder.host.as_ref(), "server:2049");
    assert_eq!(builder.options.root, Path::new("/export/app"));
    assert_eq!(builder.options.minorversion, Some(1));
    assert_eq!(
        builder.options.client_owner.as_deref(),
        Some(b"production-client-17".as_slice())
    );
    assert_eq!(builder.options.request_timeout, Duration::from_secs(7));
    assert_eq!(builder.options.recovery_policy, policy);
    assert!(!builder.options.auto_reconnect());
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
        assert_eq!(error.raw_code(), ERR_INVAL);
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
        assert_eq!(error.raw_code(), ERR_INVAL);
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

#[test]
fn fs_id_and_fileid_decode_as_one_fs_obj_id() {
    let major = 0x0102_0304_0506_0708u64;
    let minor = 0x1112_1314_1516_1718u64;
    let fileid = 0x2122_2324_2526_2728u64;
    let mut wire = Vec::new();
    wire.extend_from_slice(&major.to_be_bytes());
    wire.extend_from_slice(&minor.to_be_bytes());
    wire.extend_from_slice(&fileid.to_be_bytes());

    let ids = [FATTR4_FSID, FATTR4_FILEID];
    assert_eq!(
        request_mask_to_attr_list(&AttrMask::identity()),
        [FATTR4_TYPE, FATTR4_FSID, FATTR4_FILEID]
    );
    assert_eq!(
        request_mask_to_attr_list(&(AttrMask::identity() | AttrMask::NAMED_ATTR)),
        [FATTR4_TYPE, FATTR4_NAMED_ATTR, FATTR4_FSID, FATTR4_FILEID]
    );
    assert!(
        crate::client::READDIR_ATTRS
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    );
    let values = parse_attr_list(&ids, &wire).unwrap();
    let mut attrs = VfAttrs {
        masks: AttrMask::identity(),
        ..VfAttrs::default()
    };
    apply_attrs(&mut attrs, &values);
    assert_eq!(attrs.returned, AttrMask::identity());
    let attrs = vfsi_core::metadata_from_attrs(attrs);
    let id = attrs.fs_obj_id().unwrap();
    assert_eq!(id.fs_id(), (u128::from(major) << 64) | u128::from(minor));
    assert_eq!(id.file_id(), fileid);
}

#[test]
fn allocation_read_assembly_transfers_payloads_and_preserves_result_order() {
    let reads = [
        ReadOp::at(FileOperand::from_path("/first"), 10, 4),
        ReadOp::at(FileOperand::from_path("/missing"), 20, 4),
        ReadOp::at(FileOperand::from_fd(42), 30, 4),
    ];
    let mut first = Vec::with_capacity(32);
    first.extend_from_slice(b"a\x00\xffb");
    let first_pointer = first.as_ptr();
    let first_capacity = first.capacity();
    let third = b"tail".to_vec();
    let third_pointer = third.as_ptr();
    let mut payloads = [Some(first), None, Some(third)];
    let output = NfsVecFs::assemble_reads(
        &reads,
        [10, 20, 30],
        payloads
            .iter_mut()
            .enumerate()
            .map(|(index, payload)| match payload.take() {
                Some(data) => Ok((data, index == 2)),
                None => Err(vfsi_core::OpError::failed(
                    VfError::errno(None, ERR_NOENT),
                    None,
                )),
            }),
    );
    assert_eq!(output.len(), reads.len());
    assert!(payloads.iter().all(Option::is_none));
    assert_eq!(output[0].as_ref().unwrap().data.as_ptr(), first_pointer);
    assert_eq!(output[0].as_ref().unwrap().data.capacity(), first_capacity);
    assert_eq!(output[0].as_ref().unwrap().data, b"a\x00\xffb");
    assert_eq!(
        output[1].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::Failed
    );
    assert_eq!(output[2].as_ref().unwrap().data.as_ptr(), third_pointer);
    assert_eq!(output[2].as_ref().unwrap().data, b"tail");
    for (index, result) in output
        .iter()
        .enumerate()
        .filter_map(|(index, result)| result.as_ref().ok().map(|result| (index, result)))
    {
        assert_eq!(result.file, reads[index].file);
        assert_eq!(result.offset, [10, 20, 30][index]);
        assert_eq!(result.eof, index == 2);
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires VFSI_NFS_TEST_MOUNT pointing to a direct NFS mount"]
fn live_reconnect_uses_resolved_mount_after_discovery_path_disappears() {
    let mount = std::env::var("VFSI_NFS_TEST_MOUNT").expect("mount fixture is required");
    let mut backend = NfsClientBuilder::from_mount(&mount)
        .unwrap()
        .connect()
        .unwrap();
    backend
        .mount_source
        .as_mut()
        .unwrap()
        .forget_discovery_path_for_test();
    NfsClientBuilder::from_mount_config(backend.resolved_mount().unwrap())
        .unwrap()
        .connect()
        .unwrap();
    // The original discovery route is deliberately unusable. Its remote root
    // and authentication remain pinned; reconnect must not revisit that route.
    backend.read_only = true;
    backend.reconnect().unwrap();
    assert!(backend.read_only);
    assert_eq!(
        backend
            .open_path_impl(VfPathBase::Abs, Path::new("forbidden"), libc::O_WRONLY, 0)
            .unwrap_err()
            .raw_code(),
        libc::EROFS as u32
    );
}

#[test]
fn distinct_nfs_filehandles_can_group_as_the_same_filesystem_object() {
    use super::handle_backend::object_ref_for_group;
    let id = FsObjId::new(1, 7);
    assert_eq!(
        object_ref_for_group(&[1], Some(id), false),
        object_ref_for_group(&[2], Some(id), false)
    );
    assert_ne!(
        object_ref_for_group(&[1], Some(id), false),
        object_ref_for_group(&[2], Some(FsObjId::new(2, 7)), false)
    );
    assert_eq!(object_ref_for_group(&[1], None, false), None);
}

#[test]
fn path_setattrs_resolve_symlink_policy_before_mutation() {
    assert!(nofollow_symlink_setattr_is_unsupported(
        false,
        nfs_ftype4_NF4LNK,
        AttrMask::MODE
    ));
    assert!(nofollow_symlink_setattr_is_unsupported(
        false,
        nfs_ftype4_NF4LNK,
        AttrMask::SIZE | AttrMask::MTIME
    ));
    assert!(!nofollow_symlink_setattr_is_unsupported(
        false,
        nfs_ftype4_NF4LNK,
        AttrMask::ATIME | AttrMask::MTIME
    ));
    assert!(!nofollow_symlink_setattr_is_unsupported(
        true,
        nfs_ftype4_NF4LNK,
        AttrMask::MODE
    ));
    assert!(!nofollow_symlink_setattr_is_unsupported(
        false,
        nfs_ftype4_NF4REG,
        AttrMask::MODE
    ));

    let path_mode = [VfAttrs {
        file: FileOperand::from_path("/link"),
        masks: AttrMask::MODE,
        ..VfAttrs::default()
    }];
    assert!(path_setattrs_require_type_resolution(&path_mode, false));
    assert!(path_setattrs_require_type_resolution(&path_mode, true));

    let path_mtime = [VfAttrs {
        file: FileOperand::from_path("/link"),
        masks: AttrMask::MTIME,
        ..VfAttrs::default()
    }];
    assert!(!path_setattrs_require_type_resolution(&path_mtime, false));
    assert!(path_setattrs_require_type_resolution(&path_mtime, true));

    let handle = [VfAttrs {
        file: FileOperand::from_fd(7),
        masks: AttrMask::MODE,
        ..VfAttrs::default()
    }];
    assert!(!path_setattrs_require_type_resolution(&handle, false));

    let no_updates = [VfAttrs {
        file: FileOperand::from_path("/empty"),
        ..VfAttrs::default()
    }];
    assert!(!path_setattrs_require_type_resolution(&no_updates, false));
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "requires VFSI_NFS_TEST_MOUNT pointing to a writable direct NFS mount"]
fn live_nofollow_setattr_respects_symlink_semantics() {
    use std::os::unix::fs::symlink;

    fn snapshot(backend: &mut NfsVecFs, path: &str, follow: bool) -> VfAttrs {
        let mut attrs = VfAttrs {
            file: FileOperand::from_path(path),
            masks: AttrMask::MODE | AttrMask::MTIME,
            ..VfAttrs::default()
        };
        if follow {
            backend.vgetattrs_impl(std::slice::from_mut(&mut attrs))
        } else {
            backend.vgetattrs_nofollow_impl(std::slice::from_mut(&mut attrs))
        }
        .unwrap();
        assert!(attrs.returned.contains(attrs.masks));
        attrs
    }

    let mount = std::env::var("VFSI_NFS_TEST_MOUNT").expect("mount fixture is required");
    let name = format!(".vfsi-nofollow-setattr-{}", std::process::id());
    let fixture = Path::new(&mount).join(&name);
    std::fs::create_dir(&fixture).unwrap();
    std::fs::write(fixture.join("target"), b"target").unwrap();
    symlink("target", fixture.join("link")).unwrap();
    let mut backend = NfsClientBuilder::from_mount(&fixture)
        .unwrap()
        .connect()
        .unwrap();
    let before_link = snapshot(&mut backend, "/link", false);
    let before_target = snapshot(&mut backend, "/target", true);
    let mode_update = VfAttrs {
        file: FileOperand::from_path("/link"),
        masks: AttrMask::MODE,
        mode: 0o600,
        ..VfAttrs::default()
    };
    let error = {
        let batch_updates = std::slice::from_ref(&mode_update);
        vfsi_core::internal::collect_vec_result(
            backend.vsetattrs_raw_nofollow_impl(batch_updates),
            batch_updates.len(),
        )
        .map(|_| ())
    }
    .unwrap_err();
    assert_eq!(error.raw_code(), VF_ERR_UNSUPPORTED);
    let after_mode_link = snapshot(&mut backend, "/link", false);
    let after_mode_target = snapshot(&mut backend, "/target", true);
    assert_eq!(after_mode_link.mode, before_link.mode);
    assert_eq!(after_mode_target.mode, before_target.mode);

    // NFS can set symlink timestamps directly, so no-follow updates apply to
    // the link and leave the target untouched.
    let time_update = VfAttrs {
        file: FileOperand::from_path("/link"),
        masks: AttrMask::MTIME,
        mtime_sec: 1_700_000_003,
        mtime_nsec: 123_456_789,
        ..VfAttrs::default()
    };
    {
        let batch_updates = std::slice::from_ref(&time_update);
        vfsi_core::internal::collect_vec_result(
            backend.vsetattrs_raw_nofollow_impl(batch_updates),
            batch_updates.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    let after_link = snapshot(&mut backend, "/link", false);
    let after_target = snapshot(&mut backend, "/target", true);
    assert_eq!(
        (after_link.mtime_sec, after_link.mtime_nsec),
        (time_update.mtime_sec, time_update.mtime_nsec)
    );
    assert_eq!(
        (after_target.mtime_sec, after_target.mtime_nsec),
        (before_target.mtime_sec, before_target.mtime_nsec)
    );

    // The MODE update uses an in-compound NVERIFY guard, so it can stay in
    // the vector without forcing the independent no-follow timestamp update
    // onto a phased path.
    let target_mode_update = VfAttrs {
        file: FileOperand::from_path("/target"),
        masks: AttrMask::MODE,
        mode: 0o640,
        ..VfAttrs::default()
    };
    let link_time_update = VfAttrs {
        file: FileOperand::from_path("/link"),
        masks: AttrMask::MTIME,
        mtime_sec: 1_700_000_007,
        mtime_nsec: 987_654_321,
        ..VfAttrs::default()
    };
    {
        let batch_updates = &[target_mode_update.clone(), link_time_update.clone()];
        vfsi_core::internal::collect_vec_result(
            backend.vsetattrs_raw_nofollow_impl(batch_updates),
            batch_updates.len(),
        )
        .map(|_| ())
    }
    .unwrap();
    let after_mixed_link = snapshot(&mut backend, "/link", false);
    let after_mixed_target = snapshot(&mut backend, "/target", true);
    assert_eq!(after_mixed_target.mode & 0o777, target_mode_update.mode);
    assert_eq!(
        (after_mixed_link.mtime_sec, after_mixed_link.mtime_nsec),
        (link_time_update.mtime_sec, link_time_update.mtime_nsec)
    );
    assert_eq!(
        (after_mixed_target.mtime_sec, after_mixed_target.mtime_nsec),
        (before_target.mtime_sec, before_target.mtime_nsec)
    );
    drop(backend);
    std::fs::remove_file(fixture.join("link")).unwrap();
    std::fs::remove_file(fixture.join("target")).unwrap();
    std::fs::remove_dir(fixture).unwrap();
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "requires VFSI_NFS_TEST_MOUNT pointing to a writable direct NFS mount"]
fn live_guarded_setattr_handles_symlink_cohorts_on_a_bounded_stack() {
    use std::os::unix::fs::symlink;

    let mount = std::env::var("VFSI_NFS_TEST_MOUNT").expect("mount fixture is required");
    let fixture = Path::new(&mount).join(format!(".vfsi-setattr-cohort-{}", std::process::id()));
    std::fs::create_dir(&fixture).unwrap();
    // Own just this test's newly created directory, including on assertion failure.
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let cleanup = Cleanup(fixture.clone());
    for name in ["target", "tail"] {
        std::fs::write(fixture.join(name), b"keep").unwrap();
    }
    for index in 0..512 {
        symlink("target", fixture.join(format!("link-{index}"))).unwrap();
    }
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut backend = NfsClientBuilder::from_mount(&fixture)
                .unwrap()
                .connect()
                .unwrap();
            let update = |path: String, mode| VfAttrs {
                file: FileOperand::from_path(&path),
                masks: AttrMask::MODE,
                mode,
                ..VfAttrs::default()
            };
            let mut updates = vec![update("/target".into(), 0o640)];
            updates.extend((0..512).map(|index| update(format!("/link-{index}"), 0o600)));
            updates.push(update("/tail".into(), 0o640));
            {
                let batch_updates = &updates;
                vfsi_core::internal::collect_vec_result(
                    backend.vsetattrs_raw_impl(batch_updates),
                    batch_updates.len(),
                )
                .map(|_| ())
            }
            .unwrap();
            assert_eq!(
                backend.stat_impl(Path::new("/target")).unwrap().mode & 0o777,
                0o600
            );
            assert_eq!(
                backend.stat_impl(Path::new("/tail")).unwrap().mode & 0o777,
                0o640
            );

            let missing_index = updates.len();
            updates.push(update("/missing".into(), 0o600));
            updates.push(update("/tail".into(), 0o666));
            let error = {
                let batch_updates = &updates;
                vfsi_core::internal::collect_vec_result(
                    backend.vsetattrs_raw_impl(batch_updates),
                    batch_updates.len(),
                )
                .map(|_| ())
            }
            .unwrap_err();
            assert_eq!(error.index(), Some(missing_index));
            assert_eq!(error.raw_code(), ERR_NOENT);
            assert_eq!(
                backend.stat_impl(Path::new("/tail")).unwrap().mode & 0o777,
                0o640,
                "do not execute the suffix after a later failure"
            );

            // The optimized guard path must not allocate a suffix-sized type vector
            // that its caller never consumes.
            let operation = crate::client::PathSetattrOp {
                file: crate::client::FileRef::Path(
                    vfsi_core::path::path_bytes(&backend.server_path(Path::new("/tail"))).to_vec(),
                ),
                mode: Some(0o640),
                uid: None,
                gid: None,
                size: None,
                atime: None,
                mtime: None,
                check_type: false,
            };
            let outcome = backend
                .nfs
                .setattr_path_compound_guarded(&[operation], &[false])
                .unwrap();
            assert!(outcome.1.is_none());
            assert_eq!(outcome.0, [Ok(None)]);
        })
        .unwrap()
        .join()
        .unwrap();
    drop(cleanup);
}

#[test]
fn recursive_readdir_rejects_a_stalled_continuation_cookie() {
    let mut seen = std::collections::HashSet::new();
    assert!(validate_readdir_progress(0, 7, &mut seen, None).is_ok());
    assert!(validate_readdir_progress(7, 11, &mut seen, None).is_ok());
    assert!(validate_readdir_progress(11, 0, &mut seen, None).is_ok());

    let mut seen = std::collections::HashSet::new();
    assert!(validate_readdir_progress(0, 7, &mut seen, None).is_ok());
    let error = validate_readdir_progress(7, 7, &mut seen, None).unwrap_err();
    assert!(error.is_transport());
}

#[test]
fn recursive_readdir_rejects_nonadjacent_cookie_cycles() {
    let mut seen = std::collections::HashSet::new();
    assert!(validate_readdir_progress(0, 7, &mut seen, None).is_ok());
    assert!(validate_readdir_progress(7, 11, &mut seen, None).is_ok());
    let error = validate_readdir_progress(11, 7, &mut seen, Some(3)).unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), Some(3));
}

#[test]
fn live_close_status_failure_preserves_prefix_and_resumes_only_unexecuted_suffix() {
    use crate::compound::thread_compound_stats;
    use vfsi_core::FailureState;
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    let path = PathBuf::from(format!("/vfsi-close-status-{}", std::process::id()));
    let files: Vec<_> = (0..3)
        .map(|_| {
            fs.open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR, 0o600)
                .unwrap()
        })
        .collect();
    let fd = files[1].fd().unwrap();
    let open = &fs.open_files[&fd];
    // The server has closed this input, but local registration is deliberately
    // retained to exercise an actual CLOSE error inside the compound.
    fs.nfs.close(&open.fh, &open.stateid).unwrap();
    let _ = thread_compound_stats();
    let outcomes = fs.vclose_impl(&files).unwrap();
    let compounds = thread_compound_stats().0;
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(outcomes[1].as_ref().unwrap_err().cause().index(), None);
    assert_eq!(
        compounds, 2,
        "one failed compound plus only the untouched suffix"
    );
    assert_eq!(fs.open_files.len(), 1);
    // Test-induced known-dead state is not an unresolved cleanup obligation.
    fs.open_files.remove(&fd).unwrap();
    fs.unlink_impl(&path).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_close_lost_reply_retains_only_uncertain_owner() {
    use vfsi_core::FailureState;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    let path = PathBuf::from(format!("/vfsi-close-lost-{}", std::process::id()));
    let files: Vec<_> = (0..3)
        .map(|_| {
            fs.open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR, 0o600)
                .unwrap()
        })
        .collect();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterCloseReply { index: 1 },
        VfError::transport(None, "lost CLOSE reply"),
    ));
    fs.set_fault_injector(script.clone());
    let _ = crate::compound::thread_compound_stats();
    let outcomes = fs.vclose_impl(&files).unwrap();
    assert_eq!(
        crate::compound::thread_compound_stats().0,
        1,
        "no replay after lost reply"
    );
    assert!(script.is_consumed());
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        FailureState::Unknown
    );
    assert_eq!(fs.open_files.len(), 1);
    fs.open_files.remove(&files[1].fd().unwrap()).unwrap();
    fs.unlink_impl(&path).unwrap();
}

#[test]
fn live_native_open_read_only_policy_rejects_only_mutating_inputs() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    let path = PathBuf::from(format!("/vfsi-native-open-ro-{}", std::process::id()));
    let new = path.with_extension("uncreated");
    let first = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    fs.write_raw_impl(&first, 0, b"readable").unwrap();
    fs.close_impl(&first).unwrap();
    fs.read_only = true;
    let outcomes = fs
        .vopen_raw_impl(
            &[&path, &new, &path],
            &[libc::O_RDONLY, libc::O_CREAT | libc::O_RDWR, libc::O_RDONLY],
            &[0o600; 3],
        )
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes[0].is_ok() && outcomes[2].is_ok());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().state(),
        vfsi_core::FailureState::NotAttempted
    );
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().cause().raw_code(),
        libc::EROFS as u32
    );
    let files: Vec<_> = outcomes.into_iter().filter_map(Result::ok).collect();
    for file in &files {
        assert_eq!(fs.read_raw_impl(file, 0, 8).unwrap(), b"readable");
    }
    assert!(
        fs.vclose_impl(&files)
            .unwrap()
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert!(!fs.exists_impl(&new).unwrap());
    fs.read_only = false;
    fs.unlink_impl(&path).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_descriptor_write_results_isolate_invalid_inputs_and_wire_failures() {
    use vfsi_core::FailureState;
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for split in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        if split {
            fs.set_max_compound_bytes(64 * 1024);
        }
        let bad_payload = vec![
            b'b';
            if split {
                fs.nfs.per_op_bytes() * 2 + 17
            } else {
                3
            }
        ];
        let paths: Vec<_> = (0..3)
            .map(|index| {
                PathBuf::from(format!(
                    "/vfsi-write-results-{}-{index}",
                    std::process::id()
                ))
            })
            .collect();
        for path in &paths {
            let file = fs
                .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
                .unwrap();
            fs.write_raw_impl(&file, 0, b"old").unwrap();
            fs.close_impl(&file).unwrap();
        }
        let files: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                fs.open_raw_impl(
                    path,
                    if index == 1 {
                        libc::O_RDONLY
                    } else {
                        libc::O_RDWR
                    },
                    0,
                )
                .unwrap()
            })
            .collect();
        let invalid = FileOperand::from_fd(i32::MAX);
        let writes = [
            WriteOp::at(&files[0], 0, b"new".as_slice()),
            WriteOp::at(&invalid, 0, b"bad".as_slice()),
            WriteOp::at(&files[1], 0, bad_payload.as_slice()),
            WriteOp::at(&files[2], 0, b"new".as_slice()),
            WriteOp::at(&files[2], u64::MAX - 1, b"bad".as_slice()),
        ];
        let _ = crate::compound::thread_compound_stats();
        assert!(fs.vwrite_batch_nfs(&[]).unwrap().is_empty());
        let results = fs.vwrite_batch_nfs(&writes).unwrap();
        let stats = crate::compound::thread_compound_stats();
        assert_eq!((stats.0, stats.1), if split { (3, 9) } else { (2, 10) });
        assert_eq!(results.len(), writes.len());
        for index in [0, 3] {
            let result = results[index].as_ref().unwrap();
            assert_eq!(result.bytes_written, 3);
            assert_eq!(result.file, *writes[index].file());
            assert!(result.stable);
        }
        for (index, state) in [
            (1, FailureState::NotAttempted),
            (2, FailureState::Failed),
            (4, FailureState::NotAttempted),
        ] {
            let error = results[index].as_ref().unwrap_err();
            assert_eq!(error.state(), state);
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        }
        for (index, file) in files.iter().enumerate() {
            assert_eq!(
                fs.read_raw_impl(file, 0, 3).unwrap(),
                if index == 1 { b"old" } else { b"new" }
            );
        }
        assert!(fs.vclose_impl(&files).unwrap().iter().all(Result::is_ok));
        for path in &paths {
            fs.unlink_impl(path).unwrap();
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_descriptor_write_results_keep_prefixes_and_never_replay_uncertain_chunks() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vfsi_core::internal::faults::{FaultInjector, FaultScript, OpenFaultPoint};
    use vfsi_core::{FailureState, IoProgress};
    struct LoseSecondReply(AtomicUsize);
    impl FaultInjector for LoseSecondReply {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()> {
            if matches!(point, OpenFaultPoint::BeforeWriteCompound { .. }) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            if matches!(point, OpenFaultPoint::AfterWriteCompoundReply { .. })
                && self.0.load(Ordering::Relaxed) == 2
            {
                return Err(VfError::transport(None, "lost second WRITE reply"));
            }
            Ok(())
        }
    }
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for lost_reply in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let paths: Vec<_> = (0..2)
            .map(|index| {
                PathBuf::from(format!(
                    "/vfsi-write-prefix-{}-{lost_reply}-{index}",
                    std::process::id()
                ))
            })
            .collect();
        let files: Vec<_> = paths
            .iter()
            .map(|path| {
                fs.open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
                    .unwrap()
            })
            .collect();
        fs.set_max_compound_bytes(64 * 1024);
        let per = fs.nfs.per_op_bytes();
        let payload = vec![b'a'; per * 2 + 17];
        let lost = Arc::new(LoseSecondReply(AtomicUsize::new(0)));
        let known = Arc::new(FaultScript::one(
            OpenFaultPoint::AfterWriteChunk { chunk: 0 },
            VfError::errno(None, libc::ENOSPC as u32),
        ));
        let injector: Arc<dyn FaultInjector> = if lost_reply {
            lost.clone()
        } else {
            known.clone()
        };
        fs.set_fault_injector(injector);
        let writes = [
            WriteOp::new(&files[0], VfOffset::Cur, payload.as_slice()),
            WriteOp::at(&files[1], 0, b"untouched".as_slice()),
        ];
        let _ = crate::compound::thread_compound_stats();
        let results = fs.vwrite_batch_nfs(&writes).unwrap();
        let stats = crate::compound::thread_compound_stats();
        let calls = if lost_reply { 2 } else { 1 };
        assert_eq!(
            (stats.0, stats.1),
            (calls, calls * 3),
            "no replay after failure"
        );
        if lost_reply {
            assert_eq!(lost.0.load(Ordering::Relaxed), 2);
        } else {
            assert!(known.is_consumed());
        }
        let error = results[0].as_ref().unwrap_err();
        assert_eq!(
            error.state(),
            if lost_reply {
                FailureState::Unknown
            } else {
                FailureState::Failed
            }
        );
        assert_eq!(error.cause().index(), None);
        assert_eq!(
            error.progress(),
            Some(&IoProgress::Write { bytes_written: per })
        );
        let later = results[1].as_ref().unwrap_err();
        assert_eq!(later.state(), FailureState::NotAttempted);
        assert!(later.progress().is_none());
        assert_eq!(
            fs.seek_raw_impl(&files[0], 0, SeekFrom::Cur).unwrap(),
            per as i64
        );
        assert!(fs.read_raw_impl(&files[1], 0, 16).unwrap().is_empty());
        let on_disk = fs.read_raw_impl(&files[0], 0, payload.len()).unwrap();
        assert_eq!(on_disk, vec![b'a'; per * calls as usize]);
        assert!(fs.vclose_impl(&files).unwrap().iter().all(Result::is_ok));
        for path in &paths {
            fs.unlink_impl(path).unwrap();
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_path_write_close_failure_never_replays_data_and_retains_cleanup_owner() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for defer in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let path = PathBuf::from(format!(
            "/vfsi-write-close-no-replay-{}-{defer}",
            std::process::id()
        ));
        let file = FileOperand::from_os_path(&path);
        fs.merged_mode = MergedIoMode::Full;
        fs.nfs.fail_trailing_write_close_once = true;
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::BeforePathCloseBatch,
            VfError::transport(None, "defer only temporary CLOSE"),
        ));
        if defer {
            fs.set_fault_injector(fault.clone());
        }
        let before = fs.nfs.confirmed_path_closes();
        let _ = crate::compound::thread_compound_stats();
        let written = fs
            .vwrite_impl(&[WriteOp::at(&file, 0, b"written-once".as_slice())
                .with_creation()
                .with_truncate()])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let stats = crate::compound::thread_compound_stats();
        assert_eq!(written[0].bytes_written, b"written-once".len());
        assert_eq!(written[0].offset, 0);
        assert!(matches!(fs.merged_mode, MergedIoMode::OpenWrite));
        assert!(!fs.nfs.fail_trailing_write_close_once);
        assert_eq!(
            stats.0,
            if defer { 1 } else { 2 },
            "only the data compound and optional close-only compound may be sent"
        );
        if defer {
            assert!(fault.is_consumed());
            assert_eq!(fs.nfs.deferred_path_close_count(), 1);
            assert_eq!(fs.nfs.confirmed_path_closes(), before);
        } else {
            assert_eq!(fs.nfs.deferred_path_close_count(), 0);
            assert_eq!(fs.nfs.confirmed_path_closes(), before + 1);
        }
        // The post-failure data is readable without replaying CREATE/TRUNCATE.
        let opened = fs.open_raw_impl(&path, libc::O_RDONLY, 0).unwrap();
        assert_eq!(fs.read_raw_impl(&opened, 0, 32).unwrap(), b"written-once");
        fs.close_impl(&opened).unwrap();
        fs.unlink_impl(&path).unwrap();
        fs.nfs.shutdown().unwrap();
        assert_eq!(fs.nfs.deferred_path_close_count(), 0);
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_path_close_recovery_resumes_only_unwritten_bytes_and_preserves_later_flags() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for alias in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        fs.merged_mode = MergedIoMode::Full;
        fs.set_max_compound_bytes(64 * 1024);
        let paths: Vec<_> = (0..2)
            .map(|index| {
                PathBuf::from(format!("/vfsi-close-resume-{}-{index}", std::process::id()))
            })
            .collect();
        for path in &paths {
            let file = fs
                .open_raw_impl(path, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
                .unwrap();
            fs.write_raw_impl(&file, 0, b"stale tail must disappear")
                .unwrap();
            fs.close_impl(&file).unwrap();
        }
        let per = fs.nfs.per_op_bytes();
        let data: Vec<_> = (0..per * 2 + 17).map(|index| (index % 251) as u8).collect();
        let files: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                FileOperand::from_os_path(if alias && index == 1 { &paths[0] } else { path })
            })
            .collect();
        fs.nfs.fail_trailing_write_close_once = true;
        let before = fs.nfs.confirmed_path_closes();
        let _ = crate::compound::thread_compound_stats();
        let written = fs
            .vwrite_impl(&[
                WriteOp::at(&files[0], 0, data.as_slice())
                    .with_creation()
                    .with_truncate(),
                WriteOp::at(&files[1], 0, b"END".as_slice())
                    .with_creation()
                    .with_truncate(),
            ])
            .and_then(|outcomes| {
                let count = outcomes.len();
                vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
            })
            .unwrap();
        let stats = crate::compound::thread_compound_stats();
        assert_eq!(written.len(), 2);
        assert_eq!(
            (written[0].offset, written[0].bytes_written),
            (0, data.len())
        );
        assert_eq!((written[1].offset, written[1].bytes_written), (0, 3));
        assert!(written.iter().all(|result| result.stable));
        assert!(!fs.nfs.fail_trailing_write_close_once);
        assert!(matches!(fs.merged_mode, MergedIoMode::OpenWrite));
        assert!(
            fs.nfs.confirmed_path_closes() > before,
            "actual stateid must be closed"
        );
        assert_eq!(fs.nfs.deferred_path_close_count(), 0);
        assert!(
            stats.2 < (data.len() + 3 + 16 * 1024) as u64,
            "already acknowledged payload was retransmitted: {} request bytes for {} payload bytes",
            stats.2,
            data.len() + 3
        );
        let expected = if alias {
            [b"END".as_slice(), b"stale tail must disappear".as_slice()]
        } else {
            [data.as_slice(), b"END".as_slice()]
        };
        for (path, expected) in paths.iter().zip(expected) {
            let file = fs.open_raw_impl(path, libc::O_RDONLY, 0).unwrap();
            assert_eq!(fs.read_raw_impl(&file, 0, data.len()).unwrap(), expected);
            fs.close_impl(&file).unwrap();
            fs.unlink_impl(path).unwrap();
        }
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_path_write_minimal_budget_preserves_cleanup_and_completes_without_replay() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    let directory = PathBuf::from(format!("/vfsi-small-write-close-{}", std::process::id()));
    let nested = directory.join("nested");
    fs.mkdir_raw_impl(&directory, 0o755).unwrap();
    fs.mkdir_raw_impl(&nested, 0o755).unwrap();
    fs.nfs.test_limit_compound_operations(8);
    fs.merged_mode = MergedIoMode::Full;
    let path = nested.join("file");
    let target = FileOperand::from_os_path(&path);
    let _ = crate::compound::thread_compound_stats();
    let results = fs
        .vwrite_impl(&[WriteOp::at(&target, 0, b"bounded".as_slice())
            .with_creation()
            .with_truncate()])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_sync::internal::collect_vec_result(Ok(outcomes), count)
        })
        .unwrap();
    let stats = crate::compound::thread_compound_stats();
    assert_eq!(stats.0, 2, "one WRITE compound and one close-only compound");
    assert!(
        stats.3 <= 8,
        "the hard server operation ceiling is respected"
    );
    assert_eq!(results[0].bytes_written, 7);
    assert!(matches!(fs.merged_mode, MergedIoMode::OpenWrite));
    assert_eq!(fs.nfs.deferred_path_close_count(), 0);
    let file = fs.open_raw_impl(&path, libc::O_RDONLY, 0).unwrap();
    assert_eq!(fs.read_raw_impl(&file, 0, 7).unwrap(), b"bounded");
    fs.close_impl(&file).unwrap();
    fs.unlink_impl(&path).unwrap();
    {
        let remove_roots: &[&std::path::Path] = &[&directory];
        vfsi_core::internal::collect_vec_result(
            fs.remove_paths_impl(remove_roots, true),
            remove_roots.len(),
        )
        .map(|_| ())
    }
    .unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_readlink_retains_other_parent_cohorts_after_lookup_reply_loss() {
    use vfsi_core::FailureState;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    fs.auto_reconnect = false;
    let directory = format!("/vfsi-readlink-cohorts-{}", std::process::id());
    for name in ["a", "b"] {
        fs.ensure_dir_impl(Path::new(&format!("{directory}/{name}")), 0o755)
            .unwrap();
    }
    let paths = [
        format!("{directory}/b/healthy"),
        format!("{directory}/a/first"),
        format!("{directory}/a/second"),
    ];
    let targets = [
        Path::new("../raw-target"),
        Path::new("first"),
        Path::new("second"),
    ];
    let refs: Vec<_> = paths.iter().map(Path::new).collect();
    fs.vsymlink_impl(&targets, &refs)
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count).map(|_| ())
        })
        .unwrap();
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterLookupChunkReply { first: 0 },
        VfError::transport(None, "first parent cohort lookup reply lost"),
    ));
    fs.set_fault_injector(fault.clone());
    let _ = crate::compound::thread_compound_stats();
    let results = fs.vreadlink_impl(&refs).unwrap();
    assert!(fault.is_consumed());
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].as_ref().unwrap(), b"../raw-target");
    let visited = fault.visited();
    assert_eq!(
        visited
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::AfterLookupChunkReply { .. }))
            .count(),
        2
    );
    assert_eq!(
        visited
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::AfterReadlinkCompoundReply { .. }))
            .count(),
        1
    );
    for result in &results[1..] {
        let error = result.as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(error.cause().index(), None);
    }
    assert_eq!(
        crate::compound::thread_compound_stats().0,
        7,
        "two parent resolve/type phases, two child lookup batches and one healthy READLINK batch; no replay"
    );
    fs.remove_impl(Path::new(&directory), true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_link_lookup_loss_never_claims_an_unsubmitted_mutation_is_uncertain() {
    use vfsi_core::FailureState;
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for hard in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        fs.auto_reconnect = false;
        let directory = format!("/vfsi-link-lookup-loss-{}-{hard}", std::process::id());
        for name in ["a", "b"] {
            fs.ensure_dir_impl(Path::new(&format!("{directory}/{name}/sub")), 0o755)
                .unwrap();
            let path = PathBuf::from(format!("{directory}/{name}/source"));
            let file = fs
                .open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
                .unwrap();
            fs.close_impl(&file).unwrap();
        }
        let paths = [
            format!("{directory}/b/sub/healthy"),
            format!("{directory}/a/sub/first"),
            format!("{directory}/a/sub/second"),
        ];
        let sources = [
            format!("{directory}/b/source"),
            format!("{directory}/a/source"),
            format!("{directory}/a/source"),
        ];
        let targets: Vec<_> = sources.iter().map(Path::new).collect();
        let refs: Vec<_> = paths.iter().map(Path::new).collect();
        let fault = Arc::new(FaultScript::one(
            OpenFaultPoint::AfterLookupChunkReply { first: 0 },
            VfError::transport(None, "source or parent LOOKUP reply lost before mutation"),
        ));
        fs.set_fault_injector(fault.clone());
        let results = if hard {
            fs.vhardlink_impl(&targets, &refs)
        } else {
            fs.vsymlink_impl(&targets, &refs)
        }
        .unwrap();
        assert!(fault.is_consumed());
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok(), "{hard}: {results:?}");
        for result in &results[1..] {
            let error = result.as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert_eq!(error.cause().index(), None);
            assert!(error.progress().is_none());
        }
        let visited = fault.visited();
        assert_eq!(
            visited
                .iter()
                .filter(|point| matches!(
                    point,
                    OpenFaultPoint::AfterCreateCompoundReply { .. }
                        | OpenFaultPoint::AfterLinkCompoundReply { .. }
                ))
                .count(),
            1
        );
        assert!(fs.exists_impl(refs[0]).unwrap());
        assert!(!fs.exists_impl(refs[1]).unwrap());
        assert!(!fs.exists_impl(refs[2]).unwrap());
        fs.remove_impl(Path::new(&directory), true).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_directory_modes_need_only_the_create_phase() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    let root = PathBuf::from(format!("/vfsi-mkdir-atomic-mode-{}", std::process::id()));
    fs.ensure_dir_impl(&root, 0o755).unwrap();
    let modes = [0o700, 0o751, 0o711];
    let paths: Vec<_> = (0..modes.len())
        .map(|index| root.join(format!("dir-{index}")))
        .collect();
    let attrs: Vec<_> = paths
        .iter()
        .zip(modes)
        .map(|(path, mode)| VfAttrs {
            file: FileOperand::from_os_path(path),
            masks: AttrMask::MODE,
            mode,
            ..Default::default()
        })
        .collect();
    let _ = crate::compound::thread_compound_stats();
    let results = fs.vmkdir_impl(&attrs).unwrap();
    assert!(results.iter().all(Result::is_ok));
    let count = crate::compound::thread_compound_stats().0;
    assert!(
        count <= 3,
        "parent resolution and one CREATE batch need no post-create lookup/chmod; got {count}"
    );
    for (path, mode) in paths.iter().zip(modes) {
        assert_eq!(fs.stat_impl(path).unwrap().mode & 0o7777, mode);
    }
    fs.remove_impl(&root, true).unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_setattr_guard_repair_preserves_alias_order_and_ancestor_following() {
    use vfsi_core::FailureState;
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for case in ["follow-final", "nofollow-final", "nofollow-ancestor"] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let root = PathBuf::from(format!("/vfsi-setattr-guard-{}-{case}", std::process::id()));
        let real = root.join("real");
        fs.ensure_dir_impl(&real, 0o755).unwrap();
        let target = real.join("target");
        let alias = real.join("alias");
        let file = fs
            .open_raw_impl(&target, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
            .unwrap();
        fs.write_raw_impl(&file, 0, b"original").unwrap();
        fs.close_impl(&file).unwrap();
        fs.hard_link_impl(&target, &alias).unwrap();
        let link = root.join("link");
        fs.symlink_impl(
            Path::new(if case == "nofollow-ancestor" {
                "real"
            } else {
                "real/target"
            }),
            &link,
        )
        .unwrap();
        let through_link = if case == "nofollow-ancestor" {
            link.join("target")
        } else {
            link.clone()
        };
        let attrs = [
            VfAttrs {
                file: FileOperand::from_os_path(&through_link),
                masks: AttrMask::MODE | AttrMask::SIZE,
                mode: 0o640,
                size: 3,
                ..Default::default()
            },
            VfAttrs {
                file: FileOperand::from_os_path(&alias),
                masks: AttrMask::MODE | AttrMask::SIZE,
                mode: 0o600,
                size: 1,
                ..Default::default()
            },
        ];
        let results = fs.vsetattrs_nfs(&attrs, case == "follow-final").unwrap();
        assert_eq!(results.len(), 2);
        if case == "nofollow-final" {
            let error = results[0].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert_eq!(error.cause().kind(), std::io::ErrorKind::Unsupported);
        } else {
            assert!(results[0].is_ok(), "{case}: {results:?}");
        }
        assert!(results[1].is_ok());
        let target_attrs = fs.stat_impl(&target).unwrap();
        assert_eq!(
            target_attrs.size, 1,
            "guard repair must precede the later alias update"
        );
        assert_eq!(target_attrs.mode & 0o7777, 0o600);
        assert!(fs.lstat_impl(&link).unwrap().ftype == VfType::Symlink);
        fs.remove_impl(&root, true).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_native_setattrs_preserve_valid_siblings_across_preparation_and_wire_errors() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for ownership in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let root = PathBuf::from(format!(
            "/vfsi-setattrs-sparse-{}-{ownership}",
            std::process::id()
        ));
        fs.ensure_dir_impl(&root, 0o755).unwrap();
        let paths = [root.join("first"), root.join("last")];
        for path in &paths {
            let file = fs
                .open_raw_impl(path, libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC, 0o600)
                .unwrap();
            fs.write_raw_impl(&file, 0, b"original").unwrap();
            fs.close_impl(&file).unwrap();
        }
        let mut owner = [VfAttrs {
            file: FileOperand::from_os_path(&paths[0]),
            masks: AttrMask::UID,
            ..Default::default()
        }];
        vfsi_core::internal::collect_vec_result(fs.getattr_with_recovery(&mut owner, true), 1)
            .unwrap();
        assert!(owner[0].returned.contains(AttrMask::UID));
        let uid = owner[0].uid;
        let extra = if ownership {
            AttrMask::UID
        } else {
            AttrMask::empty()
        };
        let attrs = [
            VfAttrs {
                file: FileOperand::from_os_path(&paths[0]),
                masks: AttrMask::SIZE | AttrMask::MODE | extra,
                size: 2,
                mode: 0o640,
                uid,
                ..Default::default()
            },
            VfAttrs {
                file: FileOperand::from_fd(i32::MAX),
                masks: AttrMask::SIZE | extra,
                uid,
                ..Default::default()
            },
            VfAttrs {
                file: FileOperand::from_os_path(&root.join("missing")),
                masks: AttrMask::SIZE | extra,
                uid,
                ..Default::default()
            },
            VfAttrs {
                file: FileOperand::from_os_path(&paths[0]),
                masks: AttrMask::CTIME | extra,
                uid,
                ..Default::default()
            },
            VfAttrs {
                file: FileOperand::from_os_path(&paths[1]),
                masks: AttrMask::SIZE | AttrMask::MODE | extra,
                size: 3,
                mode: 0o650,
                uid,
                ..Default::default()
            },
        ];
        let fault = Arc::new(FaultScript::new([]));
        fs.set_fault_injector(fault.clone());
        let results = fs.vsetattrs_nfs(&attrs, true).unwrap();
        assert_eq!(results.len(), attrs.len());
        assert!(
            results[0].is_ok() && results[4].is_ok(),
            "ownership={ownership}: {results:?}"
        );
        for index in [1, 3] {
            let error = results[index].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert_eq!(error.cause().index(), None);
        }
        let missing = results[2].as_ref().unwrap_err();
        assert_eq!(
            missing.state(),
            if ownership {
                FailureState::NotAttempted
            } else {
                FailureState::Failed
            }
        );
        assert_eq!(missing.cause().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(missing.cause().index(), None);
        let requests = fault
            .visited()
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::AfterSetattrCompoundReply { .. }))
            .count();
        assert_eq!(
            requests,
            if ownership { 1 } else { 2 },
            "valid siblings retain batching and completed mutations are never replayed"
        );
        for ((path, size), mode) in paths.iter().zip([2, 3]).zip([0o640, 0o650]) {
            let actual = fs.stat_impl(path).unwrap();
            assert_eq!(actual.size, size);
            assert_eq!(actual.mode & 0o7777, mode);
            let mut owner = [VfAttrs {
                file: FileOperand::from_os_path(path),
                masks: AttrMask::UID,
                ..Default::default()
            }];
            vfsi_core::internal::collect_vec_result(fs.getattr_with_recovery(&mut owner, true), 1)
                .unwrap();
            assert!(owner[0].returned.contains(AttrMask::UID));
            assert_eq!(owner[0].uid, uid);
        }
        fs.remove_impl(&root, true).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_rename_ancestor_repair_precedes_later_alias_mutations() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for source_link in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let root = PathBuf::from(format!(
            "/vfsi-rename-repair-{}-{source_link}",
            std::process::id()
        ));
        let real = root.join("real");
        fs.ensure_dir_impl(&real, 0o755).unwrap();
        let file = fs
            .open_raw_impl(
                &real.join("old"),
                libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
                0o600,
            )
            .unwrap();
        fs.write_raw_impl(&file, 0, b"identity").unwrap();
        fs.close_impl(&file).unwrap();
        let alias = root.join("alias");
        fs.symlink_impl(Path::new("real"), &alias).unwrap();
        let source = if source_link {
            alias.join("old")
        } else {
            real.join("old")
        };
        let intermediate = if source_link {
            real.join("middle")
        } else {
            alias.join("middle")
        };
        let pairs = [
            (
                FileOperand::from_os_path(&source),
                FileOperand::from_os_path(&intermediate),
            ),
            (
                FileOperand::from_os_path(&real.join("middle")),
                FileOperand::from_os_path(&real.join("final")),
            ),
        ];
        let results = fs.vrename_nfs(&pairs).unwrap();
        assert!(
            results.iter().all(Result::is_ok),
            "{source_link}: {results:?}"
        );
        assert!(!fs.exists_impl(&real.join("old")).unwrap());
        assert!(!fs.exists_impl(&real.join("middle")).unwrap());
        let file = fs
            .open_raw_impl(&real.join("final"), libc::O_RDONLY, 0)
            .unwrap();
        assert_eq!(fs.read_raw_impl(&file, 0, 32).unwrap(), b"identity");
        fs.close_impl(&file).unwrap();
        assert_eq!(fs.lstat_impl(&alias).unwrap().ftype, VfType::Symlink);
        fs.remove_impl(&root, true).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_native_rename_preserves_sparse_preparation_errors_and_failed_statuses() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    for phased in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
        let root = PathBuf::from(format!(
            "/vfsi-native-rename-{}-{phased}",
            std::process::id()
        ));
        fs.ensure_dir_impl(&root, 0o755).unwrap();
        for name in ["first", "invalid-destination", "last"] {
            let file = fs
                .open_raw_impl(
                    &root.join(name),
                    libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
                    0o600,
                )
                .unwrap();
            fs.write_raw_impl(&file, 0, name.as_bytes()).unwrap();
            fs.close_impl(&file).unwrap();
        }
        let path = |name: &str| FileOperand::from_os_path(&root.join(name));
        let pairs = [
            (path("first"), path("first-renamed")),
            (FileOperand::from_fd(i32::MAX), path("invalid-handle")),
            (path("missing"), path("missing-renamed")),
            (path("invalid-destination"), FileOperand::from_path("/")),
            (path("last"), path("last-renamed")),
        ];
        let fault = Arc::new(FaultScript::new([]));
        fs.set_fault_injector(fault.clone());
        let results = if phased {
            fs.renamev_phased(&pairs)
        } else {
            fs.vrename_nfs(&pairs)
        }
        .unwrap();
        assert_eq!(results.len(), pairs.len());
        assert!(results[0].is_ok() && results[4].is_ok());
        for index in [1, 3] {
            let error = results[index].as_ref().unwrap_err();
            assert_eq!(error.state(), FailureState::NotAttempted);
            assert_eq!(error.cause().index(), None);
        }
        let error = results[2].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::Failed);
        assert_eq!(error.cause().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(error.cause().index(), None);
        assert!(error.progress().is_none());
        assert_eq!(
            fault
                .visited()
                .iter()
                .filter(|point| matches!(point, OpenFaultPoint::AfterRenameCompoundReply { .. }))
                .count(),
            2,
            "failed RENAME is not replayed and valid siblings remain batched"
        );
        for name in ["first", "last"] {
            assert!(!fs.exists_impl(&root.join(name)).unwrap());
            let file = fs
                .open_raw_impl(&root.join(format!("{name}-renamed")), libc::O_RDONLY, 0)
                .unwrap();
            assert_eq!(fs.read_raw_impl(&file, 0, 32).unwrap(), name.as_bytes());
            fs.close_impl(&file).unwrap();
        }
        assert!(fs.exists_impl(&root.join("invalid-destination")).unwrap());
        assert!(!fs.exists_impl(&root.join("missing-renamed")).unwrap());
        fs.remove_impl(&root, true).unwrap();
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_rename_parent_type_failure_is_not_resubmitted_to_rename() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut fs = NfsVecFs::connect_minor(&server, minor).unwrap();
    let root = PathBuf::from(format!("/vfsi-rename-parent-type-{}", std::process::id()));
    fs.ensure_dir_impl(&root, 0o755).unwrap();
    for name in ["parent-file", "healthy"] {
        let file = fs
            .open_raw_impl(
                &root.join(name),
                libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR,
                0o600,
            )
            .unwrap();
        fs.write_raw_impl(&file, 0, name.as_bytes()).unwrap();
        fs.close_impl(&file).unwrap();
    }
    let fault = Arc::new(FaultScript::new([]));
    fs.set_fault_injector(fault.clone());
    let results = fs
        .vrename_nfs(&[
            (
                FileOperand::from_os_path(&root.join("parent-file/from")),
                FileOperand::from_os_path(&root.join("parent-file/to")),
            ),
            (
                FileOperand::from_os_path(&root.join("healthy")),
                FileOperand::from_os_path(&root.join("completed")),
            ),
        ])
        .unwrap();
    assert_eq!(
        results[0].as_ref().unwrap_err().state(),
        FailureState::Failed
    );
    assert_eq!(
        results[0].as_ref().unwrap_err().cause().status(),
        Some(vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_NOTDIR))
    );
    assert!(results[1].is_ok());
    assert_eq!(
        fault
            .visited()
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::AfterRenameCompoundReply { .. }))
            .count(),
        2,
        "type-check failures do not send another RENAME"
    );
    assert!(fs.exists_impl(&root.join("parent-file")).unwrap());
    assert!(!fs.exists_impl(&root.join("healthy")).unwrap());
    assert!(fs.exists_impl(&root.join("completed")).unwrap());
    fs.remove_impl(&root, true).unwrap();
}

#[cfg(all(feature = "test-faults", feature = "server-copy"))]
#[test]
fn live_server_copy_attribute_failure_reports_original_sparse_input() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    for after in [false, true] {
        let mut fs = NfsVecFs::connect_minor(&server, 2).unwrap();
        assert!(fs.server_copy_enabled());
        let root = PathBuf::from(format!(
            "/vfsi-copy-attrs-map-{}-{after}",
            std::process::id()
        ));
        fs.ensure_dir_impl(&root, 0o755).unwrap();
        let sources: Vec<_> = (0..4).map(|i| root.join(format!("src-{i}"))).collect();
        let targets: Vec<_> = (0..4).map(|i| root.join(format!("dst-{i}"))).collect();
        for path in &sources {
            let file = fs
                .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
                .unwrap();
            fs.write_raw_impl(&file, 0, b"source").unwrap();
            fs.close_impl(&file).unwrap();
        }
        let lengths = [None, Some(0), Some(3), Some(3)];
        let pairs: Vec<_> = sources
            .iter()
            .zip(&targets)
            .zip(lengths)
            .map(|((src, dst), length)| ExtentPair::from_os_paths(src, 0, dst, 0, length))
            .collect();
        let point = if after {
            OpenFaultPoint::AfterCopySourceAttrs { index: 0 }
        } else {
            OpenFaultPoint::BeforeCopySourceAttrs
        };
        let fault = Arc::new(FaultScript::one(
            point.clone(),
            VfError::errno(0, libc::EACCES as u32),
        ));
        fs.set_fault_injector(fault.clone());
        let before = fs.server_copy_stats();
        let results = fs.vcopy_impl(&pairs, CopyOptions::new()).unwrap();
        assert_eq!(results.len(), 4);
        for index in [0, 1] {
            assert!(
                results[index].is_ok(),
                "healthy copy {index}: {:?}",
                results[index]
            );
        }
        assert_eq!(results[3].is_ok(), after);
        assert_eq!(
            results[2].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::Failed,
            "the destination was already created"
        );
        let error = results[2].as_ref().unwrap_err().cause();
        let expected_operations = if after { 3 } else { 2 };
        let expected_sizes = if after { [6, 0, 0, 3] } else { [6, 0, 0, 0] };
        assert!(fault.is_consumed());
        assert_eq!(
            fault
                .visited()
                .iter()
                .filter(|visited| **visited == point)
                .count(),
            1
        );
        assert_eq!(error.raw_code(), libc::EACCES as u32);
        let after = fs.server_copy_stats();
        assert_eq!(
            after.requests - before.requests,
            2,
            "only the through-EOF copy needs a continuation"
        );
        assert_eq!(
            after.operations - before.operations,
            expected_operations,
            "failed metadata input never sends COPY"
        );
        assert_eq!(
            after.fallbacks, before.fallbacks,
            "metadata failure is never replayed as copy fallback"
        );
        assert!(
            fs.open_files.is_empty(),
            "every acquired temporary descriptor is retired"
        );
        for (path, size) in targets.iter().zip(expected_sizes) {
            assert_eq!(fs.stat_impl(path).unwrap().size, size);
        }
        fs.remove_impl(&root, true).unwrap();
        assert_eq!(
            error.index(),
            None,
            "slot two supplies identity; its cause has no duplicate index"
        );
    }
}

#[cfg(all(feature = "test-faults", feature = "server-copy"))]
#[test]
fn live_server_copy_lost_reply_never_replays_or_forgets_temporary_owners() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = NfsVecFs::connect_minor(&server, 2).unwrap();
    assert!(fs.server_copy_enabled());
    let root = PathBuf::from(format!("/vfsi-copy-lost-{}", std::process::id()));
    fs.ensure_dir_impl(&root, 0o755).unwrap();
    let mut pairs = Vec::new();
    for index in 0..3 {
        let source = root.join(format!("source-{index}"));
        let destination = root.join(format!("destination-{index}"));
        let file = fs
            .open_raw_impl(&source, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
            .unwrap();
        fs.write_raw_impl(&file, 0, b"source").unwrap();
        fs.close_impl(&file).unwrap();
        pairs.push(ExtentPair::from_os_paths(
            &source,
            0,
            &destination,
            0,
            Some(3),
        ));
    }
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterCopyCompoundReply { first: 0 },
        VfError::transport(None, "lost COPY reply"),
    ));
    fs.set_fault_injector(fault.clone());
    let before = fs.server_copy_stats();
    let results = fs.vcopy_impl(&pairs, CopyOptions::new()).unwrap();
    assert!(fault.is_consumed());
    assert_eq!(results.len(), 3);
    for result in results {
        assert_eq!(result.unwrap_err().state(), FailureState::Unknown);
    }
    let after = fs.server_copy_stats();
    assert_eq!(after.requests - before.requests, 1);
    assert_eq!(after.fallbacks, before.fallbacks);
    assert_eq!(
        fault
            .visited()
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::BeforeCopyCompound { .. }))
            .count(),
        1,
        "no replay after ambiguous COPY"
    );
    assert!(fs.open_files.is_empty());
    assert!(fs.deferred_descriptor_closes.is_empty());
    for pair in &pairs {
        assert_eq!(fs.stat_impl(&pair.src_path).unwrap().size, 6);
        assert_eq!(
            fs.stat_impl(&pair.dst_path).unwrap().size,
            3,
            "the lost reply had actually copied data"
        );
    }
    fs.remove_impl(&root, true).unwrap();
}

#[test]
fn live_object_reference_preparation_rejects_failed_identity_slots_without_caching_them() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = NfsVecFs::connect_minor(&server, 1).unwrap();
    let paths: Vec<_> = (0..2)
        .map(|index| PathBuf::from(format!("/object-ref-error-{}-{index}", std::process::id())))
        .collect();
    let files: Vec<_> = paths
        .iter()
        .map(|path| {
            fs.open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
                .unwrap()
        })
        .collect();
    let bad = files[1].fd().unwrap();
    let original = fs.open_files[&bad].fh.clone();
    fs.open_files.get_mut(&bad).unwrap().fh = NfsFh::test_from_bytes(vec![0xff; 32]);
    let result = fs.prepare_object_refs(&[&files[0], &files[0], &files[1]]);
    fs.open_files.get_mut(&bad).unwrap().fh = original;
    let cached_failed = fs.open_object_ids.contains_key(&bad);
    // Restore/close real retained objects before assertions, even on a red run.
    if result.is_err() {
        fs.prepare_object_refs(&[&files[0], &files[0], &files[1]])
            .unwrap();
        assert!(
            fs.open_object_ids[&bad].is_some(),
            "a later healthy query can establish identity"
        );
    }
    for file in &files {
        fs.close_impl(file).unwrap();
    }
    for path in &paths {
        fs.unlink_impl(path).unwrap();
    }
    let error = result.expect_err("a failed metadata slot cannot establish a retained identity");
    assert_eq!(
        error.index(),
        Some(2),
        "duplicate descriptors must preserve the caller's original position"
    );
    assert!(
        !cached_failed,
        "never cache missing identity from a failed query"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn phased_remove_rejects_non_directory_parents_before_mutation() {
    use vfsi_core::{
        FailureState,
        internal::faults::{FaultScript, OpenFaultPoint},
    };
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let mut fs = NfsVecFs::connect_minor(&server, 1).unwrap();
    let parent = PathBuf::from(format!("/remove-parent-file-{}", std::process::id()));
    let healthy = PathBuf::from(format!("/remove-parent-healthy-{}", std::process::id()));
    for path in [&parent, &healthy] {
        let file = fs
            .open_raw_impl(path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
            .unwrap();
        fs.close_impl(&file).unwrap();
    }
    let fault = Arc::new(FaultScript::new([]));
    fs.set_fault_injector(fault.clone());
    let results = fs
        .removev_phased(&[
            FileOperand::from_os_path(&parent.join("one")),
            FileOperand::from_os_path(&healthy),
            FileOperand::from_os_path(&parent.join("two")),
        ])
        .unwrap();
    assert!(!fs.exists_impl(&healthy).unwrap());
    fs.unlink_impl(&parent).unwrap();
    assert_eq!(results.len(), 3);
    assert!(results[1].is_ok());
    for index in [0, 2] {
        let error = results[index].as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        assert_eq!(error.cause().kind(), std::io::ErrorKind::NotADirectory);
        assert_eq!(error.cause().index(), None);
    }
    assert_eq!(
        fault
            .visited()
            .iter()
            .filter(|point| matches!(point, OpenFaultPoint::BeforeRemoveChunk { .. }))
            .count(),
        1,
        "only the healthy directory can submit REMOVE"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn live_quarantined_identity_reconnects_fresh_and_reopens_exact_retained_objects() {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let owner = format!("quarantined-retained-{}-{minor}", std::process::id()).into_bytes();
    let mut fs = NfsVecFs::connect_with_options(
        &server,
        NfsConnectOptions {
            minorversion: Some(minor),
            client_owner: Some(owner.clone()),
            ..NfsConnectOptions::default()
        },
    )
    .unwrap();
    let path = PathBuf::from(format!(
        "/quarantined-retained-{}-{minor}",
        std::process::id()
    ));
    let saved = path.with_extension("saved");
    let retained = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
        .unwrap();
    fs.write_raw_impl(&retained, 0, b"original").unwrap();
    fs.rename_impl(&path, &saved).unwrap();
    let replacement = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
        .unwrap();
    fs.write_raw_impl(&replacement, 0, b"replacement").unwrap();
    fs.close_impl(&replacement).unwrap();

    assert_eq!(
        fs.connection.client_owner.as_deref(),
        Some(owner.as_slice())
    );
    fs.nfs.quarantine_open_identity();
    fs.reconnect().unwrap();
    assert!(
        fs.connection.client_owner.is_none(),
        "a configured stable identity must not renew unaddressable OPEN state"
    );
    assert!(!fs.nfs.needs_fresh_identity());
    assert_eq!(
        fs.read_raw_impl(&retained, 0, 8).unwrap(),
        b"original",
        "recovery must use the retained FH, never the replacement at its saved path"
    );
    let replacement = fs.open_raw_impl(&path, libc::O_RDONLY, 0).unwrap();
    assert_eq!(
        fs.read_raw_impl(&replacement, 0, 11).unwrap(),
        b"replacement"
    );
    fs.close_impl(&replacement).unwrap();
    fs.close_impl(&retained).unwrap();
    fs.unlink_impl(&saved).unwrap();
    fs.unlink_impl(&path).unwrap();
    fs.shutdown().unwrap();
}

#[cfg(feature = "test-faults")]
fn cleanup_test_backend() -> Option<NfsVecFs> {
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return None;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    Some(NfsVecFs::connect_minor(&server, minor).unwrap())
}

#[cfg(feature = "test-faults")]
#[test]
fn live_cleanup_scalar_registration_failure_retains_remote_close() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Some(mut fs) = cleanup_test_backend() else {
        return;
    };
    let path = PathBuf::from(format!("/cleanup-register-{}", std::process::id()));
    let script = Arc::new(FaultScript::new([
        (
            OpenFaultPoint::BeforeRegister { index: 0 },
            VfError::errno(None, libc::EMFILE as u32),
        ),
        (
            OpenFaultPoint::BeforePathClose,
            VfError::transport(None, "CLOSE unavailable"),
        ),
    ]));
    fs.set_fault_injector(script.clone());
    let error = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::EMFILE as u32);
    assert!(script.is_consumed());
    assert!(fs.open_files.is_empty());
    let retained = fs.nfs.deferred_path_close_count();
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    fs.unlink_impl(&path).unwrap();
    fs.shutdown().unwrap();
    assert_eq!(
        retained, 1,
        "failed cleanup must retain its acquired OPEN state"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn live_cleanup_lost_close_reply_does_not_block_later_vector_open() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Some(mut fs) = cleanup_test_backend() else {
        return;
    };
    let path = PathBuf::from(format!("/cleanup-lost-close-{}", std::process::id()));
    let file = fs
        .open_raw_impl(&path, libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap();
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterCloseReply { index: 0 },
        VfError::transport(None, "lost CLOSE reply"),
    ));
    fs.set_fault_injector(fault.clone());
    let outcomes = fs.close_temporary_descriptors(vec![file]).unwrap();
    assert!(outcomes[0].is_err() && fault.is_consumed());
    assert_eq!(fs.deferred_descriptor_closes.len(), 1);
    let opened = fs.vopen_raw_impl(&[path.as_path()], &[libc::O_RDONLY], &[0]);
    assert!(
        opened.is_ok(),
        "terminal cleanup status must not poison later opens: {opened:?}"
    );
    let file = opened.unwrap().remove(0).unwrap();
    assert!(fs.deferred_descriptor_closes.is_empty());
    fs.close_impl(&file).unwrap();
    fs.unlink_impl(&path).unwrap();
    fs.shutdown().unwrap();
}

#[cfg(feature = "test-faults")]
#[test]
fn live_cleanup_descriptor_drain_retires_exact_successes_and_terminal_states() {
    for terminal in [false, true] {
        let Some(mut fs) = cleanup_test_backend() else {
            return;
        };
        let paths: Vec<_> = (0..3)
            .map(|i| {
                PathBuf::from(format!(
                    "/cleanup-drain-{}-{terminal}-{i}",
                    std::process::id()
                ))
            })
            .collect();
        let files: Vec<_> = paths
            .iter()
            .map(|path| {
                fs.open_raw_impl(path, libc::O_CREAT | libc::O_RDWR, 0o600)
                    .unwrap()
            })
            .collect();
        let original = fs.open_files[&files[1].fd().unwrap()].fh.clone();
        let invalid = NfsFh::test_from_bytes(vec![0xff; 32]);
        if terminal {
            let open = &fs.open_files[&files[1].fd().unwrap()];
            fs.nfs.close(&open.fh, &open.stateid).unwrap();
        } else {
            fs.open_files.get_mut(&files[1].fd().unwrap()).unwrap().fh = invalid.clone();
        }
        for file in files {
            fs.deferred_descriptor_closes
                .push(fs.open_files.remove(&file.fd().unwrap()).unwrap());
        }
        let result = fs.drain_deferred_descriptor_closes();
        let retained = fs.deferred_descriptor_closes.len();
        // Repair the synthetic fault before checking the regression assertions.
        fs.deferred_descriptor_closes.retain_mut(|open| {
            if terminal && open.fh == original {
                return false;
            }
            if open.fh == invalid {
                open.fh = original.clone();
            }
            true
        });
        fs.drain_deferred_descriptor_closes().unwrap();
        for path in paths {
            fs.unlink_impl(&path).unwrap();
        }
        fs.shutdown().unwrap();
        assert_eq!(result.is_ok(), terminal, "{result:?}");
        assert_eq!(
            retained,
            usize::from(!terminal),
            "only unresolved owners may remain"
        );
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn live_cleanup_partial_path_open_failure_retains_failed_batch_cleanup() {
    use crate::client::{OpenCreate, OpenOp};
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Some(mut fs) = cleanup_test_backend() else {
        return;
    };
    let root = fs.nfs.root().clone();
    let name = format!("cleanup-open-batch-{}", std::process::id());
    let dir = fs.nfs.mkdir(&root, &name).unwrap();
    let (file, state) = fs
        .nfs
        .open(
            &dir,
            b"file",
            OPEN4_SHARE_ACCESS_READ,
            OpenCreate::Unchecked,
        )
        .unwrap();
    fs.nfs.close(&file, &state).unwrap();
    let ops: Vec<_> = [b"file".as_slice(), b"missing"]
        .into_iter()
        .map(|name| OpenOp {
            dir: dir.clone(),
            name: name.to_vec(),
            access: OPEN4_SHARE_ACCESS_READ,
            create: OpenCreate::NoCreate,
        })
        .collect();
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforePathCloseBatch,
        VfError::transport(None, "cleanup unavailable"),
    ));
    fs.set_fault_injector(fault.clone());
    assert!(fs.nfs.open_many_path(&ops).is_err());
    assert!(fault.is_consumed());
    let retained = fs.nfs.deferred_path_close_count();
    fs.set_fault_injector(Arc::new(FaultScript::new([])));
    fs.nfs.remove(&dir, "file").unwrap();
    fs.nfs.remove(&root, &name).unwrap();
    fs.shutdown().unwrap();
    assert_eq!(retained, 1);
}
