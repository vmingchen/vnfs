//! Compile-time coverage for the application-facing NFS API and the explicit
//! low-level backend namespace. No live server is needed.

use std::io::Read;
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
    fn callback(event: &vnfs::WalkEvent) -> vnfs::Result<vnfs::WalkControl> {
        assert!(event.entry.path().ends_with("file"));
        Ok(vnfs::WalkControl::Continue)
    }
    fn direct(fs: &vnfs::NfsClient) -> vnfs::Result<vnfs::TraversalCompletion> {
        fs.listdir("/dir", vnfs::ListDirOptions::new(), callback)
    }
    let _ = direct; // Compile-check direct NFS without requiring a connection.
    let root = tempfile_root();
    std::fs::write(root.join("file"), b"data").unwrap();
    let mounted = vnfs::Mounted::new(&root).unwrap();
    let auto = vnfs::Auto::new(&root).unwrap();
    let concrete: &vnfs::Auto = &auto;
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
            .listdir("/", vnfs::ListDirOptions::new(), callback)
            .unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(
        VfsiExt::listdir(&mounted, "/", vnfs::ListDirOptions::new(), callback).unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(
        auto.listdir("/", vnfs::ListDirOptions::new(), callback)
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
    let into = vnfs::ReadResult::buffered(7, 1, true);
    assert!(into.eof());
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
        let listings = client
            .read_dirs_with_options(&["/a", "/b"], vnfs::ListDirOptions::new().fields(fields))?;
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
        let _ = client
            .read_dirs_with_options(
                &["/a"],
                vnfs::ListDirOptions::new().fields(fields).recursive(true),
            )
            .map(|mut trees| trees.remove(0))?;
        client.vcopy(&[("/a/source", "/a/copy")], vnfs::CopyOption::default())?;
        client.vremove(&["/a/copy"], vnfs::RemoveMode::Entry, Default::default())
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
    let client = vnfs::Auto::new(&root).unwrap().with_limits(
        vnfs::ResourceLimits::new()
            .max_read_bytes(6)
            .stream_chunk_bytes(std::num::NonZeroUsize::new(2).unwrap()),
    );
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
        client
            .read_stream("/sub/a", |_, _| Ok(std::ops::ControlFlow::Break(())))
            .unwrap(),
        vnfs::StreamCompletion::Stopped { next_offset: 2 }
    );
    assert_eq!(
        client
            .listdir(
                "/sub",
                vfsi_core::api::ListDirOptions::new().recursive(true),
                |_| Ok(vfsi_core::api::WalkControl::Continue)
            )
            .unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(
        client
            .read_dirs_with_options(
                &["/sub"],
                vnfs::ListDirOptions::new()
                    .fields(vnfs::Attributes::stat())
                    .recursive(true)
            )
            .map(|mut trees| trees.remove(0))
            .unwrap()[0]
            .entries
            .len(),
        2
    );
    client.vclose(&mut files).unwrap();
    assert!(files.iter().all(|file| file.is_closed()));
    client.vclose(&mut files).unwrap();
    assert!(client.std_io(&files[0]).read(&mut a).is_err());
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
        .vsetattrs(&[vnfs::SetAttrsOp::new("/sub/a")
            .len(2)
            .permissions(vnfs::Permissions::from_mode(0o600))])
        .unwrap();
    assert_eq!(client.attrs("/sub/a").unwrap().len().unwrap(), 2);
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
                vnfs::ListDirOptions::new()
                    .max_entries(5)
                    .fields(vnfs::Attributes::stat())
            )
            .is_err()
    );
    assert_eq!(
        client.open_dir_handle("/sub").unwrap_err().kind(),
        vnfs::ErrorKind::Unsupported
    );
    client
        .vremove(
            &["/copies/a", "/copies/b", "/copies/hard"],
            vnfs::RemoveMode::Entry,
            Default::default(),
        )
        .unwrap();
    client.remove_dir_contents("/copies").unwrap();
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
        let mut contents = Vec::new();
        client.std_io(&files[0]).read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"abc");
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
            client.listdir(prefix, vnfs::ListDirOptions::default(), |_| {
                visited += 1;
                Ok(vfsi_core::api::WalkControl::Continue)
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
    use vnfs::{Attributes, ListDirOptions, ResourceLimits};
    let root = tempfile::tempdir().unwrap();
    let fs = vnfs::Mounted::new(root.path()).unwrap();
    fs.create_dir_all("/tree/child").unwrap();
    fs.write("/tree/child/file", b"x").unwrap();
    for depth in [201, 254, 255, 256, 500, usize::MAX - 1, usize::MAX] {
        let walk = ListDirOptions::new().recursive(true).max_depth(depth);
        assert_eq!(walk.depth_limit(), usize::MAX);
        let converted = walk;
        assert!(converted.is_recursive());
        let explicit = fs
            .read_dirs_with_options(&["/tree"], walk.fields(Attributes::MODE).recursive(true))
            .map(|mut trees| trees.remove(0))
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
            .with_limits(ResourceLimits::new().max_walk_depth(depth));
        assert_eq!(
            inherited
                .read_dirs_with_options(
                    &["/tree"],
                    vnfs::ListDirOptions::new()
                        .fields(vnfs::Attributes::stat())
                        .recursive(true)
                )
                .map(|mut trees| trees.remove(0))
                .unwrap()
                .len(),
            2
        );
    }
    assert!(
        fs.read_dirs_with_options(
            &["/tree"],
            vnfs::ListDirOptions::new()
                .max_depth(0)
                .fields(Attributes::MODE)
                .recursive(true)
        )
        .map(|mut trees| trees.remove(0))
        .is_err()
    );
    assert_eq!(
        fs.read_dirs_with_options(
            &["/tree"],
            vnfs::ListDirOptions::new()
                .max_depth(0)
                .truncate_at_max_depth(true)
                .fields(Attributes::MODE)
                .recursive(true)
        )
        .map(|mut trees| trees.remove(0))
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        fs.read_dirs_with_options(
            &["/tree"],
            vnfs::ListDirOptions::new()
                .max_depth(1)
                .fields(Attributes::MODE)
                .recursive(true)
        )
        .map(|mut trees| trees.remove(0))
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        ListDirOptions::new()
            .recursive(true)
            .max_depth(200)
            .depth_limit(),
        200
    );
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

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn explicit_io_adapter_bounds_collecting_reads_and_preserves_retained_identity() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use vnfs::FileHandle;
    fn check<C: Vfsi>(fs: &C) {
        fs.write("/original", b"abcdef").unwrap();
        fs.write("/exact", b"abc").unwrap();
        fs.write("/empty", b"").unwrap();
        let file = fs.open("/original").unwrap();
        fs.rename("/original", "/moved").unwrap();
        fs.write("/original", b"replacement").unwrap();
        let metadata = fs
            .vgetattrs(
                &[
                    vnfs::Target::file(&file),
                    vnfs::Target::Path(std::path::Path::new("/original")),
                ],
                vnfs::AttrsOptions::new(),
            )
            .unwrap();
        assert_eq!(
            metadata.iter().map(|m| m.len()).collect::<Vec<_>>(),
            [Some(6), Some(11)]
        );
        let sparse = fs
            .attrs_with_options(
                vnfs::Target::file(&file),
                vnfs::AttrsOptions::new().fields(vnfs::Attributes::MODE),
            )
            .unwrap();
        assert_eq!(sparse.len(), None);
        assert_eq!(sparse.is_empty(), None);
        assert!(sparse.permissions().is_some());
        let mut io = fs.std_io(&file);
        let mut output = vec![9];
        assert_eq!(
            io.read_to_end(&mut output).unwrap_err().kind(),
            std::io::ErrorKind::FileTooLarge
        );
        assert_eq!(output, [9, b'a', b'b', b'c']);
        assert_eq!(io.stream_position().unwrap(), 4); // bounded EOF probe
        io.seek(SeekFrom::Start(0)).unwrap();
        let mut text = String::from("prefix");
        assert_eq!(
            io.read_to_string(&mut text).unwrap_err().kind(),
            std::io::ErrorKind::FileTooLarge
        );
        assert_eq!(text, "prefix");
        assert_eq!(io.seek(SeekFrom::End(-2)).unwrap(), 4);
        assert!(io.seek(SeekFrom::Current(-5)).is_err());
        assert_eq!(io.stream_position().unwrap(), 4);
        let exact = fs.open("/exact").unwrap();
        let mut output = Vec::new();
        assert_eq!(fs.std_io(&exact).read_to_end(&mut output).unwrap(), 3);
        assert_eq!(output, b"abc");
        let writable = fs
            .open_options()
            .read(true)
            .write(true)
            .open("/exact")
            .unwrap();
        let mut io = fs.std_io(&writable);
        io.seek(SeekFrom::Start(1)).unwrap();
        io.write_all(b"XY").unwrap();
        io.flush().unwrap();
        fs.sync_all(&writable).unwrap();
        assert_eq!(fs.read("/exact").unwrap(), b"aXY");
        let mut closed = fs.open("/exact").unwrap();
        closed.try_close().unwrap();
        assert_eq!(
            fs.vfsync(&[&file, &closed], vnfs::SyncMode::All)
                .unwrap_err()
                .index(),
            Some(1)
        );
        assert_eq!(
            fs.vgetattrs(
                &[vnfs::Target::file(&file), vnfs::Target::file(&closed)],
                vnfs::AttrsOptions::new()
            )
            .unwrap_err()
            .index(),
            Some(1)
        );
        let mut buffer = [0; 1];
        assert!(fs.std_io(&closed).read(&mut buffer).is_err());
        fs.vfsync(&[&file, &exact, &writable], vnfs::SyncMode::Data)
            .unwrap();
        // Both concrete and routed clients keep append behavior behind vectors.
        fs.write("/append", b"abc").unwrap();
        let append = fs
            .open_options()
            .read(true)
            .append(true)
            .open("/append")
            .unwrap();
        let mut io = fs.std_io(&append);
        assert_eq!(io.write(b"d").unwrap(), 1);
        assert_eq!(io.stream_position().unwrap(), 4);
        io.seek(SeekFrom::Start(0)).unwrap();
        io.write_all(b"ef").unwrap();
        assert_eq!(io.stream_position().unwrap(), 6);
        let mut output = [0; 6];
        io.seek(SeekFrom::Start(0)).unwrap();
        io.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"abcdef");
    }
    for auto in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let limits = vnfs::ResourceLimits::new().max_read_bytes(3);
        if auto {
            check(&vnfs::Auto::new(root.path()).unwrap().with_limits(limits));
        } else {
            check(&vnfs::Mounted::new(root.path()).unwrap().with_limits(limits));
        }
    }
    let root = tempfile::tempdir().unwrap();
    let fs = vnfs::Mounted::new(root.path())
        .unwrap()
        .with_limits(vnfs::ResourceLimits::new().max_read_bytes(0));
    fs.write("/empty", b"").unwrap();
    let file = fs.open("/empty").unwrap();
    assert_eq!(fs.std_io(&file).read_to_end(&mut Vec::new()).unwrap(), 0);
    fs.write("/nonempty", b"x").unwrap();
    let file = fs.open("/nonempty").unwrap();
    assert_eq!(
        fs.std_io(&file)
            .read_to_end(&mut Vec::new())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::FileTooLarge
    );
}

#[test]
fn policy_builders_preserve_zero_budgets_and_bound_recovery() {
    let op = OpenOp::new("/file", OpenFlags::WRITE | OpenFlags::CREATE).mode(0o600);
    assert_eq!(op.path(), std::path::Path::new("/file"));
    assert_eq!(op.flags(), OpenFlags::WRITE | OpenFlags::CREATE);
    assert_eq!(op.creation_mode(), 0o600);
    let limits = vnfs::ResourceLimits::new()
        .max_read_bytes(0)
        .stream_chunk_bytes(std::num::NonZeroUsize::new(2).unwrap())
        .max_directory_entries(0)
        .max_directory_path_bytes(0)
        .max_walk_depth(0);
    assert_eq!(limits.read_byte_limit(), 0);
    assert_eq!(limits.stream_chunk_size(), 2);
    assert_eq!(limits.directory_entry_limit(), 0);
    assert_eq!(limits.directory_path_byte_limit(), 0);
    assert_eq!(limits.walk_depth_limit(), 0);
    assert_eq!(limits.directory_options().entry_limit(), 0);
    assert_eq!(limits.walk_options().depth_limit(), 0);
    let remove = vnfs::RemoveOptions::new()
        .batch(0)
        .retries(0)
        .continue_on_error(true);
    assert_eq!(remove.batch_size(), 0);
    assert_eq!(remove.retry_limit(), 0);
    assert!(remove.continues_on_error());
    let recovery = vnfs::NfsRecoveryPolicy::new()
        .reconnect_attempts(0)
        .initial_backoff(std::time::Duration::MAX)
        .max_backoff(std::time::Duration::ZERO)
        .max_elapsed(std::time::Duration::ZERO);
    assert_eq!(recovery.attempt_limit(), 1);
    assert_eq!(recovery.initial_delay(), std::time::Duration::MAX);
    assert_eq!(recovery.maximum_delay(), std::time::Duration::ZERO);
    assert_eq!(recovery.retry_window(), std::time::Duration::ZERO);
}

#[cfg(all(feature = "auto", target_os = "linux"))]
#[test]
fn invalid_open_batch_cannot_create_or_truncate_earlier_files() {
    fn check<C: Vfsi>(fs: &C, root: &std::path::Path) {
        for flags in [
            OpenFlags::empty(),
            OpenFlags::READ | OpenFlags::TRUNCATE,
            OpenFlags::READ | OpenFlags::CREATE,
            OpenFlags::READ | OpenFlags::CREATE_NEW,
            OpenFlags::READ | OpenFlags::from_bits_retain(1 << 31),
        ] {
            std::fs::write(root.join("existing"), b"preserved").unwrap();
            let error = fs
                .vopen(&[
                    OpenOp::new("/new", OpenFlags::WRITE | OpenFlags::CREATE),
                    OpenOp::new("/existing", OpenFlags::WRITE | OpenFlags::TRUNCATE),
                    OpenOp::new("/invalid", flags),
                ])
                .err()
                .unwrap();
            assert_eq!(error.index(), Some(2));
            assert_eq!(error.err_no(), libc::EINVAL as u32);
            assert!(!root.join("new").exists());
            assert!(!root.join("invalid").exists());
            assert_eq!(std::fs::read(root.join("existing")).unwrap(), b"preserved");
        }
        // Append alone is writable under the existing portable contract.
        let file = fs
            .open_with(OpenOp::new("/existing", OpenFlags::APPEND))
            .unwrap();
        fs.close_files(vec![file]).unwrap();
    }
    let root = tempfile::tempdir().unwrap();
    check(&vnfs::Mounted::new(root.path()).unwrap(), root.path());
    check(&vnfs::Auto::new(root.path()).unwrap(), root.path());
}
