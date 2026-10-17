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
        file: FileOperand::from_path("/f"),
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
            file: FileOperand::from_path("/one"),
            offset: 0,
            data: b"a".to_vec(),
            eof: true,
        },
        ReadResult {
            file: FileOperand::from_path("/two"),
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
    let policy = NfsRecoveryPolicy::new().reconnect_attempts(3);
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

    // Preserve the previous type and accessor for callers migrating gradually.
    let legacy: Option<vfsi_core::FileIdentity> = attrs.file_identity();
    assert_eq!(legacy, Some(id));
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
        &[10, 20, 30],
        &mut payloads,
        &[Some(false), None, Some(true)],
    );
    assert_eq!(output.len(), reads.len());
    assert!(payloads.iter().all(Option::is_none));
    assert_eq!(output[0].data.as_ptr(), first_pointer);
    assert_eq!(output[0].data.capacity(), first_capacity);
    assert_eq!(output[0].data, b"a\x00\xffb");
    assert!(output[1].data.is_empty());
    assert_eq!(output[2].data.as_ptr(), third_pointer);
    assert_eq!(output[2].data, b"tail");
    for (index, result) in output.iter().enumerate() {
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
            .err_no(),
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
    let error = backend
        .vsetattrs_raw_nofollow_impl(std::slice::from_ref(&mode_update))
        .unwrap_err();
    assert_eq!(error.err_no(), VF_ERR_UNSUPPORTED);
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
    backend
        .vsetattrs_raw_nofollow_impl(std::slice::from_ref(&time_update))
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
    backend
        .vsetattrs_raw_nofollow_impl(&[target_mode_update.clone(), link_time_update.clone()])
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
            backend.vsetattrs_raw_impl(&updates).unwrap();
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
            let error = backend.vsetattrs_raw_impl(&updates).unwrap_err();
            assert_eq!(error.index(), Some(missing_index));
            assert_eq!(error.err_no(), ERR_NOENT);
            assert_eq!(
                backend.stat_impl(Path::new("/tail")).unwrap().mode & 0o777,
                0o640,
                "do not execute the suffix after a later failure"
            );

            // The optimized guard path must not allocate a suffix-sized type vector
            // that its caller never consumes.
            let operation = crate::client::PathSetattrOp {
                file: crate::client::FileRef::Path(
                    crate::path::path_bytes(&backend.server_path(Path::new("/tail"))).to_vec(),
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
            assert!(outcome.failed.is_none());
            assert!(outcome.types.is_empty());
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
