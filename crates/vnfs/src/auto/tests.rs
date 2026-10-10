use super::routing::{MountSpec, parse_mount};
use super::*;
use crate::ListDirOptions;
use crate::Vfsi;
use std::io::Read;
use vfsi_nfs::mount::decode_mount_field;
use vfsi_sync::DEFAULT_READ_ALLV_MAX_TOTAL_BYTES;

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
            .err_no(),
        libc::EINVAL as u32
    );
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
            .read_dirs_with_options(&["/dir"], ListDirOptions::new().max_path_bytes(12))
            .unwrap()[0][0]
            .entries
            .len(),
        2
    );
    let error = client
        .read_dirs_with_options(&["/dir"], ListDirOptions::new().max_path_bytes(11))
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
            .err_no(),
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
            .err_no(),
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
fn parses_only_unambiguous_supported_mounts() {
    let line = b"46 49 0:45 / /mnt/nfs rw,relatime - nfs4 server:/export rw,vers=4.2,proto=tcp,sec=sys,addr=192.0.2.7";
    let spec = parse_mount(line).unwrap();
    assert_eq!(spec.mount_point, Path::new("/mnt/nfs"));
    assert_eq!(spec.server, "192.0.2.7:2049");
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
        .server,
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
        Route::Mounted
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
    assert_eq!(client.route_for("/file"), AutoRoute::Mounted);
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
            crate::AttrsOptions::new().follow_symlinks(false),
        )
        .unwrap();
    assert_eq!(metadata[0].file_type(), crate::FileType::Regular);
    assert_eq!(metadata[1].file_type(), crate::FileType::Symlink);
    let error = client
        .mounted
        .vgetattrs(
            &[Path::new("/one"), Path::new("/missing")],
            crate::AttrsOptions::new().follow_symlinks(false),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    let tiny = Auto::new(&root)
        .unwrap()
        .with_limits(ResourceLimits::new().max_read_bytes(5));
    let tiny_file = tiny.open("/one").unwrap();
    let error = tiny
        .vread(
            [crate::ReadOp::range(&tiny_file, 0, 6)],
            crate::ReadOptions::default(),
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
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
    assert_eq!(error.err_no(), libc::EFBIG as u32);
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
                crate::WriteOptions::new().write_all(true),
            )
            .unwrap_err();
        let first = client.read("/first").unwrap();
        client.vclose(&mut files).unwrap();
        assert_eq!(first, b"original");
        assert_eq!(error.index(), Some(1));
        assert_eq!(
            error.err_no(),
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
    assert_eq!(client.route_for(&local), AutoRoute::Mounted);
    let requests = [first.as_path(), second.as_path(), local.as_path()].map(|path| {
        OpenOp::new(
            path,
            OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE_NEW,
        )
    });
    let files = client.vopen(&requests).unwrap();
    assert!(matches!(files[0].route(), AutoRoute::DirectNfs { .. }));
    assert!(matches!(files[1].route(), AutoRoute::DirectNfs { .. }));
    assert_eq!(files[2].route(), AutoRoute::Mounted);
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
        reads.iter().map(|read| read.read()).collect::<Vec<_>>(),
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
            crate::WriteOptions::new().write_all(true),
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
                crate::ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(15))
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
    assert_eq!(linked.route(), AutoRoute::Mounted);
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
            .with_limits(ResourceLimits::new().max_read_bytes(4));
        let file = client.open(&path).unwrap();
        assert!(matches!(file.route(), AutoRoute::DirectNfs { .. }));
        let client = client.with_limits(ResourceLimits::new().max_read_bytes(8));
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
                    assert_eq!(results[0].read(), 6);
                    assert_eq!(&buffer, b"abcdef");
                }),
        );
        let client = client.with_limits(ResourceLimits::new().max_read_bytes(3));
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
                    crate::WriteOptions::new().write_all(true),
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
            error.err_no(),
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
                .fields(crate::Attributes::stat())
                .recursive(true),
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
                .max_path_bytes(backend_total)
                .fields(crate::Attributes::stat())
                .recursive(true),
        )
        .map(|mut trees| trees.remove(0));
    let mut dir_bytes = 0;
    let dir = client.listdir(
        &root,
        ListDirOptions::new().max_path_bytes(backend_entries),
        |entry| {
            dir_bytes += entry.entry.path().as_os_str().len();
            Ok(vfsi_core::api::WalkControl::Continue)
        },
    );
    let mut tree_bytes = root.as_os_str().len();
    let tree = client.listdir(
        &root,
        crate::ListDirOptions::new()
            .recursive(true)
            .max_path_bytes(backend_total),
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
                    .max_path_bytes(public_total)
                    .fields(crate::Attributes::stat())
                    .recursive(true)
            )
            .map(|mut trees| trees.remove(0))
            .is_ok()
    );
    assert!(
        client
            .listdir(
                &root,
                ListDirOptions::new().max_path_bytes(public_entries),
                |_| Ok(vfsi_core::api::WalkControl::Continue)
            )
            .is_ok()
    );
    assert_eq!(
        client
            .listdir(
                &root,
                crate::ListDirOptions::new()
                    .recursive(true)
                    .max_path_bytes(public_total),
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
    let fs = crate::Auto::new(root.path()).unwrap();
    for path in ["/first", "/missing", "/file"] {
        require_live_direct_route(&fs, Path::new(path));
    }
    let error = fs.vopen_dirs(&["/first", "/missing"]).unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.path(), Some(Path::new("/missing")));
    assert_eq!(error.operation(), Some("vopen_dirs"));
    assert_eq!(error.kind(), crate::ErrorKind::NotFound);
    fs.drain_cleanup().unwrap();

    let error = fs.open_dir_handle("/file").unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.path(), Some(Path::new("/file")));
    assert_eq!(error.operation(), Some("vopen_dirs"));
    assert_eq!(error.kind(), crate::ErrorKind::NotADirectory);
    assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"keep");
    fs.open_dir_handle("/first").unwrap().close().unwrap();
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
    assert_eq!(file.route(), AutoRoute::Mounted);
    let mut contents = String::new();
    client.std_io(&file).read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "bind source\n");
    file.close().unwrap();
}
