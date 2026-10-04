//! Compile-time coverage for the application-facing NFS API and the explicit
//! low-level backend namespace. No live server is needed.

use vnfs::{Fs, FsExt, Nfs, NfsAuthentication, NfsBuilder, OpenFlags, OpenRequest, ReadOptions};

#[test]
fn application_surface_is_small_and_typed() {
    let _: NfsBuilder = Nfs::builder("server.example.com")
        .root("/export")
        .auth(NfsAuthentication::AuthSys);
    let _ = OpenRequest::new("/file", OpenFlags::READ);
    assert_eq!(ReadOptions::new().total_byte_limit(), None);
    assert_eq!(
        ReadOptions::new().max_total_bytes(42).total_byte_limit(),
        Some(42)
    );
    let _: Option<vnfs::NfsClient> = None;
    let _: Option<vnfs::NfsFile> = None;
    let _: Option<vnfs::Result<()>> = None;
}

#[test]
fn application_requests_results_and_errors_are_root_types() {
    fn request(file: &vnfs::NfsFile) -> vnfs::ReadOp<'_, vnfs::NfsFile> {
        vnfs::ReadOp::range(file, 0, 1)
    }
    let _ = request;
    let result = vnfs::ReadResult {
        offset: 0,
        read: 1,
        data: Some(vec![1]),
        eof: false,
    };
    assert_eq!(result.data.as_deref(), Some([1].as_slice()));
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
        let fields =
            vnfs::MetadataFields::MODE | vnfs::MetadataFields::BLOCKS | vnfs::MetadataFields::NLINK;
        let listings =
            client.read_dirs_with_options(&["/a", "/b"], fields, vnfs::ReadDirOptions::new())?;
        for directory in listings {
            for entry in directory.entries {
                let _ = (entry.path(), entry.metadata().blocks());
            }
        }
        let _ = client.symlink_metadata_with_fields_one("/a/link", fields)?;
        let _ = client.walk_with_options_one("/a", fields, vnfs::WalkOptions::new())?;
        client.copyv(&[("/a/source", "/a/copy")])?;
        client.remove_paths(&["/a/copy"], false)
    }
    let _ = app as fn(&vnfs::NfsClient) -> vnfs::Result<()>;
}

#[test]
fn low_level_types_are_under_backend() {
    use vnfs::backend::{ReadOp, VecFs, VfFile, VfOffset};

    let _ = ReadOp::new(VfFile::from_path("/file"), VfOffset::At(0), 1);
    fn accepts_backend<T: VecFs + ?Sized>(_: &mut T) {}
    let _ = accepts_backend::<dyn VecFs>;
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
    client.create_dir_all_one("/sub").unwrap();
    client
        .write_files(&[("/sub/a", b"abc"), ("/sub/b", b"def")])
        .unwrap();
    assert_eq!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/sub/a"), vnfs::ReadOp::whole("/sub/b")],
                vnfs::ReadOptions::default()
            )
            .unwrap()
            .into_iter()
            .map(|result| result.data.unwrap())
            .collect::<Vec<_>>(),
        [b"abc".to_vec(), b"def".to_vec()]
    );
    assert!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/sub/a"), vnfs::ReadOp::whole("/sub/b")],
                vnfs::ReadOptions::new().max_total_bytes(5)
            )
            .is_err()
    );
    assert_eq!(client.read_to_string_one("/sub/a").unwrap(), "abc");
    let mut files = client
        .open_options()
        .read(true)
        .write(true)
        .openv(&["/sub/a", "/sub/b"])
        .unwrap();
    let mut a = [0; 4];
    let mut b = [0; 2];
    let result = client
        .readv([
            vnfs::ReadOp::into(&files[0], 0, &mut a),
            vnfs::ReadOp::into(&files[1], 1, &mut b),
        ])
        .unwrap();
    assert_eq!(result[0].read, 3);
    assert!(result[0].eof);
    assert_eq!(&b, b"ef");
    client
        .writev_with_options(
            &[
                vnfs::WriteOp::at(&files[0], 1, b"XY"),
                vnfs::WriteOp::at(&files[1], 0, b"UV"),
            ],
            vnfs::WriteOptions::new().write_all(true),
        )
        .unwrap();
    assert_eq!(
        client
            .readv_with_options(
                [vnfs::ReadOp::whole("/sub/a")],
                vnfs::ReadOptions::default()
            )
            .unwrap()[0]
            .data
            .as_deref()
            .unwrap(),
        b"aXY"
    );
    assert_eq!(
        client.read_stream_one("/sub/a", |_, _| Ok(false)).unwrap(),
        vnfs::StreamCompletion::Stopped { next_offset: 2 }
    );
    assert_eq!(
        client
            .visit_walk_one("/sub", |_| Ok(std::ops::ControlFlow::Continue(())))
            .unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(client.walk_one("/sub").unwrap()[0].entries.len(), 2);
    client.try_closev(&mut files).unwrap();
    assert!(files.iter().all(|file| file.is_closed()));
    client.try_closev(&mut files).unwrap();
    assert!(files[0].read_at(&mut a, 0).is_err());
    let foreign = vnfs::Auto::new(&root).unwrap();
    let file = client.open_one("/sub/a").unwrap();
    assert!(
        foreign
            .writev_with_options(
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
    assert_eq!(client.metadata_one("/sub/a").unwrap().len(), 2);
    client.create_dir_with_mode("/copies", 0o700).unwrap();
    client
        .copyv(&[("/sub/a", "/copies/a"), ("/sub/b", "/copies/b")])
        .unwrap();
    client.hard_link("/sub/a", "/copies/hard").unwrap();
    client.symlink("a", "/sub/link").unwrap();
    assert_eq!(
        client.read_link("/sub/link").unwrap(),
        std::path::Path::new("a")
    );
    assert!(
        client
            .symlink_metadata_one("/sub/link")
            .unwrap()
            .is_symlink()
    );
    let metadata = client
        .symlink_metadatav(&[
            std::path::Path::new("/sub/a"),
            std::path::Path::new("/sub/link"),
        ])
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
                vnfs::MetadataFields::stat(),
                vnfs::ReadDirOptions::new().max_entries(5)
            )
            .is_err()
    );
    assert_eq!(
        client.open_dir_handle("/sub").unwrap_err().kind(),
        vnfs::ErrorKind::Unsupported
    );
    client
        .remove_paths(&["/copies/a", "/copies/b", "/copies/hard"], false)
        .unwrap();
    client.ensure_empty_dir("/copies").unwrap();
    client.remove_dir_all_one("/copies").unwrap();
    client.remove_dir_contents_one("/sub").unwrap();
    client.remove_dir_one("/sub").unwrap();
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
    use vnfs::{FileHandle, Fs, FsExt};
    fn workflow<C: Fs>(client: &C, prefix: &str) -> vnfs::Result<()> {
        client.create_dir_all_one(prefix)?;
        let paths = [format!("{prefix}/a"), format!("{prefix}/b")];
        let requests: Vec<_> = paths
            .iter()
            .map(|path| {
                OpenRequest::new(
                    path,
                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE_NEW,
                )
            })
            .collect();
        let mut files = client.openv(&requests)?;
        let writes: Vec<_> = files
            .iter()
            .map(|file| vnfs::WriteOp::at(file, 0, b"abc"))
            .collect();
        assert!(
            client
                .writev_with_options(&writes, vnfs::WriteOptions::new().write_all(true))?
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
                .readv(reads)?
                .iter()
                .all(|result| result.data.as_deref() == Some(b"abc".as_slice()) && result.eof)
        );

        let mut first = [0; 4];
        let mut second = [0; 4];
        let result = client.readv([
            vnfs::ReadOp::into(&files[0], 0, &mut first),
            vnfs::ReadOp::into(&files[1], 0, &mut second),
        ])?;
        assert!(result.iter().all(|result| result.read == 3 && result.eof));
        files[0].seek_native(std::io::SeekFrom::Start(0))?;
        assert_eq!(files[0].read_to_end_with_limit(3)?, b"abc");
        files[0].try_close()?;
        client.try_closev(&mut files)?;
        client.try_closev(&mut files)?;
        assert!(files.iter().all(|file| file.is_closed()));
        assert_eq!(
            client
                .readv_with_options(
                    paths.iter().map(vnfs::ReadOp::whole).collect::<Vec<_>>(),
                    vnfs::ReadOptions::default()
                )?
                .into_iter()
                .map(|result| result.data.unwrap())
                .collect::<Vec<_>>(),
            [b"abc".to_vec(), b"abc".to_vec()]
        );
        assert_eq!(client.read_dirs(&[prefix])?[0].entries.len(), 2);
        let mut visited = 0;
        assert_eq!(
            client.visit_dir_with_options_one(prefix, vnfs::ReadDirOptions::default(), |_| {
                visited += 1;
                Ok(vnfs::ControlFlow::Continue(()))
            })?,
            vnfs::TraversalCompletion::Complete
        );
        assert_eq!(visited, 2);
        client.remove_dir_all_one(prefix)
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
