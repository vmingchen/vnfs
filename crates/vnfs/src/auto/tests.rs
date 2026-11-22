use super::routing::{MountSpec, parse_mount};
use super::*;
use crate::ListDirOptions;
use crate::Vfsi;
use std::io::Read;
use vfsi_nfs::mount::decode_mount_field;
use vfsi_sync::DEFAULT_READ_ALLV_MAX_TOTAL_BYTES;

#[test]
fn auto_empty_batches_do_not_read_mounts() {
    let root = tempfile::tempdir().unwrap();
    let client = Auto::new(root.path()).unwrap();
    let reads = &routing::MOUNT_INFO_READS;
    let before = reads.get();
    assert!(client.vopen(&[]).unwrap().is_empty());
    assert!(
        client
            .vcopy::<&str, &str>(&[], vfsi_core::api::CopyOptions::new())
            .unwrap()
            .is_empty()
    );
    assert_eq!(reads.get(), before);
}

#[test]
fn auto_metadata_routes_only_admitted_path_operands() {
    use crate::{AttrsOptions, SetAttrsOp, Target, VfsiExt};
    use vfsi_core::FailureState;
    let root = tempfile::tempdir().unwrap();
    let client = Auto::new(root.path()).unwrap();
    client.write("/file", b"contents").unwrap();
    let file = client
        .open_with(crate::files::OpenOp::new(
            "/file",
            crate::files::OpenFlags::WRITE,
        ))
        .unwrap();
    let reads = &routing::MOUNT_INFO_READS;
    let before = reads.get();

    client
        .vgetattrs(&[Target::file(&file)], AttrsOptions::new())
        .unwrap()[0]
        .as_ref()
        .unwrap();
    client.vstatfs(&[Target::file(&file)]).unwrap()[0]
        .as_ref()
        .unwrap();
    client
        .vsetattrs(&[SetAttrsOp::new(Target::file(&file)).len(5)])
        .unwrap()[0]
        .as_ref()
        .unwrap();
    let invalid = SetAttrsOp::new(Target::path(&"/invalid")).uid(u32::MAX);
    let result = client.vsetattrs(&[invalid]).unwrap();
    let error = result[0].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::NotAttempted);
    assert_eq!(error.cause().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        vfsi_core::internal::errors::path(error.cause()),
        Some(Path::new("/invalid"))
    );
    assert_eq!(reads.get(), before);

    let results = client
        .vsetattrs(&[
            SetAttrsOp::new(Target::path(&"/file")).len(3),
            invalid,
            SetAttrsOp::new(Target::file(&file)).len(2),
        ])
        .unwrap();
    assert!(results[0].is_ok() && results[2].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().state(),
        FailureState::NotAttempted
    );
    assert_eq!(reads.get(), before + 1);
    assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"co");
}

#[test]
fn allocation_auto_routes_retain_one_shared_pointer() {
    use super::routing::NfsConnectionState;
    use std::mem::size_of;

    #[allow(dead_code)]
    enum InlineRoute {
        Posix,
        Nfs(NfsConnectionState),
    }
    #[allow(dead_code)]
    struct InlineAutoFile {
        path: PathBuf,
        route: InlineRoute,
        inner: AutoFileInner,
        owner: Arc<()>,
    }
    assert_eq!(size_of::<NfsConnection>(), size_of::<usize>());
    assert!(size_of::<Route>() <= 2 * size_of::<usize>());
    assert!(size_of::<Route>() < size_of::<InlineRoute>());
    assert!(size_of::<AutoFile>() < size_of::<InlineAutoFile>());
    println!(
        "retained layout: Route {} -> {} bytes; AutoFile {} -> {} bytes",
        size_of::<InlineRoute>(),
        size_of::<Route>(),
        size_of::<InlineAutoFile>(),
        size_of::<AutoFile>(),
    );
}

fn require_live_direct_route(client: &Auto, path: &Path) {
    // Ganesha/kernel mount setup can briefly return EREMOTEIO. Retry only
    // the read-only eligibility probe, never an application mutation.
    let mut route = client.route_for(path);
    for delay_ms in [5, 20, 100] {
        if matches!(route, AutoRoute::DirectNfs { .. }) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        route = client.route_for(path);
    }
    assert!(
        matches!(route, AutoRoute::DirectNfs { .. }),
        "expected direct NFS for {path:?}, got {route:?}"
    );
}

#[test]
fn child_seeds_recheck_routing_and_keep_owner_validation() {
    use crate::{Attributes, VfsiExt};
    let root = tempfile::tempdir().unwrap();
    let client = Auto::new(root.path()).unwrap();
    client.create_dir("/dir").unwrap();
    client.write("/dir/a", b"x").unwrap();
    client.write("/dir/b", b"x").unwrap();
    for reroute in [false, true] {
        let (first, next, _) = {
            let page_inputs: &[&Path] = &[Path::new("/dir")];
            vfsi_core::internal::collect_vec_result(
                client.read_dir_pages_with_fields(
                    page_inputs,
                    Attributes::MODE,
                    vec![None],
                    1,
                    10,
                    true,
                ),
                page_inputs.len(),
            )
        }
        .unwrap()
        .remove(0);
        let mut saved = next.unwrap().into_state::<RoutedDirectoryCursor>().unwrap();
        if reroute {
            saved.backend_path = "/obsolete".into();
        }
        let seed = vfsi_sync::DirPageCursor::new(RoutedChildDirectoryCursor(saved));
        let (page, _, _) = {
            let page_inputs: &[&Path] = &[Path::new("/dir")];
            vfsi_core::internal::collect_vec_result(
                client.read_dir_pages_with_fields(
                    page_inputs,
                    Attributes::MODE,
                    vec![Some(seed)],
                    1,
                    10,
                    true,
                ),
                page_inputs.len(),
            )
        }
        .unwrap()
        .remove(0);
        assert_eq!(page.entries.len(), 1);
        if reroute {
            assert_eq!(
                page.entries, first.entries,
                "changed routing must start a fresh page"
            );
        } else {
            assert_ne!(
                page.entries, first.entries,
                "unchanged routing must preserve the cursor"
            );
        }
    }
    let (_, next, _) = {
        let page_inputs: &[&Path] = &[Path::new("/dir")];
        vfsi_core::internal::collect_vec_result(
            client.read_dir_pages_with_fields(
                page_inputs,
                Attributes::MODE,
                vec![None],
                1,
                10,
                true,
            ),
            page_inputs.len(),
        )
    }
    .unwrap()
    .remove(0);
    let saved = next.unwrap().into_state::<RoutedDirectoryCursor>().unwrap();
    let seed = vfsi_sync::DirPageCursor::new(RoutedChildDirectoryCursor(saved));
    let other = Auto::new(root.path()).unwrap();
    assert_eq!(
        ({
            let page_inputs: &[&Path] = &[Path::new("/dir")];
            vfsi_core::internal::collect_vec_result(
                other.read_dir_pages_with_fields(
                    page_inputs,
                    Attributes::MODE,
                    vec![Some(seed)],
                    1,
                    10,
                    true,
                ),
                page_inputs.len(),
            )
        }
        .err()
        .unwrap()
        .cause())
        .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
#[ignore = "requires an isolated root child and VFSI_NFS_SERVER; no mount required"]
fn live_retained_directory_cursor_rechecks_filesystem_credentials() {
    use crate::{Attributes, NfsVersion};

    struct RestoreFsuid(libc::uid_t);
    impl Drop for RestoreFsuid {
        fn drop(&mut self) {
            // SAFETY: the isolated child retains euid 0 and restores its own fsuid.
            unsafe { libc::setfsuid(self.0) };
        }
    }

    // Credential changes are confined to the explicitly selected test child.
    // SAFETY: geteuid is a credential query with no pointer arguments.
    let euid = unsafe { libc::geteuid() };
    assert_eq!(euid, 0, "run this test in a root child");
    let server = std::env::var("VFSI_NFS_SERVER").expect("live NFS server required");
    let export = std::env::var("VFSI_NFS_EXPORT").unwrap_or_else(|_| "/".into());
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let telemetry = crate::nfs::Telemetry::default();
    let remote = Nfs::builder(&server)
        .root(&export)
        .version(NfsVersion::try_from(Some(minor)).unwrap())
        .telemetry(telemetry.clone())
        .connect()
        .unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = PathBuf::from(format!("/auto-cursor-auth-{}-{nonce}", std::process::id()));
    remote
        .vmkdir(&[crate::MkDirOp::new(&path, 0o700)])
        .and_then(|outcomes| {
            let count = outcomes.len();
            vfsi_core::internal::collect_vec_result(Ok(outcomes), count)
                .map(|_| ())
                .map_err(vfsi_core::IndexedError::into_cause)
        })
        .unwrap();
    remote.write(path.join("first"), b"private").unwrap();
    remote.write(path.join("second"), b"private").unwrap();
    let local = tempfile::tempdir().unwrap();
    let client = Auto::new(local.path()).unwrap();
    let credentials = AuthSysIdentity::current().unwrap();
    let connection = Arc::new(routing::NfsConnectionState {
        spec: MountSpec {
            id: u64::MAX,
            mount_point: local.path().to_path_buf(),
            export: export.into(),
            server: server.into_boxed_str(),
            minor,
        },
        client: remote.inner.clone(),
        identity: Arc::new(()),
        credentials: credentials.clone(),
    });
    let mut outcomes = Vec::new();
    for (changed, index, child) in [
        (false, 0, false),
        (true, 0, false),
        (true, 1, false),
        (true, 1, true),
    ] {
        let (_, next, _) = {
            let page_inputs: &[&Path] = &[&path];
            vfsi_core::internal::collect_vec_result(
                remote.inner.read_dir_pages_with_fields(
                    page_inputs,
                    Attributes::MODE,
                    vec![None],
                    1,
                    10,
                    true,
                ),
                page_inputs.len(),
            )
        }
        .unwrap()
        .remove(0);
        let saved = RoutedDirectoryCursor {
            route: Route::Nfs(connection.clone()),
            backend_path: path.clone(),
            public_path: path.clone(),
            owner: client.owner.clone(),
            cursor: next.expect("the live directory must have a second page"),
        };
        let cursor = if child {
            vfsi_sync::DirPageCursor::new(RoutedChildDirectoryCursor(saved))
        } else {
            vfsi_sync::DirPageCursor::new(saved)
        };
        let before = telemetry.snapshot().calls;
        let previous = changed.then(|| {
            // SAFETY: setfsuid affects only this isolated calling thread; the
            // guard restores the exact prior identity, including on unwind.
            let previous = unsafe { libc::setfsuid(65534) };
            RestoreFsuid(previous as libc::uid_t)
        });
        let mismatched = AuthSysIdentity::current().as_ref() != Some(&credentials);
        let mut paths = Vec::new();
        let mut cursors = Vec::new();
        if index == 1 {
            paths.push(Path::new("/unvisited"));
            cursors.push(None);
        }
        paths.push(path.as_path());
        cursors.push(Some(cursor));
        let result =
            client.read_dir_pages_with_fields(&paths, Attributes::MODE, cursors, 1, 10, true);
        drop(previous);
        outcomes.push((
            changed,
            index,
            mismatched,
            before,
            telemetry.snapshot().calls,
            result,
        ));
    }
    // Remove only the successfully created remote fixture before assertions.
    remote.remove_dir_all(&path).unwrap();
    assert_eq!(AuthSysIdentity::current().as_ref(), Some(&credentials));
    for (changed, index, mismatched, before, after, result) in outcomes {
        assert_eq!(mismatched, changed);
        if changed {
            let result = result.unwrap();
            let error = result[index]
                .as_ref()
                .err()
                .expect("stale credentials must reject the page");
            assert_eq!(crate::test_native_code(error.cause()), libc::EACCES as u32);

            assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
            assert_eq!(
                vfsi_core::internal::errors::path(error.cause()),
                Some(path.as_path())
            );
            assert_eq!(after, before, "reject before any retained-identity RPC");
        } else {
            assert_eq!(result.unwrap()[0].as_ref().unwrap().0.entries.len(), 1);
        }
    }
}

#[test]
fn routed_pages_enforce_public_budgets_and_pin_cursor_ownership() {
    use crate::{Attributes, ListDirOptions, VfsiExt};
    let root = tempfile::tempdir().unwrap();
    let client = Auto::new(root.path()).unwrap();
    client.create_dir("/dir").unwrap();
    client.create_dir("/other").unwrap();
    client.write("/dir/a", b"x").unwrap();
    client.write("/dir/b", b"x").unwrap();
    assert_eq!(
        client
            .read_dirs_with_options(&["/dir"], ListDirOptions::new().with_max_path_bytes(12))
            .unwrap()[0][0]
            .entries
            .len(),
        2
    );
    let error = client
        .read_dirs_with_options(&["/dir"], ListDirOptions::new().with_max_path_bytes(11))
        .unwrap_err();
    assert_eq!(error.cause().kind(), crate::ErrorKind::FileTooLarge);
    assert_eq!(error.index(), Some(0));
    let cursor = {
        let page_inputs: &[&Path] = &[Path::new("/dir")];
        vfsi_core::internal::collect_vec_result(
            client.read_dir_pages_with_fields(
                page_inputs,
                Attributes::MODE,
                vec![None],
                1,
                10,
                true,
            ),
            page_inputs.len(),
        )
    }
    .unwrap()
    .remove(0)
    .1
    .unwrap();
    let other = Auto::new(root.path()).unwrap();
    assert_eq!(
        ({
            let page_inputs: &[&Path] = &[Path::new("/dir")];
            vfsi_core::internal::collect_vec_result(
                other.read_dir_pages_with_fields(
                    page_inputs,
                    Attributes::MODE,
                    vec![Some(cursor)],
                    1,
                    10,
                    true,
                ),
                page_inputs.len(),
            )
        }
        .err()
        .unwrap()
        .cause())
        .kind(),
        std::io::ErrorKind::InvalidInput
    );
    let cursor = {
        let page_inputs: &[&Path] = &[Path::new("/dir")];
        vfsi_core::internal::collect_vec_result(
            client.read_dir_pages_with_fields(
                page_inputs,
                Attributes::MODE,
                vec![None],
                1,
                10,
                true,
            ),
            page_inputs.len(),
        )
    }
    .unwrap()
    .remove(0)
    .1
    .unwrap();
    assert_eq!(
        ({
            let page_inputs: &[&Path] = &[Path::new("/other")];
            vfsi_core::internal::collect_vec_result(
                client.read_dir_pages_with_fields(
                    page_inputs,
                    Attributes::MODE,
                    vec![Some(cursor)],
                    1,
                    10,
                    true,
                ),
                page_inputs.len(),
            )
        }
        .err()
        .unwrap()
        .cause())
        .kind(),
        std::io::ErrorKind::InvalidInput
    );
    let cursor = {
        let page_inputs: &[&Path] = &[Path::new("/dir")];
        vfsi_core::internal::collect_vec_result(
            client.read_dir_pages_with_fields(
                page_inputs,
                Attributes::MODE,
                vec![None],
                1,
                10,
                true,
            ),
            page_inputs.len(),
        )
    }
    .unwrap()
    .remove(0)
    .1
    .unwrap();
    let (page, next, _) = {
        let page_inputs: &[&Path] = &[Path::new("/dir")];
        vfsi_core::internal::collect_vec_result(
            client.read_dir_pages_with_fields(
                page_inputs,
                Attributes::MODE,
                vec![Some(cursor)],
                1,
                10,
                true,
            ),
            page_inputs.len(),
        )
    }
    .unwrap()
    .remove(0);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].path().parent(), Some(Path::new("/dir")));
    if let Some(cursor) = next {
        let (page, next, _) = {
            let page_inputs: &[&Path] = &[Path::new("/dir")];
            vfsi_core::internal::collect_vec_result(
                client.read_dir_pages_with_fields(
                    page_inputs,
                    Attributes::MODE,
                    vec![Some(cursor)],
                    1,
                    10,
                    true,
                ),
                page_inputs.len(),
            )
        }
        .unwrap()
        .remove(0);
        assert!(page.entries.is_empty());
        assert!(next.is_none());
    }
}

#[test]
fn fixed_text_mount_specs_keep_endpoints_and_public_strings() {
    use std::mem::{size_of, size_of_val};

    let line = b"46 49 0:45 / /mnt/nfs rw,relatime - nfs4 server:/export rw,vers=4.2,proto=tcp,sec=sys,addr=2001:db8::7";
    let spec = parse_mount(line).unwrap();
    assert_eq!(spec.server.as_ref(), "[2001:db8::7]:2049");
    assert_eq!(
        size_of::<String>() - size_of_val(&spec.server),
        size_of::<usize>()
    );
    let cloned = spec.clone();
    assert_eq!(cloned, spec);
    assert_ne!(cloned.server.as_ptr(), spec.server.as_ptr());
    drop(spec);
    assert_eq!(cloned.server.as_ref(), "[2001:db8::7]:2049");

    // The public parsed mount report retains its mutable String field.
    let mut info = vfsi_nfs::mount::parse_mount(line).unwrap();
    let public_server: &mut String = &mut info.server;
    public_server.push_str("-edited");
    assert_eq!(cloned.server.as_ref(), "[2001:db8::7]:2049");
    let report = AutoRoute::DirectNfs {
        mount_point: cloned.mount_point,
        server: cloned.server.into_string(),
    };
    let AutoRoute::DirectNfs { mut server, .. } = report else {
        unreachable!()
    };
    server.push_str("-edited");
    assert_eq!(server, "[2001:db8::7]:2049-edited");
}

#[test]
fn parses_only_unambiguous_supported_mounts() {
    let line = b"46 49 0:45 / /mnt/nfs rw,relatime - nfs4 server:/export rw,vers=4.2,proto=tcp,sec=sys,addr=192.0.2.7";
    let spec = parse_mount(line).unwrap();
    assert_eq!(spec.mount_point, Path::new("/mnt/nfs"));
    assert_eq!(spec.server.as_ref(), "192.0.2.7:2049");
    assert_eq!(spec.export, Path::new("/export"));
    assert_eq!(spec.minor, 2);
    let line = std::str::from_utf8(line).unwrap();
    assert!(parse_mount(line.replace("sec=sys", "sec=krb5").as_bytes()).is_none());
    assert!(parse_mount(line.replace("sec=sys", "sec=sys:krb5").as_bytes()).is_none());
    assert!(parse_mount(line.replace("sec=sys", "sec=sys,sec=krb5").as_bytes()).is_none());
    assert!(parse_mount(line.replace("vers=4.2", "vers=3").as_bytes()).is_none());
    assert!(parse_mount(line.replace("vers=4.2", "vers=4.0").as_bytes()).is_none());
    assert!(parse_mount(line.replace(" rw,relatime", " ro,relatime").as_bytes()).is_none());
    assert!(parse_mount(line.replace(" / /mnt/nfs ", " /sub /mnt/nfs ").as_bytes()).is_none());
    assert!(parse_mount(line.replace("sec=sys", "sec=sys,port=bad").as_bytes()).is_none());
    assert!(parse_mount(line.replace("sec=sys", "sec=sys,port=0").as_bytes()).is_none());
    assert!(parse_mount(line.replace(",addr=192.0.2.7", "").as_bytes()).is_none());
    assert!(parse_mount(line.replace("addr=192.0.2.7", "addr=server").as_bytes()).is_none());
    assert!(
        parse_mount(
            line.replace("addr=192.0.2.7", "addr=192.0.2.7,addr=192.0.2.8")
                .as_bytes()
        )
        .is_none()
    );
    assert_eq!(
        parse_mount(
            line.replace("addr=192.0.2.7", "addr=2001:db8::7")
                .as_bytes()
        )
        .unwrap()
        .server
        .as_ref(),
        "[2001:db8::7]:2049"
    );
}

#[test]
fn decodes_mountinfo_escapes() {
    assert_eq!(
        decode_mount_field(br"/mnt/with\040space").unwrap(),
        Path::new("/mnt/with space")
    );
    assert!(decode_mount_field(br"/mnt/bad\08x").is_none());
    assert!(decode_mount_field(br"/mnt/bad\777").is_none());
}

#[test]
fn mount_id_prevents_prefix_based_misrouting() {
    let client = Auto::new("/").unwrap();
    let local = std::env::temp_dir();
    let fake = MountSpec {
        id: u64::MAX,
        mount_point: PathBuf::from("/"),
        export: PathBuf::from("/"),
        server: "127.0.0.1:2049".into(),
        minor: 2,
    };
    assert!(matches!(
        client
            .resolve(
                &local,
                &MountTable {
                    eligible: vec![fake],
                    ..MountTable::default()
                }
            )
            .route,
        Route::Posix
    ));
}

#[test]
fn local_paths_stay_mounted_and_open_file_roundtrips() {
    let root = std::env::temp_dir().join(format!(
        "vnfs-auto-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    fs::create_dir_all(&root).unwrap();
    let client = Auto::new(&root).unwrap();
    assert_eq!(client.route_for("/file"), AutoRoute::Posix);
    let files = client
        .open_batch(&[
            OpenOp::new(
                "/one",
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
            ),
            OpenOp::new(
                "/two",
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
            ),
        ])
        .unwrap();
    client
        .write_batch(
            &[
                crate::WriteOp::at(&files[0], 0, b"one"),
                crate::WriteOp::at(&files[1], 0, b"two"),
            ],
            Default::default(),
        )
        .unwrap();
    let reads = client
        .read_batch(
            [
                crate::ReadOp::range(&files[0], 0, 3),
                crate::ReadOp::range(&files[1], 0, 3),
            ],
            crate::ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(reads[0].data().unwrap(), b"one");
    assert_eq!(reads[1].data().unwrap(), b"two");
    std::os::unix::fs::symlink("one", root.join("link")).unwrap();
    let metadata = client
        .mounted
        .getattrs_all(
            &[Path::new("/one"), Path::new("/link")],
            crate::AttrsOptions::new().with_follow_final_symlink(false),
        )
        .unwrap();
    assert_eq!(metadata[0].file_type(), crate::FileType::Regular);
    assert_eq!(metadata[1].file_type(), crate::FileType::Symlink);
    let error = client
        .mounted
        .getattrs_all(
            &[Path::new("/one"), Path::new("/missing")],
            crate::AttrsOptions::new().with_follow_final_symlink(false),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let tiny = Auto::new(&root)
        .unwrap()
        .with_limits(ResourceLimits::new().with_max_read_bytes(5));
    let tiny_file = tiny.open("/one").unwrap();
    let error = tiny
        .read_batch(
            [crate::ReadOp::range(&tiny_file, 0, 6)],
            crate::ReadOptions::default(),
        )
        .unwrap_err();
    assert_eq!(crate::test_native_code(error.cause()), libc::EFBIG as u32);
    assert_eq!(
        tiny.read_batch(
            [crate::ReadOp::range(&tiny_file, 0, 3)],
            crate::ReadOptions::default()
        )
        .unwrap()[0]
            .data()
            .unwrap(),
        b"one"
    );
    tiny_file.close().unwrap();
    let error = client
        .read_batch(
            [crate::ReadOp::range(
                &files[0],
                0,
                DEFAULT_READ_ALLV_MAX_TOTAL_BYTES + 1,
            )],
            crate::ReadOptions::default(),
        )
        .unwrap_err();
    assert_eq!(crate::test_native_code(error.cause()), libc::EFBIG as u32);
    client.close_files(files).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn strict_write_all_collection_retains_healthy_effects_around_local_rejection() {
    let root = std::env::temp_dir().join(format!(
        "auto-local-preflight-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    fs::create_dir(&root).unwrap();
    let client = Auto::new(&root).unwrap();
    for closed in [false, true] {
        client.write("/first", b"original").unwrap();
        client.write("/second", b"original").unwrap();
        let mut files = client
            .open_batch(
                &["/first", "/second"]
                    .map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE)),
            )
            .unwrap();
        if closed {
            files[1].try_close().unwrap();
        }
        let error = client
            .write_batch(
                &[
                    crate::WriteOp::at(&files[0], 0, b"changed"),
                    crate::WriteOp::at(
                        &files[1],
                        if closed { 0 } else { u64::MAX },
                        if closed { b"" } else { b"XX" },
                    ),
                ],
                crate::WriteOptions::new().with_write_all(true),
            )
            .unwrap_err();
        let first = client.read("/first").unwrap();
        client.close_batch(&mut files).unwrap();
        assert_eq!(first, b"changedl");
        assert_eq!(error.index(), Some(1));
        assert_eq!(
            crate::test_native_code(error.cause()),
            if closed { libc::EBADF } else { libc::EOVERFLOW } as u32
        );
        assert_eq!(
            vfsi_core::internal::errors::operation(error.cause()),
            Some("vwrite_native")
        );
        assert_eq!(
            vfsi_core::internal::errors::path(error.cause()),
            Some(Path::new("/second"))
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn nfs_child_projection_keeps_relative_root_and_checks_backend_parent() {
    use super::dispatch::project_nfs_child;

    let backend = Path::new("/remote/dir");
    let relative = Path::new("dir");
    let absolute = Path::new("/mount/dir");
    let child = backend.join("child");
    assert_eq!(
        project_nfs_child(backend, relative, &child).unwrap(),
        relative.join("child"),
    );
    assert_eq!(
        project_nfs_child(backend, absolute, &child).unwrap(),
        absolute.join("child"),
    );
    for path in [
        "/remote/dir",
        "/remote/other/child",
        "/remote/dir/nested/child",
    ] {
        assert!(project_nfs_child(backend, relative, Path::new(path)).is_err());
    }
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_dangling_symlink_keeps_kernel_routing() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let fixture = tempfile::tempdir_in(mount).unwrap();
    let client = Auto::new(fixture.path()).unwrap();
    require_live_direct_route(&client, Path::new("missing-target"));
    std::os::unix::fs::symlink("missing-target", fixture.path().join("link")).unwrap();

    for path in ["link", "link/", "link/."] {
        assert_eq!(client.route_for(path), AutoRoute::Posix);
    }
    assert_eq!(
        client.attrs("link").unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        client.symlink_attrs("link").unwrap().file_type(),
        crate::FileType::Symlink
    );
    require_live_direct_route(&client, Path::new("missing-target"));

    let preserve_link = crate::files::CopyOptions::new().with_follow_source_symlinks(false);
    client
        .vcopy(&[("link", "copied-link")], preserve_link)
        .unwrap()
        .remove(0)
        .unwrap();
    let mounted = client.resolve(Path::new("/"), &read_mounts(false));
    let Route::Nfs(connection) = mounted.route else {
        panic!("expected direct route for fixture");
    };
    // Use direct verification and cleanup for a name the kernel may have
    // cached as absent before the direct copy.
    let copy = mounted.path.join("copied-link");
    let target = connection.client.read_link(&copy);
    connection.client.remove_file(copy).unwrap();
    assert_eq!(target.unwrap(), Path::new("missing-target"));
    fs::write(fixture.path().join("file"), b"payload").unwrap();
    require_live_direct_route(&client, Path::new("file"));
    client
        .vcopy(&[("file", "link")], Default::default())
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(
        fs::read(fixture.path().join("missing-target")).unwrap(),
        b"payload"
    );

    fs::write(fixture.path().join("file"), b"second!").unwrap();
    for pair in [("file", "link"), ("link", "copied-target")] {
        client
            .vcopy(&[pair], Default::default())
            .unwrap()
            .remove(0)
            .unwrap();
    }
    assert_eq!(
        fs::read(fixture.path().join("missing-target")).unwrap(),
        b"second!"
    );
    assert_eq!(
        fs::read(fixture.path().join("copied-target")).unwrap(),
        b"second!"
    );
    fs::write(fixture.path().join("file"), b"payload").unwrap();

    // A host-existing absolute target can be missing beneath the client's
    // root. Only the rooted destination is used.
    let local = tempfile::tempdir().unwrap();
    let host_target = local.path().canonicalize().unwrap().join("target");
    fs::write(&host_target, b"host").unwrap();
    let rooted_target = fixture.path().join(host_target.strip_prefix("/").unwrap());
    fs::create_dir_all(rooted_target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&host_target, fixture.path().join("rooted-link")).unwrap();
    client
        .vcopy(&[("file", "rooted-link")], Default::default())
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(fs::read(&rooted_target).unwrap(), b"payload");
    assert_eq!(fs::read(&host_target).unwrap(), b"host");

    fs::create_dir(fixture.path().join("subdir")).unwrap();
    std::os::unix::fs::symlink("../relative-target", fixture.path().join("subdir/link")).unwrap();
    let absolute_target = format!("/vnfs-auto-root-relative-{}", std::process::id());
    let rooted_file = fixture.path().join(absolute_target.trim_start_matches('/'));
    fs::write(&rooted_file, b"original").unwrap();
    std::os::unix::fs::symlink(&absolute_target, fixture.path().join("absolute-link")).unwrap();
    std::os::unix::fs::symlink("chain-tail", fixture.path().join("chain-head")).unwrap();
    std::os::unix::fs::symlink("chain-target", fixture.path().join("chain-tail")).unwrap();
    let parent_target = format!("vnfs-auto-parent-target-{}", std::process::id());
    std::os::unix::fs::symlink(
        Path::new("..").join(&parent_target),
        fixture.path().join("root-parent-link"),
    )
    .unwrap();
    for target in [
        "subdir/link",
        "absolute-link",
        "chain-head",
        "root-parent-link",
    ] {
        client
            .vcopy(&[("file", target)], Default::default())
            .unwrap()
            .remove(0)
            .unwrap();
    }
    assert_eq!(
        fs::read(fixture.path().join("relative-target")).unwrap(),
        b"payload"
    );
    assert_eq!(fs::read(rooted_file).unwrap(), b"payload");
    assert_eq!(
        fs::read(fixture.path().join("chain-target")).unwrap(),
        b"payload"
    );
    assert_eq!(
        fs::read(fixture.path().join(parent_target)).unwrap(),
        b"payload"
    );

    let real = fixture.path().join("real");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink("real", fixture.path().join("alias")).unwrap();
    std::os::unix::fs::symlink("/real", fixture.path().join("rooted-alias")).unwrap();
    std::os::unix::fs::symlink("alias/chained", fixture.path().join("via-ancestor")).unwrap();
    for (from, to) in [
        ("file", "alias/destination"),
        ("rooted-alias/destination", "from-ancestor"),
        ("file", "via-ancestor"),
    ] {
        client
            .vcopy(&[(from, to)], Default::default())
            .unwrap()
            .remove(0)
            .unwrap();
    }
    assert_eq!(fs::read(real.join("destination")).unwrap(), b"payload");
    assert_eq!(fs::read(real.join("chained")).unwrap(), b"payload");
    assert_eq!(
        fs::read(fixture.path().join("from-ancestor")).unwrap(),
        b"payload"
    );
    std::os::unix::fs::symlink("destination", real.join("source-link")).unwrap();
    client
        .vcopy(&[("alias/source-link", "preserved-link")], preserve_link)
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(
        fs::read_link(fixture.path().join("preserved-link")).unwrap(),
        Path::new("destination")
    );

    let host_client = Auto::new("/").unwrap();
    let mount_table = read_mounts(false);
    let source = host_client.resolve(&fixture.path().join("file"), &mount_table);
    assert!(matches!(&source.route, Route::Nfs(_)));
    let off_mount = fixture.path().join("off-mount");
    std::os::unix::fs::symlink(local.path().join("missing"), &off_mount).unwrap();
    assert!(!host_client.mounted_copy_path_on(&off_mount, true, &mount_table, &source.route));
    let source_link = fixture.path().join("preserved-link");
    let existing = host_client
        .vcopy(&[(&source_link, &off_mount)], preserve_link)
        .unwrap()
        .remove(0)
        .unwrap_err();
    assert_eq!(existing.state(), vfsi_core::FailureState::NotAttempted);
    assert_eq!(existing.cause().kind(), io::ErrorKind::AlreadyExists);
    let cross_mount = host_client
        .vcopy(&[(&fixture.path().join("file"), &off_mount)], preserve_link)
        .unwrap()
        .remove(0)
        .unwrap_err();
    assert_eq!(cross_mount.cause().kind(), io::ErrorKind::Unsupported);
    assert_eq!(
        fs::read_link(&off_mount).unwrap(),
        local.path().join("missing")
    );
    let off_mount_ancestor = fixture.path().join("off-mount-ancestor");
    std::os::unix::fs::symlink(local.path(), &off_mount_ancestor).unwrap();
    let outside = off_mount_ancestor.join("missing");
    let origin = fixture.path().join("file");
    let result = host_client
        .vcopy(&[(&origin, &outside)], Default::default())
        .unwrap()
        .remove(0)
        .unwrap_err();
    assert_eq!(result.cause().kind(), io::ErrorKind::Unsupported);
    assert!(!local.path().join("missing").exists());
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_cross_device_transfer_replaces_file_through_open_handles() {
    use crate::directory::DepthLimit;
    use crate::helpers::{Existing, TransferOptions, copy_items, move_items};

    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let remote = tempfile::tempdir_in(mount).unwrap();
    let local = tempfile::tempdir().unwrap();
    let client = Auto::new("/").unwrap();
    let variants = [(true, None), (false, DepthLimit::new(1)), (false, None)];
    for (index, (copy, depth)) in variants.into_iter().enumerate() {
        let name = format!("source-{index}");
        let source = remote.path().join(&name);
        let target = local.path().join(&name);
        fs::write(&source, b"new payload").unwrap();
        fs::write(&target, b"previous").unwrap();

        require_live_direct_route(&client, &source);
        let resolved = client.resolve(&source, &read_mounts(false));
        let Route::Nfs(connection) = resolved.route else {
            panic!("expected direct route for source");
        };
        assert!(matches!(
            client.resolve(&target, &read_mounts(false)).route,
            Route::Posix
        ));
        let mut options = TransferOptions::new().with_existing(Existing::Replace);
        if let Some(depth) = depth {
            options = options.with_max_depth(depth);
        }
        let summary = if copy {
            copy_items(&client, &[&source], local.path(), options)
        } else {
            move_items(&client, &[&source], local.path(), options)
        }
        .unwrap();
        assert_eq!(summary.roots_renamed, 0);
        assert_eq!(summary.roots_removed, u64::from(!copy));
        assert_eq!(summary.bytes_copied, Some(11));
        assert_eq!(fs::read(&target).unwrap(), b"new payload");
        let source_attrs = connection.client.symlink_attrs(&resolved.path);
        if copy {
            assert!(source_attrs.unwrap().is_file());
        } else {
            assert_eq!(source_attrs.unwrap_err().kind(), io::ErrorKind::NotFound);
        }
    }

    // Direct OPEN can leave a negative kernel entry for its own new name.
    let direct_created = remote.path().join("direct-created");
    let same_route_destination = remote.path().join("direct-targets");
    fs::create_dir(&same_route_destination).unwrap();
    fs::write(same_route_destination.join("direct-created"), b"previous").unwrap();
    client.write(&direct_created, b"direct payload").unwrap();
    let direct_summary = copy_items(
        &client,
        &[&direct_created],
        &same_route_destination,
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(direct_summary.bytes_copied, None);
    assert_eq!(
        client
            .read(same_route_destination.join("direct-created"))
            .unwrap(),
        b"direct payload",
    );

    let local_source = local.path().join("local-created");
    let mixed_destination = local.path().join("mixed-targets");
    fs::create_dir(&mixed_destination).unwrap();
    fs::write(&local_source, b"local data").unwrap();
    fs::write(mixed_destination.join("local-created"), b"previous").unwrap();
    let mixed = copy_items(
        &client,
        &[&direct_created, &local_source],
        &mixed_destination,
        TransferOptions::new()
            .with_existing(Existing::Replace)
            .with_layout(crate::helpers::CopyLayout::Contents),
    )
    .unwrap();
    assert_eq!(mixed.files_copied, 2);
    assert_eq!(
        fs::read(mixed_destination.join("direct-created")).unwrap(),
        b"direct payload"
    );
    assert_eq!(
        fs::read(mixed_destination.join("local-created")).unwrap(),
        b"local data"
    );

    let alias = local.path().join("alias");
    std::os::unix::fs::symlink(remote.path(), &alias).unwrap();
    fs::create_dir(remote.path().join("same-mount")).unwrap();
    let source = remote.path().join("source-0");
    let directory = alias.join("same-mount");
    let target = directory.join("source-0");
    let mounts = read_mounts(false);
    assert!(matches!(
        client.resolve(&source, &mounts).route,
        Route::Nfs(_)
    ));
    assert!(matches!(
        client.resolve(&target, &mounts).route,
        Route::Posix
    ));
    let zero_read_budget = client.with_limits(crate::ResourceLimits::new().with_max_read_bytes(0));
    let summary = copy_items(
        &zero_read_budget,
        &[&source],
        &directory,
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap();
    assert_eq!(summary.bytes_copied, None);
    assert_eq!(fs::read(&target).unwrap(), b"new payload");

    let direct = remote.path().join("same-mount/source-0");
    let error = copy_items(
        &zero_read_budget,
        &[&direct],
        &directory,
        TransferOptions::new().with_existing(Existing::Replace),
    )
    .unwrap_err();
    assert_eq!(error.cause().kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fs::read(&direct).unwrap(), b"new payload");
    assert_eq!(fs::read(&target).unwrap(), b"new payload");

    if let Some(other_mount) = std::env::var_os("VFSI_AUTO_TEST_ALIAS_MOUNT") {
        let same_remote = PathBuf::from(other_mount).join(remote.path().file_name().unwrap());
        let second_alias = local.path().join("second-alias");
        std::os::unix::fs::symlink(same_remote, &second_alias).unwrap();
        let second_directory = second_alias.join("same-mount");
        let second_target = second_directory.join("source-0");
        let second_direct = PathBuf::from(std::env::var_os("VFSI_AUTO_TEST_ALIAS_MOUNT").unwrap())
            .join(remote.path().file_name().unwrap())
            .join("same-mount/source-0");
        let earlier_kernel = zero_read_budget.mounted.symlink_attrs(&direct).unwrap();
        require_live_direct_route(&zero_read_budget, &direct);
        require_live_direct_route(&zero_read_budget, &second_direct);
        let later_native = zero_read_budget.symlink_attrs(&second_direct).unwrap();
        assert_ne!(earlier_kernel.fs_obj_id(), later_native.fs_obj_id());
        let identity = zero_read_budget.compare_routed_objects(&[(
            (direct.as_path(), &earlier_kernel),
            (second_direct.as_path(), &later_native),
        )]);
        assert!(matches!(identity.as_slice(), [Ok(Some(true))]));
        let directory_alias = local.path().join("directory-alias");
        std::os::unix::fs::symlink(&second_directory, &directory_alias).unwrap();
        let alias_error = crate::helpers::copy_tree(
            &zero_read_budget,
            remote.path().join("same-mount"),
            directory_alias.join("never-created"),
            TransferOptions::new(),
        )
        .unwrap_err();
        assert_eq!(alias_error.kind(), io::ErrorKind::InvalidInput);
        assert!(!directory_alias.join("never-created").exists());
        assert_ne!(
            fs::metadata(&direct).unwrap().dev(),
            fs::metadata(&second_target).unwrap().dev()
        );
        for source in [&direct, &target] {
            let error = copy_items(
                &zero_read_budget,
                &[source],
                &second_directory,
                TransferOptions::new().with_existing(Existing::Replace),
            )
            .unwrap_err();
            assert_eq!(error.cause().kind(), io::ErrorKind::InvalidInput);
            assert_eq!(fs::read(&direct).unwrap(), b"new payload");
            assert_eq!(fs::read(&second_target).unwrap(), b"new payload");
        }
    }

    if let Some(readonly_mount) = std::env::var_os("VFSI_AUTO_TEST_READONLY_MOUNT") {
        let readonly_source = PathBuf::from(readonly_mount)
            .join(remote.path().file_name().unwrap())
            .join("same-mount/source-0");
        assert_ne!(
            fs::metadata(&direct).unwrap().dev(),
            fs::metadata(&readonly_source).unwrap().dev()
        );
        assert!(matches!(
            zero_read_budget
                .resolve(&readonly_source, &read_mounts(false))
                .route,
            Route::Posix
        ));
        let error = copy_items(
            &zero_read_budget,
            &[&readonly_source],
            &directory,
            TransferOptions::new().with_existing(Existing::Replace),
        )
        .unwrap_err();
        // The mount guard recognizes the same NFS object through the
        // read-only kernel route before replacement can remove a name.
        assert_eq!(error.index(), Some(0));
        assert_eq!(error.cause().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            vfsi_core::internal::errors::operation(error.cause()),
            Some("transfer mount ancestry")
        );
        assert_eq!(
            vfsi_core::internal::errors::path(error.cause()),
            Some(target.as_path())
        );
        assert_eq!(fs::read(direct).unwrap(), b"new payload");
    }
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_transfer_identity_queries_batch_existing_direct_paths() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let fixture = tempfile::tempdir_in(mount).unwrap();
    let client = Auto::new("/").unwrap();
    let mut paths = Vec::new();
    for index in 0..16 {
        let source = fixture.path().join(format!("source-{index}"));
        let destination = fixture.path().join(format!("destination-{index}"));
        fs::write(&source, b"source").unwrap();
        if index % 2 == 0 {
            fs::hard_link(&source, &destination).unwrap();
        } else {
            fs::write(&destination, b"destination").unwrap();
        }
        paths.extend([source, destination]);
    }
    for path in &paths {
        require_live_direct_route(&client, path);
    }
    let attrs = client
        .getattrs_all(
            &paths,
            crate::AttrsOptions::new().with_fields(crate::Attributes::identity()),
        )
        .unwrap();
    let observed: Vec<_> = paths
        .iter()
        .zip(&attrs)
        .map(|(path, attrs)| (path.as_path(), attrs))
        .collect();
    let (pairs, remainder) = observed.as_chunks::<2>();
    assert!(remainder.is_empty());
    let pairs: Vec<_> = pairs.iter().map(|[left, right]| (*left, *right)).collect();

    let calls = || vfsi_nfs::compound::thread_compound_stats().0;
    let start = calls();
    let scalar: Vec<_> = pairs
        .iter()
        .flat_map(|pair| client.compare_routed_objects(std::slice::from_ref(pair)))
        .map(Result::unwrap)
        .collect();
    let scalar_calls = calls() - start;
    let start = calls();
    let vector: Vec<_> = client
        .compare_routed_objects(&pairs)
        .into_iter()
        .map(Result::unwrap)
        .collect();
    let vector_calls = calls() - start;
    let expected: Vec<_> = (0..pairs.len()).map(|index| Some(index % 2 == 0)).collect();
    assert_eq!(scalar, expected);
    assert_eq!(vector, expected);
    assert!(
        vector_calls > 0 && vector_calls < scalar_calls,
        "{vector_calls} vector vs {scalar_calls} scalar compounds"
    );
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_relative_directory_pages_preserve_public_child_paths() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let fixture = tempfile::tempdir_in(mount).unwrap();
    fs::create_dir_all(fixture.path().join("dir/subdir")).unwrap();
    fs::write(fixture.path().join("dir/subdir/data"), b"data").unwrap();
    fs::write(fixture.path().join("dir/file"), b"file").unwrap();
    let client = Auto::new(fixture.path()).unwrap();
    require_live_direct_route(&client, Path::new("dir"));
    let mut entries: Vec<_> = client
        .read_dir("dir")
        .unwrap()
        .into_iter()
        .map(|entry| entry.into_parts().0)
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        [PathBuf::from("dir/file"), PathBuf::from("dir/subdir")]
    );

    let tree = client
        .read_dirs_with_options(&["dir"], ListDirOptions::new().with_recursive(true))
        .unwrap()
        .remove(0);
    assert!(tree.iter().any(|listing| {
        listing.path == Path::new("dir/subdir")
            && listing
                .entries
                .iter()
                .any(|entry| entry.path() == Path::new("dir/subdir/data"))
    }));
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_recursive_paging_follows_nfs_ancestors_only() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let fixture = tempfile::tempdir_in(mount).unwrap();
    fs::create_dir(fixture.path().join("dir")).unwrap();
    for name in ["first", "second"] {
        fs::write(fixture.path().join("dir").join(name), b"data").unwrap();
    }
    std::os::unix::fs::symlink(".", fixture.path().join("alias")).unwrap();
    std::os::unix::fs::symlink("dir", fixture.path().join("final")).unwrap();

    let fs = Auto::new(fixture.path()).unwrap();
    require_live_direct_route(&fs, Path::new("/"));
    let root = fs.resolve(Path::new("/"), &read_mounts(false));
    let Route::Nfs(connection) = root.route else {
        panic!("expected direct route for fixture");
    };
    let paths = [root.path.join("alias/dir"), root.path.join("final")];
    let recursive = ListDirOptions::new().with_recursive(true);
    let mut visited = Vec::new();
    let results = connection
        .client
        .vlistdirs(&paths, recursive, |index, page| {
            assert_eq!(index, 0);
            assert_eq!(page.path, paths[0]);
            visited.extend(page.entries.into_iter().map(|entry| entry.into_parts().0));
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    assert!(results[0].is_ok(), "{:?}", results[0]);
    visited.sort();
    let expected = [paths[0].join("first"), paths[0].join("second")];
    assert_eq!(visited, expected);
    assert_eq!(
        results[1].as_ref().unwrap_err().cause().kind(),
        io::ErrorKind::NotADirectory
    );

    // Exercise the NFS open policy independently of traversal's metadata
    // preflight and force a continuation for the permitted ancestor link.
    let symlinks = vfsi_sync::backend::DirectorySymlinkPolicy::FollowParents;
    let mut results = connection
        .client
        .read_dir_pages_with_fields(
            &[paths[0].as_path(), paths[1].as_path()],
            crate::Attributes::MODE,
            vec![None, None],
            1,
            10,
            symlinks,
        )
        .unwrap();
    let rejected = results
        .pop()
        .unwrap()
        .err()
        .expect("final symlink must be rejected");
    assert_eq!(rejected.cause().kind(), io::ErrorKind::NotADirectory);
    let (mut page, next, _) = results.pop().unwrap().unwrap();
    assert_eq!(page.entries.len(), 1);
    let (second, _, _) = connection
        .client
        .read_dir_pages_with_fields(
            &[paths[0].as_path()],
            crate::Attributes::MODE,
            vec![Some(next.expect("two entries require a second page"))],
            1,
            9,
            symlinks,
        )
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(second.entries.len(), 1);
    page.entries.extend(second.entries);
    let mut paged: Vec<_> = page
        .entries
        .into_iter()
        .map(|entry| entry.into_parts().0)
        .collect();
    paged.sort();
    assert_eq!(paged, expected);
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_nfs_mount_uses_direct_connection() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
        .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
    let client = Auto::new("/").unwrap();
    let mount = PathBuf::from(mount);
    let mut route = client.route_for(&mount);
    // A just-mounted kernel NFS client can briefly return EREMOTEIO for
    // the root stat. Auto safely falls back for that attempt; retry the
    // read-only route probe after the mount has settled.
    for delay_ms in [5, 20] {
        if matches!(&route, AutoRoute::DirectNfs { .. }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        route = client.route_for(&mount);
    }
    assert!(
        matches!(&route, AutoRoute::DirectNfs { .. }),
        "Auto chose {route:?}"
    );
    assert!(client.attrs(&mount).unwrap().is_dir());
    let unique = format!(
        "vnfs-auto-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    );
    let first = mount.join(format!("{unique}-1"));
    let second = mount.join(format!("{unique}-2"));
    let local = std::env::temp_dir().join(format!("{unique}-local"));
    assert_eq!(client.route_for(&local), AutoRoute::Posix);
    let requests = [first.as_path(), second.as_path(), local.as_path()].map(|path| {
        OpenOp::new(
            path,
            OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE_NEW,
        )
    });
    let files = client.open_batch(&requests).unwrap();
    assert!(matches!(files[0].route(), AutoRoute::DirectNfs { .. }));
    assert!(matches!(files[1].route(), AutoRoute::DirectNfs { .. }));
    assert_eq!(files[2].route(), AutoRoute::Posix);
    client
        .write_batch(
            &[
                crate::WriteOp::at(&files[0], 0, b"first"),
                crate::WriteOp::at(&files[1], 0, b"second"),
                crate::WriteOp::at(&files[2], 0, b"local"),
            ],
            Default::default(),
        )
        .unwrap();
    let reads = client
        .read_batch(
            [
                crate::ReadOp::range(&files[0], 0, 5),
                crate::ReadOp::range(&files[1], 0, 6),
                crate::ReadOp::range(&files[2], 0, 5),
            ],
            crate::ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(reads[0].data().unwrap(), b"first");
    assert_eq!(reads[1].data().unwrap(), b"second");
    assert_eq!(reads[2].data().unwrap(), b"local");
    let mut buffers = [[0_u8; 8]; 3];
    let [first_buffer, second_buffer, local_buffer] = &mut buffers;
    let reads = client
        .read_batch(
            [
                crate::ReadOp::into(&files[0], 0, first_buffer),
                crate::ReadOp::into(&files[1], 0, second_buffer),
                crate::ReadOp::into(&files[2], 0, local_buffer),
            ],
            Default::default(),
        )
        .unwrap();
    assert_eq!(
        reads
            .iter()
            .map(|read| read.bytes_read())
            .collect::<Vec<_>>(),
        [5, 6, 5]
    );
    assert!(reads[..2].iter().all(|read| read.eof()));
    // NFS supplies an explicit EOF bit. A positive short POSIX read does not;
    // only a subsequent zero-byte read proves the local file is exhausted.
    assert!(!reads[2].eof());
    let probe = client
        .read_batch(
            [crate::ReadOp::into(&files[2], 5, local_buffer)],
            Default::default(),
        )
        .unwrap();
    assert_eq!(probe[0].bytes_read(), 0);
    assert!(probe[0].eof());
    assert_eq!(&buffers[0][..5], b"first");
    assert_eq!(&buffers[1][..6], b"second");
    assert_eq!(&buffers[2][..5], b"local");
    client
        .write_batch(
            &[
                crate::WriteOp::at(&files[0], 0, b"FIRST"),
                crate::WriteOp::at(&files[1], 0, b"SECOND"),
                crate::WriteOp::at(&files[2], 0, b"LOCAL"),
            ],
            crate::WriteOptions::new().with_write_all(true),
        )
        .unwrap();
    let mut files = files;
    client.close_batch(&mut files).unwrap();
    client.close_batch(&mut files).unwrap();
    assert!(files.iter().all(AutoFile::is_closed));
    let paths = [first.as_path(), second.as_path(), local.as_path()];
    assert_eq!(
        client.read_files(&paths).unwrap(),
        [b"FIRST".to_vec(), b"SECOND".to_vec(), b"LOCAL".to_vec()]
    );
    assert!(
        client
            .read_files_with_options(
                &paths,
                crate::ReadOptions::new()
                    .with_max_total_bytes(std::num::NonZeroUsize::new(15).unwrap())
            )
            .is_err()
    );
    let existing = client
        .open_batch(&[
            OpenOp::new(
                &first,
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
            ),
            OpenOp::new(
                &second,
                OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE,
            ),
        ])
        .unwrap();
    assert!(
        existing
            .iter()
            .all(|file| matches!(file.route(), AutoRoute::DirectNfs { .. }))
    );
    client.close_files(existing).unwrap();
    let missing = std::env::temp_dir().join(format!("{unique}-missing"));
    let error = client
        .open_batch(&[
            OpenOp::new(&first, OpenFlags::READ),
            OpenOp::new(&second, OpenFlags::READ),
            OpenOp::new(&missing, OpenFlags::READ),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(2));
    let link = mount.join(format!("{unique}-symlink"));
    std::os::unix::fs::symlink(&local, &link).unwrap();
    let linked = client.open(&link).unwrap();
    assert_eq!(linked.route(), AutoRoute::Posix);
    linked.close().unwrap();
    let entries = client.read_dir(&mount).unwrap();
    assert!(entries.iter().any(|entry| entry.path() == first));
    client.remove_file(&link).unwrap();
    client.remove_file(&first).unwrap();
    client.remove_file(&second).unwrap();
    client.remove_file(&local).unwrap();
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_changed_auto_limits_apply_to_cached_connections_and_open_handles() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
        .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
    let path = Path::new(&mount).join(format!(
        "auto-limits-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    fs::write(&path, b"abcdef").unwrap();
    let mut outcomes = Vec::new();
    for reopen in [false, true] {
        let client = Auto::new("/")
            .unwrap()
            .with_limits(ResourceLimits::new().with_max_read_bytes(4));
        let file = client.open(&path).unwrap();
        assert!(matches!(file.route(), AutoRoute::DirectNfs { .. }));
        let Route::Nfs(pinned) = &file.route else {
            panic!("expected NFS handle")
        };
        let cached = client
            .connections
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(pinned, &cached));
        assert_eq!(pinned.spec.server.as_ptr(), cached.spec.server.as_ptr());
        let client = client.with_limits(ResourceLimits::new().with_max_read_bytes(8));
        let updated = client
            .connections
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        assert!(!Arc::ptr_eq(&cached, &updated));
        assert!(Route::Nfs(cached.clone()).same_backend(&Route::Nfs(updated.clone())));
        assert_eq!(cached.client.limits().max_read_bytes(), 4);
        assert_eq!(updated.client.limits().max_read_bytes(), 8);
        assert_eq!(cached.spec, updated.spec);
        assert_eq!(cached.credentials, updated.credentials);
        let file = if reopen {
            file.close().unwrap();
            client.open(&path).unwrap()
        } else {
            file
        };
        let mut buffer = [0; 6];
        outcomes.push(
            client
                .read_batch(
                    [crate::ReadOp::into(&file, 0, &mut buffer)],
                    Default::default(),
                )
                .map(|results| {
                    assert_eq!(results[0].bytes_read(), 6);
                    assert_eq!(&buffer, b"abcdef");
                }),
        );
        let client = client.with_limits(ResourceLimits::new().with_max_read_bytes(3));
        buffer.fill(0xff);
        assert_eq!(
            client
                .read_batch(
                    [crate::ReadOp::into(&file, 0, &mut buffer)],
                    Default::default()
                )
                .unwrap_err()
                .cause()
                .kind(),
            crate::ErrorKind::FileTooLarge
        );
        assert_eq!(buffer, [0xff; 6]);
        file.close().unwrap();
    }
    fs::remove_file(path).unwrap();
    assert!(
        outcomes.iter().all(Result::is_ok),
        "old-handle and reopened-handle results: {outcomes:?}"
    );
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_write_allv_isolates_invalid_later_requests_across_routes() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
        .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
    let unique = format!(
        "auto-write-preflight-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    );
    let client = Auto::new("/").unwrap();
    let mut observations = Vec::new();
    for remote_first in [false, true] {
        // Cover overflow, a closed nonempty request, and a closed empty
        // request. An empty payload must not bypass handle validation.
        for invalid in 0..3 {
            // Do not reuse a pathname removed through the kernel while a
            // direct NFS client still caches its former filehandle.
            let case = format!("{unique}-{remote_first}-{invalid}");
            let remote = Path::new(&mount).join(&case);
            let local = std::env::temp_dir().join(&case);
            fs::write(&remote, b"original").unwrap();
            fs::write(&local, b"original").unwrap();
            require_live_direct_route(&client, &remote);
            let paths = if remote_first {
                [&remote, &local]
            } else {
                [&local, &remote]
            };
            let mut files = client
                .open_batch(
                    &paths.map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE)),
                )
                .unwrap();
            assert_eq!(
                matches!(files[0].route(), AutoRoute::DirectNfs { .. }),
                remote_first
            );
            assert_eq!(
                matches!(files[1].route(), AutoRoute::DirectNfs { .. }),
                !remote_first
            );
            if invalid != 0 {
                files[1].try_close().unwrap();
            }
            let payload: &[u8] = if invalid == 2 { b"" } else { b"XX" };
            let error = client
                .write_batch(
                    &[
                        crate::WriteOp::at(&files[0], 0, b"changed"),
                        crate::WriteOp::at(
                            &files[1],
                            if invalid == 0 { u64::MAX } else { 0 },
                            payload,
                        ),
                    ],
                    crate::WriteOptions::new().with_write_all(true),
                )
                .unwrap_err();
            let mut first = [0; 8];
            assert_eq!(
                client.std_io(&files[0]).read(&mut first).unwrap(),
                first.len()
            );
            let second = client.read(paths[1]).unwrap();
            observations.push((
                remote_first,
                invalid,
                error,
                first,
                second,
                paths[1].to_path_buf(),
            ));
            client.close_batch(&mut files).unwrap();
            fs::remove_file(&remote).unwrap();
            fs::remove_file(&local).unwrap();
        }
    }
    for (remote_first, invalid, error, first, second, failed_path) in observations {
        assert_eq!(
            &first, b"changedl",
            "healthy cohort must execute: remote_first={remote_first}, invalid={invalid}"
        );
        assert_eq!(second, b"original");
        assert_eq!(error.index(), Some(1));
        assert_eq!(
            crate::test_native_code(error.cause()),
            if invalid == 0 {
                libc::EOVERFLOW
            } else {
                libc::EBADF
            } as u32
        );
        assert_eq!(
            vfsi_core::internal::errors::operation(error.cause()),
            Some("vwrite_native")
        );
        assert_eq!(
            vfsi_core::internal::errors::path(error.cause()),
            Some(failed_path.as_path())
        );
    }
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_auto_traversal_charges_public_path_bytes() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT")
        .expect("VFSI_AUTO_TEST_MOUNT is required for this ignored integration test");
    let root = Path::new(&mount).join(format!(
        "auto-quota-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a"), b"a").unwrap();
    fs::write(root.join("b"), b"b").unwrap();
    let client = Auto::new("/").unwrap();
    require_live_direct_route(&client, &root);
    let route = client.resolve(&root, &read_mounts(false));
    let Route::Nfs(connection) = &route.route else {
        panic!("expected direct NFS");
    };
    let listings = connection
        .client
        .read_dirs_with_options(
            &[&route.path],
            crate::ListDirOptions::unlimited()
                .with_fields(crate::Attributes::stat())
                .with_recursive(true),
        )
        .map(|mut trees| trees.remove(0))
        .unwrap();
    let backend_entries: usize = listings
        .iter()
        .flat_map(|listing| &listing.entries)
        .map(|entry| entry.path().as_os_str().len())
        .sum();
    let backend_total = backend_entries
        + listings
            .iter()
            .map(|listing| listing.path.as_os_str().len())
            .sum::<usize>();
    let public_entries: usize = listings
        .iter()
        .flat_map(|listing| &listing.entries)
        .map(|entry| {
            // The fixture has no descendants under its root other than a/b.
            root.join(entry.path().file_name().unwrap())
                .as_os_str()
                .len()
        })
        .sum();
    let public_total = public_entries + root.as_os_str().len();
    assert!(public_entries > backend_entries && public_total > backend_total);
    let walk = client
        .read_dirs_with_options(
            &[&root],
            crate::ListDirOptions::new()
                .with_max_path_bytes(backend_total)
                .with_fields(crate::Attributes::stat())
                .with_recursive(true),
        )
        .map(|mut trees| trees.remove(0));
    let mut dir_bytes = 0;
    let dir = client.listdir(
        &root,
        ListDirOptions::new().with_max_path_bytes(backend_entries),
        |entry| {
            dir_bytes += entry.entry.path().as_os_str().len();
            Ok(vfsi_core::api::WalkControl::Continue)
        },
    );
    let mut tree_bytes = root.as_os_str().len();
    let tree = client.listdir(
        &root,
        crate::ListDirOptions::new()
            .with_recursive(true)
            .with_max_path_bytes(backend_total),
        |entry| {
            tree_bytes += entry.entry.path().as_os_str().len();
            Ok(vfsi_core::api::WalkControl::Continue)
        },
    );
    // Exact public budgets remain usable, including both callbacks.
    assert!(
        client
            .read_dirs_with_options(
                &[&root],
                crate::ListDirOptions::new()
                    .with_max_path_bytes(public_total)
                    .with_fields(crate::Attributes::stat())
                    .with_recursive(true)
            )
            .map(|mut trees| trees.remove(0))
            .is_ok()
    );
    assert!(
        client
            .listdir(
                &root,
                ListDirOptions::new().with_max_path_bytes(public_entries),
                |_| Ok(vfsi_core::api::WalkControl::Continue)
            )
            .is_ok()
    );
    assert_eq!(
        client
            .listdir(
                &root,
                crate::ListDirOptions::new()
                    .with_recursive(true)
                    .with_max_path_bytes(public_total),
                |_| Ok(vfsi_core::api::WalkControl::Continue)
            )
            .unwrap(),
        crate::TraversalCompletion::Complete
    );
    // Clean up through the client that traversed this directory instead
    // of mixing its direct NFS view with kernel directory caches.
    client.remove_dir_all(&root).unwrap();
    assert!(
        walk.is_err() && dir.is_err() && tree.is_err(),
        "walk={walk:?}; dir={dir:?}; tree={tree:?}"
    );
    assert!(dir_bytes <= backend_entries && tree_bytes <= backend_total);
}

#[test]
#[ignore = "requires VFSI_AUTO_TEST_MOUNT pointing to a direct NFS mount"]
fn live_directory_open_errors_report_application_operands() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let root = tempfile::tempdir_in(mount).unwrap();
    // Kernel-created fixtures are visible to Auto's host-side route discovery.
    std::fs::create_dir(root.path().join("first")).unwrap();
    std::fs::write(root.path().join("file"), b"keep").unwrap();
    // Use the stable host root; freshly created kernel NFS directories can
    // briefly fail metadata probes while the server fixture settles.
    let fs = crate::Auto::new("/").unwrap();
    let first = root.path().join("first");
    let missing = root.path().join("missing");
    let file = root.path().join("file");
    for path in [&first, &missing, &file] {
        require_live_direct_route(&fs, path);
    }
    let error = fs.open_dirs(&[&first, &missing]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(
        vfsi_core::internal::errors::path(error.cause()),
        Some(missing.as_path())
    );
    assert_eq!(
        vfsi_core::internal::errors::operation(error.cause()),
        Some("vopen_dirs")
    );
    assert_eq!(error.cause().kind(), crate::ErrorKind::NotFound);
    fs.drain_cleanup().unwrap();

    let error = fs.open_dirs(&[&file]).unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(
        vfsi_core::internal::errors::path(error.cause()),
        Some(file.as_path())
    );
    assert_eq!(
        vfsi_core::internal::errors::operation(error.cause()),
        Some("vopen_dirs")
    );
    assert_eq!(error.cause().kind(), crate::ErrorKind::NotADirectory);
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    fs.open_dirs(&[&first]).unwrap().remove(0).close().unwrap();
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_remove_contents_respects_nested_mounts() {
    // The fixture is an NFS directory with a separately mounted child.
    // Its remote child contains hidden data that kernel routing cannot see.
    let root = std::env::var("VFSI_AUTO_TEST_NESTED_ROOT")
        .expect("VFSI_AUTO_TEST_NESTED_ROOT is required for this ignored integration test");
    let client = Auto::new("/").unwrap();
    require_live_direct_route(&client, Path::new(&root));
    let route = client.resolve(Path::new(&root), &read_mounts(false));
    let Route::Nfs(connection) = route.route else {
        panic!("expected parent direct route");
    };
    let hidden = route.path.join("child/hidden");
    assert_eq!(connection.client.read(&hidden).unwrap(), b"preserve\n");
    let result = client.remove_dir_contents(&root);
    let preserved = connection.client.read(&hidden);
    assert!(
        result.is_err(),
        "kernel removal of a mounted child must fail"
    );
    assert_eq!(preserved.unwrap(), b"preserve\n");
}

#[test]
#[ignore = "requires configured NFS mount fixtures"]
fn live_file_bind_mount_uses_covering_mount() {
    let path = std::env::var("VFSI_AUTO_TEST_BIND")
        .expect("VFSI_AUTO_TEST_BIND is required for this ignored integration test");
    let client = Auto::new("/").unwrap();
    let file = client.open(&path).unwrap();
    assert_eq!(file.route(), AutoRoute::Posix);
    let mut contents = String::new();
    client.std_io(&file).read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "bind source\n");
    file.close().unwrap();
}

#[test]
#[ignore = "requires VFSI_AUTO_TEST_MOUNT pointing to a direct NFS mount"]
fn live_entry_mutations_keep_final_symlinks_on_their_parent_route() {
    let mount = PathBuf::from(std::env::var("VFSI_AUTO_TEST_MOUNT").expect("mount fixture"));
    let fixture = tempfile::Builder::new()
        .prefix(".vfsi-entry-routing-")
        .tempdir_in(&mount)
        .unwrap();
    let root = fixture.path();
    std::fs::write(root.join("target"), b"keep").unwrap();
    std::fs::write(root.join("regular"), b"replacement").unwrap();
    std::os::unix::fs::symlink("target", root.join("link")).unwrap();
    std::os::unix::fs::symlink("target", root.join("replace-link")).unwrap();
    let client = Auto::new("/").unwrap();
    require_live_direct_route(&client, root);
    let resolved = client.resolve(root, &routing::read_mounts(false));
    let Route::Nfs(connection) = resolved.route else {
        panic!("direct route required")
    };
    let remote = &connection.client;
    let base = resolved.path;
    struct Cleanup<'a>(&'a vfsi_sync::FsClient<vfsi_nfs::NfsVecFs>, PathBuf);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_dir_all(&self.1);
        }
    }
    // Use direct cleanup even on assertion failure: kernel negative dentries
    // do not observe names created through direct protocol operations.
    let _cleanup = Cleanup(remote, base.clone());

    let outcomes = client
        .vhardlink(&[
            (root.join("link"), root.join("linked")),
            (root.join("regular"), root.join("regular-linked")),
        ])
        .unwrap();
    assert!(outcomes[1].is_ok(), "a healthy sibling must still succeed");
    assert!(
        outcomes[0].is_ok(),
        "same-mount symlink link failed: {:?}",
        outcomes[0]
    );
    assert!(
        remote
            .symlink_attrs(base.join("linked"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        remote.read_link(base.join("linked")).unwrap(),
        Path::new("target")
    );

    let outcomes = client
        .vrename(
            &[
                (root.join("link"), root.join("moved")),
                (root.join("regular"), root.join("replace-link")),
            ],
            crate::RenameMode::Replace,
        )
        .unwrap();
    assert!(
        outcomes.iter().all(Result::is_ok),
        "same-mount entry renames failed: {outcomes:?}"
    );
    assert!(
        remote
            .symlink_attrs(base.join("moved"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        remote.read(base.join("replace-link")).unwrap(),
        b"replacement"
    );
    assert_eq!(remote.read(base.join("target")).unwrap(), b"keep");
}

#[test]
#[ignore = "requires VFSI_AUTO_TEST_MOUNT pointing to a direct NFS mount"]
fn live_mount_alias_clients_share_identity_and_cross_routes_never_use_kernel_copy() {
    let mount =
        PathBuf::from(std::env::var("VFSI_AUTO_TEST_MOUNT").expect("mount fixture is required"));
    let client = Auto::new("/").unwrap();
    require_live_direct_route(&client, &mount);
    let mounts = routing::read_mounts(false);
    let original = client.resolve(&mount, &mounts);
    let Route::Nfs(connection) = original.route else {
        panic!("direct route required")
    };
    let mut alias_spec = connection.spec.clone();
    alias_spec.id = u64::MAX;
    let alias = client.connection(&alias_spec).unwrap();
    assert!(Route::Nfs(connection.clone()).same_backend(&Route::Nfs(alias)));
    let fixture = tempfile::tempdir().unwrap();
    let source = mount.join(format!(".vfsi-route-identity-{}", std::process::id()));
    let destination = fixture.path().join("destination");
    client.write(&source, b"original").unwrap();
    assert!(
        client
            .copies_all(&[(&source, &destination)], crate::CopyOptions::default())
            .unwrap_err()
            .cause()
            .kind()
            == std::io::ErrorKind::Unsupported
    );
    assert!(!destination.exists());
    assert_eq!(
        crate::test_native_code(
            client
                .hardlinks_all(&[(&source, &destination)])
                .unwrap_err()
                .cause()
        ),
        libc::EXDEV as u32
    );
    assert_eq!(
        crate::test_native_code(
            client
                .renames_all(&[(&source, &destination)], Default::default())
                .unwrap_err()
                .cause()
        ),
        libc::EXDEV as u32
    );
    client.remove_file(&source).unwrap();
}

#[test]
#[ignore = "requires VFSI_AUTO_TEST_MOUNT pointing to a direct NFS mount"]
fn live_vector_open_retains_each_mount_alias_route() {
    let mount =
        PathBuf::from(std::env::var("VFSI_AUTO_TEST_MOUNT").expect("mount fixture is required"));
    let client = Auto::new("/").unwrap();
    require_live_direct_route(&client, &mount);
    let Route::Nfs(connection) = client.resolve(&mount, &routing::read_mounts(false)).route else {
        panic!("direct route required")
    };
    // Supply distinct already-resolved mount aliases to the production dispatch
    // path. Both use the live client, without requiring a second kernel mount.
    let mut alias_spec = connection.spec.clone();
    alias_spec.id = u64::MAX;
    alias_spec.mount_point = PathBuf::from("/vnfs-test-mount-alias");
    let alias = Arc::new(routing::NfsConnectionState {
        spec: alias_spec,
        client: connection.client.clone(),
        identity: connection.identity.clone(),
        credentials: connection.credentials.clone(),
    });
    let original_route = Route::Nfs(connection);
    let alias_route = Route::Nfs(alias);
    assert!(original_route.same_backend(&alias_route));
    assert_ne!(original_route.public(), alias_route.public());

    let remote_fixture = tempfile::tempdir_in(&mount).unwrap();
    let remote = remote_fixture.path().join("file");
    client.write(&remote, b"route").unwrap();
    let backend_path = Path::new("/").join(remote.strip_prefix(&mount).unwrap());
    let alias_path = Path::new("/vnfs-test-mount-alias").join(remote.strip_prefix(&mount).unwrap());
    let local_fixture = tempfile::tempdir().unwrap();
    let local = local_fixture.path().join("file");
    fs::write(&local, b"route").unwrap();

    // Include a POSIX prefix and another cohort so start + index is exercised,
    // then reverse alias order in the second NFS cohort.
    let public_paths = [&local, &remote, &alias_path, &local, &alias_path, &remote];
    let routes = [
        Route::Posix,
        original_route.clone(),
        alias_route.clone(),
        Route::Posix,
        alias_route,
        original_route,
    ];
    let requests: Vec<_> = public_paths
        .iter()
        .map(|path| OpenOp::new(path, OpenFlags::READ))
        .collect();
    let resolved: Vec<_> = routes
        .into_iter()
        .zip(public_paths)
        .map(|(route, path)| Resolved {
            path: match &route {
                Route::Posix => path.to_path_buf(),
                Route::Nfs(_) => backend_path.clone(),
            },
            route,
        })
        .collect();
    let files: Vec<_> = client
        .vopen_resolved(&requests, &resolved)
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(files.len(), requests.len());
    for (index, file) in files.iter().enumerate() {
        assert_eq!(file.path(), public_paths[index]);
        assert_eq!(file.route(), resolved[index].route.public(), "file {index}");
        if let Route::Nfs(expected) = &resolved[index].route {
            let Route::Nfs(actual) = &file.route else {
                panic!("NFS file {index} lost its route")
            };
            assert!(Arc::ptr_eq(actual, expected));
            assert!(format!("{file:?}").contains(expected.spec.mount_point.to_str().unwrap()));
        }
    }
    let reads = client
        .read_batch(
            files.iter().map(|file| crate::ReadOp::range(file, 0, 5)),
            Default::default(),
        )
        .unwrap();
    assert!(
        reads
            .iter()
            .all(|read| read.data() == Some(b"route".as_slice()))
    );
    client.close_files(files).unwrap();
    client.remove_file(&remote).unwrap();
}

#[test]
#[ignore = "requires VFSI_AUTO_TEST_MOUNT pointing to a direct NFS mount"]
fn live_remove_continues_after_errors_across_routes() {
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let remote = tempfile::tempdir_in(mount).unwrap();
    let local = tempfile::tempdir().unwrap();
    let client = Auto::new("/").unwrap();
    require_live_direct_route(&client, remote.path());
    let blocked = remote.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("child"), b"keep").unwrap();
    let local_file = local.path().join("local");
    assert_eq!(client.route_for(&local_file), AutoRoute::Posix);
    require_live_direct_route(&client, &blocked);
    for continue_on_error in [false, true] {
        fs::write(&local_file, b"local").unwrap();
        let last = remote.path().join("last");
        fs::write(&last, b"last").unwrap();
        let outcomes = client
            .vremove(
                &[&blocked, &local_file, &last],
                crate::RemoveMode::Entry,
                crate::RemoveOptions::new().with_continue_on_error(continue_on_error),
            )
            .unwrap();
        assert_eq!(outcomes.len(), 3);
        let error = outcomes[0].as_ref().unwrap_err();

        assert_eq!(error.cause().kind(), std::io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(error.state(), vfsi_core::FailureState::Failed);
        if continue_on_error {
            // Generic local removal genuinely rejects nondefault options.
            let error = outcomes[1].as_ref().unwrap_err();
            assert_eq!(error.state(), vfsi_core::FailureState::NotAttempted);
            assert_eq!(error.cause().kind(), std::io::ErrorKind::Unsupported);
            assert!(local_file.exists());
        } else {
            assert!(outcomes[1].is_ok());
            assert!(!local_file.exists());
        }
        assert!(
            outcomes[2].is_ok(),
            "neither failed root may suppress a later NFS route"
        );
        assert_eq!(fs::read(blocked.join("child")).unwrap(), b"keep");
        assert_eq!(
            client.attrs(&last).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

#[cfg(feature = "test-faults")]
#[test]
#[ignore = "requires VFSI_AUTO_TEST_MOUNT pointing to a direct NFS mount"]
fn live_remove_keeps_each_route_error_without_replay() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let mount = std::env::var("VFSI_AUTO_TEST_MOUNT").expect("NFS mount fixture");
    let remote = tempfile::tempdir_in(&mount).unwrap();
    let local = tempfile::tempdir().unwrap();
    let client = Auto::new("/").unwrap();
    require_live_direct_route(&client, remote.path());
    let connection = client
        .connections
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let mut backend = vfsi_nfs::NfsClientBuilder::from_mount(&connection.spec.mount_point)
        .unwrap()
        .connect()
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRemoveType { index: 0 },
        vfsi_core::internal::errors::transport(
            vfsi_core::TransportKind::Other,
            "injected removal transport failure",
        ),
    ));
    backend.set_fault_injector(script.clone());
    client.connections.lock().unwrap().insert(
        connection.spec.id,
        Arc::new(routing::NfsConnectionState {
            client: FsClient::new(backend),
            ..(*connection).clone()
        }),
    );
    let local_file = local.path().join("local");
    let remote_file = remote.path().join("remote");
    fs::write(&local_file, b"keep").unwrap();
    fs::write(&remote_file, b"keep").unwrap();
    let outcomes = client
        .vremove(
            &[&local_file, &remote_file],
            crate::RemoveMode::Entry,
            crate::RemoveOptions::new().with_continue_on_error(true),
        )
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    let local_error = outcomes[0].as_ref().unwrap_err();
    assert_eq!(local_error.state(), vfsi_core::FailureState::NotAttempted);
    assert_eq!(local_error.cause().kind(), std::io::ErrorKind::Unsupported);
    let remote_error = outcomes[1].as_ref().unwrap_err();
    assert!(vfsi_core::transport_kind(remote_error.cause()).is_some());
    assert_eq!(remote_error.state(), vfsi_core::FailureState::NotAttempted);
    assert!(script.is_consumed());
    assert_eq!(client.read(&remote_file).unwrap(), b"keep");
    assert_eq!(fs::read(&local_file).unwrap(), b"keep");
}

#[cfg(feature = "test-faults")]
#[test]
fn live_cleanup_drains_a_shared_alias_client_only_once() {
    use vfsi_core::internal::faults::{FaultScript, OpenFaultPoint};
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let mut backend = vfsi_nfs::NfsVecFs::connect_minor(&server, minor).unwrap();
    let fault = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCloseDispatch { index: 0 },
        std::io::Error::from_raw_os_error(libc::EIO),
    ));
    backend.set_fault_injector(fault.clone());
    let remote = FsClient::new(backend);
    let path = PathBuf::from(format!("/routed-cleanup-{}", std::process::id()));
    drop(remote.create(&path).unwrap());
    let root = tempfile::tempdir().unwrap();
    let auto = Auto::new(root.path()).unwrap();
    let identity = Arc::new(());
    for id in [1, 2] {
        auto.connections.lock().unwrap().insert(
            id,
            Arc::new(routing::NfsConnectionState {
                spec: MountSpec {
                    id,
                    mount_point: root.path().join(id.to_string()),
                    export: PathBuf::from("/"),
                    server: server.clone().into_boxed_str(),
                    minor,
                },
                client: remote.clone(),
                identity: identity.clone(),
                credentials: AuthSysIdentity::current().unwrap(),
            }),
        );
    }

    let _ = vfsi_nfs::compound::thread_compound_stats();
    let first = auto.drain_cleanup();
    let first_calls = vfsi_nfs::compound::thread_compound_stats().0;
    let second = auto.drain_cleanup();
    let second_calls = vfsi_nfs::compound::thread_compound_stats().0;
    remote.remove_file(&path).unwrap();
    assert!(fault.is_consumed());
    assert_eq!(
        vfsi_core::native_status(&first.unwrap_err()),
        Some(vfsi_core::StatusCode::Errno(libc::EIO as u32))
    );
    assert_eq!(
        first_calls, 0,
        "a mount alias must not retry the failed close"
    );
    second.unwrap();
    assert_eq!(second_calls, 1, "the next cleanup owns the pending close");
}

#[test]
fn live_directory_open_results_keep_siblings_across_route_cohorts() {
    use crate::{
        VfsiExt,
        nfs::{Nfs, NfsVersion},
    };
    let Ok(server) = std::env::var("VFSI_NFS_SERVER") else {
        assert_ne!(std::env::var("VFSI_NFS_REQUIRED").as_deref(), Ok("1"));
        return;
    };
    let export = std::env::var("VFSI_NFS_EXPORT").unwrap_or_else(|_| "/".into());
    let minor = std::env::var("VFSI_NFS_MINOR")
        .unwrap_or_else(|_| "2".into())
        .parse()
        .unwrap();
    let remote = Nfs::builder(&server)
        .root(&export)
        .version(NfsVersion::try_from(Some(minor)).unwrap())
        .connect()
        .unwrap();
    let local = tempfile::tempdir().unwrap();
    let client = Auto::new(local.path()).unwrap();
    client.create_dir("/local").unwrap();
    let base = PathBuf::from(format!("/routed-directory-open-{}", std::process::id()));
    let first = base.join("first");
    let last = base.join("last");
    remote.create_dir_all(&first).unwrap();
    remote.create_dir_all(&last).unwrap();
    remote.write(first.join("file"), b"first").unwrap();
    remote.write(last.join("file"), b"last").unwrap();
    let connection = Arc::new(routing::NfsConnectionState {
        spec: MountSpec {
            id: u64::MAX,
            mount_point: local.path().to_owned(),
            export: export.into(),
            server: server.into_boxed_str(),
            minor,
        },
        client: remote.inner.clone(),
        identity: Arc::new(()),
        credentials: AuthSysIdentity::current().unwrap(),
    });
    let mut alias_spec = connection.spec.clone();
    alias_spec.id -= 1;
    alias_spec.mount_point = PathBuf::from("/directory-test-alias");
    let alias = Arc::new(routing::NfsConnectionState {
        spec: alias_spec,
        client: connection.client.clone(),
        identity: connection.identity.clone(),
        credentials: connection.credentials.clone(),
    });
    let public: Vec<_> = [
        "/local-before",
        "/remote-first",
        "/remote-missing",
        "/local-between",
        "/alias-last",
        "/remote-first-again",
    ]
    .into_iter()
    .map(Path::new)
    .collect();
    let resolved = [
        Resolved {
            route: Route::Posix,
            path: "/local".into(),
        },
        Resolved {
            route: Route::Nfs(connection.clone()),
            path: first.clone(),
        },
        Resolved {
            route: Route::Nfs(alias.clone()),
            path: base.join("missing"),
        },
        Resolved {
            route: Route::Posix,
            path: "/local".into(),
        },
        Resolved {
            route: Route::Nfs(alias),
            path: last.clone(),
        },
        Resolved {
            route: Route::Nfs(connection),
            path: first.clone(),
        },
    ];
    let results = client.vopen_dirs_resolved(&public, &resolved).unwrap();
    assert_eq!(results.len(), public.len());
    let mut dirs = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        match index {
            0 | 3 => assert_eq!(
                result.err().unwrap().cause().kind(),
                crate::ErrorKind::Unsupported
            ),
            2 => {
                let error = result.err().unwrap();
                assert_eq!(error.cause().kind(), crate::ErrorKind::NotFound);
                assert_eq!(
                    vfsi_core::internal::errors::path(error.cause()),
                    Some(public[index])
                );
            }
            _ => {
                let dir = result.unwrap();
                assert_eq!(dir.path(), public[index]);
                let Route::Nfs(actual) = &dir.route else {
                    panic!("lost NFS route")
                };
                let Route::Nfs(expected) = &resolved[index].route else {
                    unreachable!()
                };
                assert!(Arc::ptr_eq(actual, expected), "retain each mount alias");
                dirs.push(dir);
            }
        }
    }
    remote.rename(&first, base.join("moved")).unwrap();
    remote.create_dir(&first).unwrap();
    remote.write(first.join("keep"), b"replacement").unwrap();
    let results = client
        .vremove_dir_contents(&dirs.iter().collect::<Vec<_>>(), Default::default())
        .unwrap();
    assert!(results.iter().all(Result::is_ok));
    assert!(remote.read_dir(base.join("moved")).unwrap().is_empty());
    assert!(remote.read_dir(&last).unwrap().is_empty());
    assert_eq!(remote.read(first.join("keep")).unwrap(), b"replacement");
    for dir in dirs {
        dir.close().unwrap();
    }
    remote.remove_dir_all(&base).unwrap();
}
