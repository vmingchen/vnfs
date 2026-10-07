//! Compile-time coverage for the application-facing NFS API and the explicit
//! low-level backend namespace. No live server is needed.

use vnfs::{Nfs, NfsAuthentication, NfsBuilder, OpenFlags, OpenOp, ReadOptions, Vfsi, VfsiExt};

#[test]
fn application_surface_is_small_and_typed() {
    let _: NfsBuilder = Nfs::builder("server.example.com")
        .root("/export")
        .auth(NfsAuthentication::AuthSys);
    let _ = OpenOp::new("/file", OpenFlags::READ);
    assert_eq!(ReadOptions::new().total_byte_limit(), None);
    assert_eq!(
        ReadOptions::new()
            .max_total_bytes(std::num::NonZeroUsize::new(42))
            .total_byte_limit(),
        std::num::NonZeroUsize::new(42)
    );
    let _: Option<vnfs::NfsClient> = None;
    let _: Option<vnfs::NfsFile> = None;
    let _: Option<vnfs::Result<()>> = None;
}

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn concrete_and_extension_directory_visitors_borrow_entries_consistently() {
    fn callback(entry: &vnfs::DirEntry) -> vnfs::Result<vnfs::ControlFlow<()>> {
        assert!(entry.path().ends_with("file"));
        Ok(vnfs::ControlFlow::Continue(()))
    }
    fn direct(fs: &vnfs::NfsClient) -> vnfs::Result<vnfs::TraversalCompletion> {
        fs.visit_dir_with_options("/dir", vnfs::ListDirOptions::new(), callback)
    }
    let _ = direct; // Compile-check direct NFS without requiring a connection.
    let root = tempfile_root();
    std::fs::write(root.join("file"), b"data").unwrap();
    let mounted = vnfs::Mounted::new(&root).unwrap();
    let auto = vnfs::Auto::new(&root).unwrap();
    // The dereferenced concrete client must not shadow the public helper with
    // its private optimized whole-file implementation and native option type.
    let concrete: &vnfs::AutoClient = &auto;
    assert_eq!(
        concrete
            .read_files_with_options(
                &["/file"],
                vnfs::ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(4))
            )
            .unwrap(),
        [b"data".to_vec()]
    );
    assert_eq!(
        mounted
            .visit_dir_with_options("/", vnfs::ListDirOptions::new(), callback)
            .unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(
        VfsiExt::visit_dir_with_options(&mounted, "/", vnfs::ListDirOptions::new(), callback)
            .unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(
        auto.visit_dir_with_options("/", vnfs::ListDirOptions::new(), callback)
            .unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    std::fs::remove_file(root.join("file")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn application_requests_results_and_errors_are_root_types() {
    fn request(file: &vnfs::NfsFile) -> vnfs::ReadOp<'_, vnfs::NfsFile> {
        vnfs::ReadOp::range(file, 0, 1)
    }
    let _ = request;
    let result = vnfs::ReadResult::owned(0, vec![1], false);
    assert_eq!(result.data(), Some([1].as_slice()));
    let into = vnfs::ReadIntoResult {
        offset: 7,
        read: 1,
        eof: true,
    };
    assert!(into.eof);
    let error = vnfs::Error::nfs(2, 28);
    assert_eq!(error.kind(), vnfs::ErrorKind::StorageFull);
    assert_eq!(error.status(), Some(vnfs::StatusCode::Nfs(28)));
    assert_eq!(error.index(), Some(2));
    assert!(vnfs::NfsVersion::try_from(Some(3)).is_err());
    let _ = Nfs::builder("server")
        .version(vnfs::NfsVersion::V4_1)
        .limits(vnfs::ResourceLimits::default());
}

#[test]
fn application_traversal_and_mutation_do_not_require_backend_imports() {
    fn app(client: &vnfs::NfsClient) -> vnfs::Result<()> {
        let fields = vnfs::Attributes::MODE | vnfs::Attributes::BLOCKS | vnfs::Attributes::NLINK;
        let listings = client.read_dirs_with_options(
            &["/a", "/b"],
            vnfs::ListDirOptions::from(vnfs::ReadDirOptions::new()).fields(fields),
        )?;
        for directory in listings.into_iter().flatten() {
            for entry in directory.entries {
                let _ = (entry.path(), entry.attrs().blocks());
            }
        }
        let _ = client.attrs_with_options(
            "/a/link",
            vnfs::AttrsOptions::new()
                .fields(fields)
                .follow_symlinks(false),
        )?;
        let _ = client.walk_with_options(
            "/a",
            vnfs::ListDirOptions::from(vnfs::WalkOptions::new()).fields(fields),
        )?;
        client.vcopy(&[("/a/source", "/a/copy")], vnfs::CopyOption::default())?;
        client.vremove_native(&["/a/copy"], false)
    }
    let _ = app as fn(&vnfs::NfsClient) -> vnfs::Result<()>;
}

#[test]
fn backend_implementers_depend_on_lower_level_crates() {
    use vfsi_sync::{Backend, ReadOp, VfFile, VfOffset};

    let _ = ReadOp::new(VfFile::from_path("/file"), VfOffset::At(0), 1);
    fn accepts_backend<T: Backend + ?Sized>(_: &mut T) {}
    let _ = accepts_backend::<dyn Backend>;
}

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn auto_supports_the_core_native_bulk_and_streaming_surface() {
    let root = tempfile_root();
    let client = vnfs::Auto::new(&root)
        .unwrap()
        .with_limits(vnfs::ResourceLimits {
            max_read_bytes: 6,
            stream_chunk_bytes: 2,
            ..Default::default()
        });
    client.create_dir_all("/sub").unwrap();
    client
        .write_files(&[("/sub/a", b"abc"), ("/sub/b", b"def")])
        .unwrap();
    assert_eq!(
        client
            .vread(
                [vnfs::ReadOp::whole("/sub/a"), vnfs::ReadOp::whole("/sub/b")],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .into_iter()
            .map(|result| result.into_data().unwrap())
            .collect::<Vec<_>>(),
        [b"abc".to_vec(), b"def".to_vec()]
    );
    assert!(
        client
            .vread(
                [vnfs::ReadOp::whole("/sub/a"), vnfs::ReadOp::whole("/sub/b")],
                vnfs::ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(5))
            )
            .is_err()
    );
    assert_eq!(client.read_to_string("/sub/a").unwrap(), "abc");
    let mut files = client
        .open_options()
        .read(true)
        .write(true)
        .vopen(&["/sub/a", "/sub/b"])
        .unwrap();
    let mut a = [0; 4];
    let mut b = [0; 2];
    let result = client
        .vread(
            [
                vnfs::ReadOp::into(&files[0], 0, &mut a),
                vnfs::ReadOp::into(&files[1], 1, &mut b),
            ],
            Default::default(),
        )
        .unwrap();
    assert_eq!(result[0].read(), 3);
    assert!(result[0].eof());
    assert_eq!(&b, b"ef");
    client
        .vwrite(
            &[
                vnfs::WriteOp::at(&files[0], 1, b"XY"),
                vnfs::WriteOp::at(&files[1], 0, b"UV"),
            ],
            vnfs::WriteOptions::new().write_all(true),
        )
        .unwrap();
    assert_eq!(
        client
            .vread(
                [vnfs::ReadOp::whole("/sub/a")],
                vnfs::ReadOptions::default()
            )
            .unwrap()[0]
            .data()
            .unwrap(),
        b"aXY"
    );
    assert_eq!(
        client.read_stream("/sub/a", |_, _| Ok(false)).unwrap(),
        vnfs::StreamCompletion::Stopped { next_offset: 2 }
    );
    assert_eq!(
        client
            .visit_walk("/sub", |_| Ok(std::ops::ControlFlow::Continue(())))
            .unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(client.walk("/sub").unwrap()[0].entries.len(), 2);
    client.vclose(&mut files).unwrap();
    assert!(files.iter().all(|file| file.is_closed()));
    client.vclose(&mut files).unwrap();
    assert!(files[0].read_at(&mut a, 0).is_err());
    let foreign = vnfs::Auto::new(&root).unwrap();
    let file = client.open("/sub/a").unwrap();
    assert!(
        foreign
            .vwrite(
                &[vnfs::WriteOp::at(&file, 0, b"wrong")],
                vnfs::WriteOptions::new().write_all(true)
            )
            .is_err()
    );
    file.close().unwrap();
    client
        .set_metadata("/sub/a")
        .len(2)
        .permissions(vnfs::Permissions::from_mode(0o600))
        .apply()
        .unwrap();
    assert_eq!(client.attrs("/sub/a").unwrap().len(), 2);
    client.create_dir_with_mode("/copies", 0o700).unwrap();
    client
        .vcopy(
            &[("/sub/a", "/copies/a"), ("/sub/b", "/copies/b")],
            vnfs::CopyOption::default(),
        )
        .unwrap();
    client.hard_link("/sub/a", "/copies/hard").unwrap();
    client.symlink("a", "/sub/link").unwrap();
    assert_eq!(
        client.read_link("/sub/link").unwrap(),
        std::path::Path::new("a")
    );
    assert!(client.symlink_attrs("/sub/link").unwrap().is_symlink());
    let metadata = client
        .vgetattrs(
            &[
                std::path::Path::new("/sub/a"),
                std::path::Path::new("/sub/link"),
            ],
            vnfs::AttrsOptions::new().follow_symlinks(false),
        )
        .unwrap();
    assert!(metadata[0].is_file());
    assert!(metadata[1].is_symlink());
    let listings = client.read_dirs(&["/sub", "/copies"]).unwrap();
    assert_eq!(
        listings
            .iter()
            .map(|listing| listing.entries.len())
            .collect::<Vec<_>>(),
        [3, 3]
    );
    assert!(
        client
            .read_dirs_with_options(
                &["/sub", "/copies"],
                vnfs::ListDirOptions::from(vnfs::ReadDirOptions::new().max_entries(5))
                    .fields(vnfs::Attributes::stat())
            )
            .is_err()
    );
    assert_eq!(
        client.open_dir_handle("/sub").unwrap_err().kind(),
        vnfs::ErrorKind::Unsupported
    );
    client
        .vremove_native(&["/copies/a", "/copies/b", "/copies/hard"], false)
        .unwrap();
    client.ensure_empty_dir("/copies").unwrap();
    client.remove_dir_all("/copies").unwrap();
    client.remove_dir_contents("/sub").unwrap();
    client.remove_dir("/sub").unwrap();
    std::fs::remove_dir(&root).unwrap();
}

#[cfg(all(feature = "auto", target_os = "linux"))]
fn tempfile_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "vnfs-public-api-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn one_generic_application_uses_mounted_auto_or_direct_nfs_without_backend_types() {
    use vnfs::{FileHandle, Vfsi, VfsiExt};
    fn workflow<C: Vfsi>(client: &C, prefix: &str) -> vnfs::Result<()> {
        client.create_dir_all(prefix)?;
        let paths = [format!("{prefix}/a"), format!("{prefix}/b")];
        let requests: Vec<_> = paths
            .iter()
            .map(|path| {
                OpenOp::new(
                    path,
                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE_NEW,
                )
            })
            .collect();
        let mut files = client.vopen(&requests)?;
        let writes: Vec<_> = files
            .iter()
            .map(|file| vnfs::WriteOp::at(file, 0, b"abc"))
            .collect();
        assert!(
            client
                .vwrite(&writes, vnfs::WriteOptions::new().write_all(true))?
                .iter()
                .all(|result| result.written == 3)
        );
        drop(writes);
        let reads: Vec<_> = files
            .iter()
            .map(|file| vnfs::ReadOp::range(file, 0, 4))
            .collect();
        assert!(
            client
                .vread(reads, Default::default())?
                .iter()
                .all(|result| result.data() == Some(b"abc".as_slice()) && result.eof())
        );

        let mut first = [0; 4];
        let mut second = [0; 4];
        let result = client.vread(
            [
                vnfs::ReadOp::into(&files[0], 0, &mut first),
                vnfs::ReadOp::into(&files[1], 0, &mut second),
            ],
            Default::default(),
        )?;
        assert!(
            result
                .iter()
                .all(|result| result.read() == 3 && result.eof())
        );
        files[0].seek_native(std::io::SeekFrom::Start(0))?;
        assert_eq!(files[0].read_to_end_with_limit(3)?, b"abc");
        files[0].try_close()?;
        client.vclose(&mut files)?;
        client.vclose(&mut files)?;
        assert!(files.iter().all(|file| file.is_closed()));
        assert_eq!(
            client
                .vread(
                    paths.iter().map(vnfs::ReadOp::whole).collect::<Vec<_>>(),
                    vnfs::ReadOptions::default()
                )?
                .into_iter()
                .map(|result| result.into_data().unwrap())
                .collect::<Vec<_>>(),
            [b"abc".to_vec(), b"abc".to_vec()]
        );
        assert_eq!(client.read_dirs(&[prefix])?[0].entries.len(), 2);
        let mut visited = 0;
        assert_eq!(
            client.visit_dir_with_options(prefix, vnfs::ListDirOptions::default(), |_| {
                visited += 1;
                Ok(vnfs::ControlFlow::Continue(()))
            })?,
            vnfs::TraversalCompletion::Complete
        );
        assert_eq!(visited, 2);
        client.remove_dir_all(prefix)
    }
    // Compile the identical function against direct NFS without connecting.
    let _ = workflow::<vnfs::NfsClient>;
    let root = tempfile_root();
    let mounted = vnfs::Mounted::new(&root).unwrap();
    workflow(&mounted, "/mounted").unwrap();
    let auto = vnfs::Auto::new(&root).unwrap();
    workflow(&auto, "/auto").unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn traversal_depths_share_finite_and_unlimited_semantics_without_panics() {
    use vnfs::{Attributes, ListDirOptions, ResourceLimits, WalkOptions};
    let root = tempfile::tempdir().unwrap();
    let fs = vnfs::Mounted::new(root.path()).unwrap();
    fs.create_dir_all("/tree/child").unwrap();
    fs.write("/tree/child/file", b"x").unwrap();
    for depth in [201, 254, 255, 256, 500, usize::MAX - 1, usize::MAX] {
        let walk = WalkOptions::new().max_depth(depth);
        assert_eq!(walk.depth_limit(), usize::MAX);
        let converted = ListDirOptions::from(walk);
        assert!(converted.is_recursive());
        let explicit = fs
            .walk_with_options(
                "/tree",
                vnfs::ListDirOptions::from(walk).fields(Attributes::MODE),
            )
            .unwrap();
        let direct = fs
            .read_dirs_with_options(
                &["/tree"],
                ListDirOptions::new().recursive(true).max_depth(depth),
            )
            .unwrap();
        assert_eq!(explicit.len(), 2);
        assert_eq!(direct[0].len(), 2);
        let inherited = vnfs::Mounted::new(root.path())
            .unwrap()
            .with_limits(ResourceLimits {
                max_walk_depth: depth,
                ..ResourceLimits::default()
            });
        assert_eq!(inherited.walk("/tree").unwrap().len(), 2);
    }
    assert!(
        fs.walk_with_options(
            "/tree",
            vnfs::ListDirOptions::from(WalkOptions::new().max_depth(0)).fields(Attributes::MODE)
        )
        .is_err()
    );
    assert_eq!(
        fs.walk_with_options(
            "/tree",
            vnfs::ListDirOptions::from(WalkOptions::new().max_depth(0).truncate_at_max_depth(true))
                .fields(Attributes::MODE)
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        fs.walk_with_options(
            "/tree",
            vnfs::ListDirOptions::from(WalkOptions::new().max_depth(1)).fields(Attributes::MODE)
        )
        .unwrap()
        .len(),
        2
    );
    assert_eq!(WalkOptions::new().max_depth(200).depth_limit(), 200);
}

#[test]
fn portable_traits_and_options_are_reexports_not_parallel_contracts() {
    fn accepts_vnfs<C: vnfs::Vfsi>(_: &C) {}
    fn accepts_core<C: vfsi_core::Vfsi>(fs: &C) {
        accepts_vnfs(fs);
    }
    fn accepts_core_extension<C: vfsi_core::VfsiExt>(_: &C) {}
    fn accepts_vnfs_extension<C: vnfs::VfsiExt>(fs: &C) {
        accepts_core_extension(fs);
    }
    // Prove bounds in both directions without opening a connection.
    let _ = accepts_core::<vfsi_sync::FsClient<vfsi_local::DummyVecFs>>;
    let _ = accepts_vnfs_extension::<vfsi_sync::FsClient<vfsi_local::DummyVecFs>>;
    let core: vfsi_core::api::ReadOptions = vnfs::ReadOptions::new();
    let _: vnfs::ReadOptions = core;
    let core: vfsi_core::api::ListDirOptions = vnfs::ListDirOptions::new();
    let _: vnfs::ListDirOptions = core;
}

#[cfg(all(feature = "auto", target_os = "linux"))]
#[path = "support/vsetattrs.rs"]
mod vsetattrs_support;

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn vsetattrs_many_local_and_routed_files() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("mounted")).unwrap();
    std::fs::create_dir(root.path().join("auto")).unwrap();
    let mounted = vnfs::Mounted::new(root.path()).unwrap();
    vsetattrs_support::check_many(&mounted, "/mounted");
    vsetattrs_support::check_handles(&mounted, "/mounted");
    vsetattrs_support::check_ownership(&mounted, "/mounted");
    vsetattrs_support::check_foreign(
        &mounted,
        &vnfs::Mounted::new(root.path()).unwrap(),
        "/mounted/foreign",
    );
    let auto = vnfs::Auto::new(root.path()).unwrap();
    vsetattrs_support::check_many(&auto, "/auto");
    vsetattrs_support::check_handles(&auto, "/auto");
    vsetattrs_support::check_ownership(&auto, "/auto");
    vsetattrs_support::check_foreign(
        &auto,
        &vnfs::Auto::new(root.path()).unwrap(),
        "/auto/foreign",
    );
    std::os::unix::fs::symlink("mounted/file-0", root.path().join("link")).unwrap();
    let updates = [vnfs::SetAttrsOp::new("/link").len(11)];
    assert!(
        mounted
            .vsetattrs(&[updates[0].follow_symlinks(false)])
            .is_err()
    );
    assert_eq!(
        std::fs::metadata(root.path().join("mounted/file-0"))
            .unwrap()
            .len(),
        7
    );
    mounted.vsetattrs(&updates).unwrap();
    assert_eq!(
        std::fs::metadata(root.path().join("mounted/file-0"))
            .unwrap()
            .len(),
        11
    );
}

#[cfg(all(feature = "auto", target_os = "linux"))]
#[path = "support/links.rs"]
mod links_support;

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn vector_links_modes_and_capabilities_work_generically_on_local_and_auto() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("local")).unwrap();
    std::fs::create_dir(root.path().join("auto")).unwrap();
    let mounted = vnfs::Mounted::new(root.path()).unwrap();
    links_support::check_links_and_modes(&mounted, "/local");
    let auto = vnfs::Auto::new(root.path()).unwrap();
    links_support::check_links_and_modes(&auto, "/auto");
    assert_eq!(
        vnfs::Vfsi::capabilities(&auto).unwrap(),
        vnfs::Vfsi::capabilities(&mounted).unwrap()
    );
}

#[path = "support/statfs.rs"]
mod statfs_support;

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn filesystem_stats_local_and_routed_handles() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("local")).unwrap();
    std::fs::create_dir(root.path().join("auto")).unwrap();
    let mounted = vnfs::Mounted::new(root.path()).unwrap();
    let other = vnfs::Mounted::new(root.path()).unwrap();
    statfs_support::check(&mounted, &other, "/local");
    let auto = vnfs::Auto::new(root.path()).unwrap();
    statfs_support::check(&auto, &vnfs::Auto::new(root.path()).unwrap(), "/auto");
    let stats = mounted.statfs("/").unwrap();
    assert!(stats.fragment_size.unwrap() > 0);
    assert!(stats.block_size.unwrap() > 0);
    assert_eq!(stats.read_only, Some(false));
    assert!(stats.max_path_len.unwrap() > 0);
    assert!(stats.file_size_bits.unwrap() > 0);
    assert!(stats.max_file_size.is_none());
    // Direct fd identity remains valid after unlink, without pathname lookup.
    use vnfs::Target;
    let file = mounted.open("/local/renamed").unwrap();
    mounted.remove_file("/local/renamed").unwrap();
    assert_eq!(
        mounted.statfs(Target::File(&file)).unwrap().total_bytes,
        stats.total_bytes
    );
    file.close().unwrap();
    std::os::unix::fs::symlink("local/stats-1", root.path().join("symlink")).unwrap();
    assert_eq!(
        mounted.statfs("/symlink").unwrap().total_bytes,
        stats.total_bytes
    );
    // Compare static values against the POSIX source independently.
    use std::os::fd::AsRawFd;
    let fd = std::fs::File::open(root.path()).unwrap();
    let mut raw: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstatvfs(fd.as_raw_fd(), &mut raw) }, 0);
    assert_eq!(stats.total_bytes, raw.f_blocks.checked_mul(raw.f_frsize));
    assert_eq!(stats.fragment_size, Some(raw.f_frsize));
}
