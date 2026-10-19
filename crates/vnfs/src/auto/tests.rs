use super::routing::{MountSpec, parse_mount};
use super::*;
use crate::ListDirOptions;
use crate::Vfsi;
use std::io::Read;
use vfsi_nfs::mount::decode_mount_field;
use vfsi_sync::DEFAULT_READ_ALLV_MAX_TOTAL_BYTES;

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
        let (first, next, _) = client
            .read_dir_pages_with_fields(
                &[Path::new("/dir")],
                Attributes::MODE,
                vec![None],
                1,
                10,
                true,
            )
            .unwrap()
            .remove(0);
        let mut saved = next.unwrap().into_state::<RoutedDirectoryCursor>().unwrap();
        if reroute {
            saved.backend_path = "/obsolete".into();
        }
        let seed = vfsi_sync::DirPageCursor::new(RoutedChildDirectoryCursor(saved));
        let (page, _, _) = client
            .read_dir_pages_with_fields(
                &[Path::new("/dir")],
                Attributes::MODE,
                vec![Some(seed)],
                1,
                10,
                true,
            )
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
    let (_, next, _) = client
        .read_dir_pages_with_fields(
            &[Path::new("/dir")],
            Attributes::MODE,
            vec![None],
            1,
            10,
            true,
        )
        .unwrap()
        .remove(0);
    let saved = next.unwrap().into_state::<RoutedDirectoryCursor>().unwrap();
    let seed = vfsi_sync::DirPageCursor::new(RoutedChildDirectoryCursor(saved));
    let other = Auto::new(root.path()).unwrap();
    assert_eq!(
        other
            .read_dir_pages_with_fields(
                &[Path::new("/dir")],
                Attributes::MODE,
                vec![Some(seed)],
                1,
                10,
                true
            )
            .err()
            .unwrap()
            .raw_code(),
        libc::EINVAL as u32
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
    remote.vmkdir(&[crate::MkDirOp::new(&path, 0o700)]).unwrap();
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
        let (_, next, _) = remote
            .inner
            .read_dir_pages_with_fields(&[&path], Attributes::MODE, vec![None], 1, 10, true)
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
            let error = result
                .err()
                .expect("stale credentials must reject the page");
            assert_eq!(error.raw_code(), libc::EACCES as u32);
            assert_eq!(error.index(), Some(index));
            assert_eq!(error.operation(), Some("auto_auth"));
            assert_eq!(error.path(), Some(path.as_path()));
            assert_eq!(after, before, "reject before any retained-identity RPC");
        } else {
            assert_eq!(result.unwrap()[0].0.entries.len(), 1);
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
    assert_eq!(error.kind(), crate::ErrorKind::FileTooLarge);
    assert_eq!(error.index(), Some(0));
    let cursor = client
        .read_dir_pages_with_fields(
            &[Path::new("/dir")],
            Attributes::MODE,
            vec![None],
            1,
            10,
            true,
        )
        .unwrap()
        .remove(0)
        .1
        .unwrap();
    let other = Auto::new(root.path()).unwrap();
    assert_eq!(
        other
            .read_dir_pages_with_fields(
                &[Path::new("/dir")],
                Attributes::MODE,
                vec![Some(cursor)],
                1,
                10,
                true
            )
            .err()
            .unwrap()
            .raw_code(),
        libc::EINVAL as u32
    );
    let cursor = client
        .read_dir_pages_with_fields(
            &[Path::new("/dir")],
            Attributes::MODE,
            vec![None],
            1,
            10,
            true,
        )
        .unwrap()
        .remove(0)
        .1
        .unwrap();
    assert_eq!(
        client
            .read_dir_pages_with_fields(
                &[Path::new("/other")],
                Attributes::MODE,
                vec![Some(cursor)],
                1,
                10,
                true
            )
            .err()
            .unwrap()
            .raw_code(),
        libc::EINVAL as u32
    );
    let cursor = client
        .read_dir_pages_with_fields(
            &[Path::new("/dir")],
            Attributes::MODE,
            vec![None],
            1,
            10,
            true,
        )
        .unwrap()
        .remove(0)
        .1
        .unwrap();
    let (page, next, _) = client
        .read_dir_pages_with_fields(
            &[Path::new("/dir")],
            Attributes::MODE,
            vec![Some(cursor)],
            1,
            10,
            true,
        )
        .unwrap()
        .remove(0);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].path().parent(), Some(Path::new("/dir")));
    if let Some(cursor) = next {
        let (page, next, _) = client
            .read_dir_pages_with_fields(
                &[Path::new("/dir")],
                Attributes::MODE,
                vec![Some(cursor)],
                1,
                10,
                true,
            )
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
                    mount_points: HashSet::new(),
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
        .vopen(&[
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
        .vwrite(
            &[
                crate::WriteOp::at(&files[0], 0, b"one"),
                crate::WriteOp::at(&files[1], 0, b"two"),
            ],
            Default::default(),
        )
        .unwrap();
    let reads = client
        .vread(
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
        .vgetattrs(
            &[Path::new("/one"), Path::new("/link")],
            crate::AttrsOptions::new().with_follow_final_symlink(false),
        )
        .unwrap();
    assert_eq!(metadata[0].file_type(), crate::FileType::Regular);
    assert_eq!(metadata[1].file_type(), crate::FileType::Symlink);
    let error = client
        .mounted
        .vgetattrs(
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
        .vread(
            [crate::ReadOp::range(&tiny_file, 0, 6)],
            crate::ReadOptions::default(),
        )
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    assert_eq!(
        tiny.vread(
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
        .vread(
            [crate::ReadOp::range(
                &files[0],
                0,
                DEFAULT_READ_ALLV_MAX_TOTAL_BYTES + 1,
            )],
            crate::ReadOptions::default(),
        )
        .unwrap_err();
    assert_eq!(error.raw_code(), libc::EFBIG as u32);
    client.close_files(files).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn write_allv_preflights_closed_empty_and_overflowing_requests_locally() {
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
            .vopen(
                &["/first", "/second"]
                    .map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE)),
            )
            .unwrap();
        if closed {
            files[1].try_close().unwrap();
        }
        let error = client
            .vwrite(
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
        client.vclose(&mut files).unwrap();
        assert_eq!(first, b"original");
        assert_eq!(error.index(), Some(1));
        assert_eq!(
            error.raw_code(),
            if closed { libc::EBADF } else { libc::EOVERFLOW } as u32
        );
        assert_eq!(error.operation(), Some("vwrite_native"));
        assert_eq!(error.path(), Some(Path::new("/second")));
    }
    fs::remove_dir_all(root).unwrap();
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
    let files = client.vopen(&requests).unwrap();
    assert!(matches!(files[0].route(), AutoRoute::DirectNfs { .. }));
    assert!(matches!(files[1].route(), AutoRoute::DirectNfs { .. }));
    assert_eq!(files[2].route(), AutoRoute::Posix);
    client
        .vwrite(
            &[
                crate::WriteOp::at(&files[0], 0, b"first"),
                crate::WriteOp::at(&files[1], 0, b"second"),
                crate::WriteOp::at(&files[2], 0, b"local"),
            ],
            Default::default(),
        )
        .unwrap();
    let reads = client
        .vread(
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
        .vread(
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
    assert!(reads.iter().all(|read| read.eof()));
    assert_eq!(&buffers[0][..5], b"first");
    assert_eq!(&buffers[1][..6], b"second");
    assert_eq!(&buffers[2][..5], b"local");
    client
        .vwrite(
            &[
                crate::WriteOp::at(&files[0], 0, b"FIRST"),
                crate::WriteOp::at(&files[1], 0, b"SECOND"),
                crate::WriteOp::at(&files[2], 0, b"LOCAL"),
            ],
            crate::WriteOptions::new().with_write_all(true),
        )
        .unwrap();
    let mut files = files;
    client.vclose(&mut files).unwrap();
    client.vclose(&mut files).unwrap();
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
        .vopen(&[
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
        .vopen(&[
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
                .vread(
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
                .vread(
                    [crate::ReadOp::into(&file, 0, &mut buffer)],
                    Default::default()
                )
                .unwrap_err()
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
fn live_write_allv_preflights_invalid_later_requests_across_routes() {
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
                .vopen(&paths.map(|path| OpenOp::new(path, OpenFlags::READ | OpenFlags::WRITE)))
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
                .vwrite(
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
            client.vclose(&mut files).unwrap();
            fs::remove_file(&remote).unwrap();
            fs::remove_file(&local).unwrap();
        }
    }
    for (remote_first, invalid, error, first, second, failed_path) in observations {
        assert_eq!(
            &first, b"original",
            "earlier cohort changed: remote_first={remote_first}, invalid={invalid}"
        );
        assert_eq!(second, b"original");
        assert_eq!(error.index(), Some(1));
        assert_eq!(
            error.raw_code(),
            if invalid == 0 {
                libc::EOVERFLOW
            } else {
                libc::EBADF
            } as u32
        );
        assert_eq!(error.operation(), Some("vwrite_native"));
        assert_eq!(error.path(), Some(failed_path.as_path()));
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
            client
                .public_path(connection, entry.path())
                .unwrap()
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
    let error = fs.vopen_dirs(&[&first, &missing]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.path(), Some(missing.as_path()));
    assert_eq!(error.operation(), Some("vopen_dirs"));
    assert_eq!(error.kind(), crate::ErrorKind::NotFound);
    fs.drain_cleanup().unwrap();

    let error = fs.vopen_dirs(&[&file]).unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.path(), Some(file.as_path()));
    assert_eq!(error.operation(), Some("vopen_dirs"));
    assert_eq!(error.kind(), crate::ErrorKind::NotADirectory);
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    fs.vopen_dirs(&[&first]).unwrap().remove(0).close().unwrap();
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
            .vcopy(&[(&source, &destination)], crate::CopyOptions::default())
            .unwrap_err()
            .kind()
            == std::io::ErrorKind::Unsupported
    );
    assert!(!destination.exists());
    assert_eq!(
        client
            .vhardlink(&[(&source, &destination)])
            .unwrap_err()
            .raw_code(),
        libc::EXDEV as u32
    );
    assert_eq!(
        client
            .vrename(&[(&source, &destination)], Default::default())
            .unwrap_err()
            .raw_code(),
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
    let files = client.vopen_resolved(&requests, &resolved).unwrap();
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
        .vread(
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
