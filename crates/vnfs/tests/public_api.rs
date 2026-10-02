//! Compile-time coverage for the application-facing NFS API and the explicit
//! low-level backend namespace. No live server is needed.

use vnfs::{Nfs, NfsAuthentication, NfsBuilder, OpenFlags, OpenRequest, ReadAllOptions};

#[test]
fn application_surface_is_small_and_typed() {
    let _: NfsBuilder = Nfs::builder("server.example.com")
        .root("/export")
        .auth(NfsAuthentication::AuthSys);
    let _ = OpenRequest::new("/file", OpenFlags::READ);
    assert_eq!(ReadAllOptions::new().total_byte_limit(), 16 * 1024 * 1024);
    let _: Option<vnfs::NfsClient> = None;
    let _: Option<vnfs::NfsFile> = None;
    let _: Option<vnfs::Result<()>> = None;
}

#[test]
fn application_requests_results_and_errors_are_root_types() {
    fn request(file: &vnfs::NfsFile) -> vnfs::NfsRead<'_> {
        file.read_request_at(0, 1)
    }
    let _ = request;
    let result = vnfs::ReadResult {
        offset: 0,
        data: vec![1],
        eof: false,
    };
    assert_eq!(result.data, [1]);
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
        let _ = client.symlink_metadata_with_fields("/a/link", fields)?;
        let _ = client.walk_with_options("/a", fields, vnfs::WalkOptions::new())?;
        client.copy_files(&[("/a/source", "/a/copy")])?;
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
    client.create_dir_all("/sub").unwrap();
    client
        .write_files(&[("/sub/a", b"abc"), ("/sub/b", b"def")])
        .unwrap();
    assert_eq!(
        client.read_files(&["/sub/a", "/sub/b"]).unwrap(),
        [b"abc".to_vec(), b"def".to_vec()]
    );
    assert!(
        client
            .read_files_with_options(
                &["/sub/a", "/sub/b"],
                vnfs::ReadAllOptions::new().max_total_bytes(5)
            )
            .is_err()
    );
    assert_eq!(client.read_to_string("/sub/a").unwrap(), "abc");
    let mut files = client
        .open_options()
        .read(true)
        .write(true)
        .openv(&["/sub/a", "/sub/b"])
        .unwrap();
    let mut a = [0; 4];
    let mut b = [0; 2];
    let result = client
        .readv_into(&mut [
            files[0].read_request_at_into(0, &mut a),
            files[1].read_request_at_into(1, &mut b),
        ])
        .unwrap();
    assert_eq!(result[0].read, 3);
    assert!(result[0].eof);
    assert_eq!(&b, b"ef");
    client
        .write_allv(&[
            files[0].write_request_at(1, b"XY"),
            files[1].write_request_at(0, b"UV"),
        ])
        .unwrap();
    assert_eq!(client.read("/sub/a").unwrap(), b"aXY");
    assert_eq!(
        client.read_stream("/sub/a", |_, _| Ok(false)).unwrap(),
        vnfs::StreamCompletion::Stopped { next_offset: 2 }
    );
    assert_eq!(
        client.visit_walk("/sub", |_| Ok(true)).unwrap(),
        vnfs::TraversalCompletion::Complete
    );
    assert_eq!(client.walk("/sub").unwrap()[0].entries.len(), 2);
    client.try_closev(&mut files).unwrap();
    assert!(files.iter().all(|file| file.is_closed()));
    client.try_closev(&mut files).unwrap();
    assert!(files[0].read_at(&mut a, 0).is_err());
    let foreign = vnfs::Auto::new(&root).unwrap();
    let file = client.open("/sub/a").unwrap();
    assert!(
        foreign
            .write_allv(&[file.write_request_at(0, b"wrong")])
            .is_err()
    );
    file.close().unwrap();
    client
        .set_metadata("/sub/a")
        .len(2)
        .permissions(vnfs::Permissions::from_mode(0o600))
        .apply()
        .unwrap();
    assert_eq!(client.metadata("/sub/a").unwrap().len(), 2);
    client.create_dir_with_mode("/copies", 0o700).unwrap();
    client
        .copy_files(&[("/sub/a", "/copies/a"), ("/sub/b", "/copies/b")])
        .unwrap();
    client.hard_link("/sub/a", "/copies/hard").unwrap();
    client.symlink("a", "/sub/link").unwrap();
    assert_eq!(
        client.read_link("/sub/link").unwrap(),
        std::path::Path::new("a")
    );
    assert!(client.symlink_metadata("/sub/link").unwrap().is_symlink());
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
    use vnfs::{Client, FileHandle};
    fn workflow<C: Client>(client: &C, prefix: &str) -> vnfs::Result<()> {
        client.create_dir_all(prefix)?;
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
            .map(|file| file.write_request_at(0, b"abc"))
            .collect();
        assert!(
            client
                .write_allv(&writes)?
                .iter()
                .all(|result| result.written == 3)
        );
        drop(writes);
        let reads: Vec<_> = files
            .iter()
            .map(|file| file.read_request_at(0, 4))
            .collect();
        assert!(
            client
                .readv(&reads)?
                .iter()
                .all(|result| result.data == b"abc" && result.eof)
        );
        drop(reads);
        let mut first = [0; 4];
        let mut second = [0; 4];
        let result = client.readv_into(&mut [
            files[0].read_request_at_into(0, &mut first),
            files[1].read_request_at_into(0, &mut second),
        ])?;
        assert!(result.iter().all(|result| result.read == 3 && result.eof));
        files[0].try_close()?;
        client.try_closev(&mut files)?;
        client.try_closev(&mut files)?;
        assert!(files.iter().all(|file| file.is_closed()));
        assert_eq!(
            client.read_files(&paths)?,
            [b"abc".to_vec(), b"abc".to_vec()]
        );
        assert_eq!(client.read_dirs(&[prefix])?[0].entries.len(), 2);
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
