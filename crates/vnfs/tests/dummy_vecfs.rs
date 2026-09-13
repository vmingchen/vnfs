//! Tests for the `std::fs`-backed [`DummyVecFs`]. These need no NFS server:
//! the suite runs against a temporary directory, proving the `VecFs` API
//! works on non-NFS filesystems too.

use vfsi_sync::test_support as common;

use std::path::Path;
use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::{VecFs, VecFsExt, VfOffset};

/// A `DummyVecFs` rooted at a fresh unique temp directory.
fn dummy() -> DummyVecFs {
    let root = std::env::temp_dir().join(format!(
        "vnfs_dummy_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    DummyVecFs::new(root)
}

#[test]
fn dummy_full_suite() {
    let mut fs = dummy();
    common::run_suite(&mut fs, "/");
}

#[test]
fn dummy_getcwd() {
    let mut fs = dummy();
    assert_eq!(fs.getcwd(), Path::new("/"));
    fs.ensure_dir(Path::new("/a/b"), 0o755).unwrap();
    fs.chdir(Path::new("/a/b")).unwrap();
    assert_eq!(fs.getcwd(), Path::new("/a/b"));
}

#[test]
fn dummy_write_read_roundtrip() {
    use vnfs::{ReadOp, VecFs, WriteOp};

    let mut fs = dummy();
    fs.ensure_dir(Path::new("/data"), 0o755).unwrap();
    let payload = b"roundtrip content".to_vec();
    fs.writev(&[WriteOp::from_path("/data/f", VfOffset::At(0), payload.clone()).with_creation()])
        .unwrap();

    let r = &fs
        .readv(&[ReadOp::from_path("/data/f", VfOffset::At(0), payload.len())])
        .unwrap()[0];
    assert_eq!(r.data, payload);
}

#[test]
fn dummy_errors_on_missing_file() {
    use vnfs::{ReadOp, VecFs, VfOffset};

    let mut fs = dummy();
    fs.ensure_dir(Path::new("/data"), 0o755).unwrap();
    let res = fs.readv(&[ReadOp::from_path("/data/missing", VfOffset::At(0), 8)]);
    match res {
        Err(e) => {
            assert_eq!(e.index(), 0);
            assert_eq!(e.err_no(), 2, "ENOENT");
        }
        Ok(_) => panic!("readv of missing file must fail"),
    }
}

#[test]
fn dummy_stays_under_root() {
    // Absolute paths map inside the root, never to the real filesystem root.
    let name = format!("vnfs_root_{}", std::process::id());
    let root = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_dir_all(&root);
    let mut fs = DummyVecFs::new(root.clone());

    fs.ensure_dir(Path::new(&format!("/{}", name)), 0o755)
        .unwrap();
    let inside_root = root.join(&name);
    assert!(inside_root.is_dir(), "created inside the root");

    let real = std::path::Path::new("/").join(&name);
    assert!(
        !real.exists(),
        "must not create anything under the real filesystem root"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&real);
}

#[test]
fn non_utf8_filenames_roundtrip() {
    use std::os::unix::ffi::OsStringExt;
    use vnfs::{ReadOp, VecFs, VfOffset, WriteOp};

    let mut fs = dummy();
    let raw = b"n\xffb";
    let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec()));
    fs.writev(&[WriteOp::from_os_path(&path, VfOffset::At(0), b"data".to_vec()).with_creation()])
        .unwrap();
    let listed = fs
        .listdir(Path::new("/"), vnfs::AttrMask::stat(), 0, false)
        .unwrap();
    assert_eq!(listed.len(), 1);
    let got = listed[0]
        .file
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_os_string();
    assert_eq!(got.into_vec(), raw);
    let r = &fs
        .readv(&[ReadOp::from_os_path(&path, VfOffset::At(0), 4)])
        .unwrap()[0];
    assert_eq!(r.data, b"data");
}

#[test]
fn path_extension_accepts_strings() {
    let mut fs = dummy();
    fs.mkdir_path("/x", 0o755).unwrap();
    assert!(fs.exists_path("/x").unwrap());
    assert_eq!(fs.stat_path("/x").unwrap().ftype, vnfs::VfType::Directory);
    fs.chdir_path("/x").unwrap();
    fs.symlink_path("missing", "dangling").unwrap();
    assert!(fs.exists_path("dangling").unwrap());
    fs.unlink_path("dangling").unwrap();
    fs.rm_recursive_path("/x").unwrap();
}

#[test]
fn standard_io_handle_is_raii_and_seekable() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use vnfs::VfOpenOptions;

    let mut fs = dummy();
    let descriptor;
    {
        let mut options = VfOpenOptions::new();
        options.read(true).write(true).create(true).truncate(true);
        let mut file = options.open(&mut fs, "/standard-io").unwrap();
        descriptor = file.descriptor().clone();
        file.write_all(b"abcdef").unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        let mut out = [0; 3];
        file.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"cde");
    }
    assert_eq!(
        fs.close(&descriptor).unwrap_err().err_no(),
        libc::EBADF as u32
    );
}

#[test]
fn standard_open_options_validate_access_modes() {
    use vnfs::VfOpenOptions;

    let mut fs = dummy();
    assert_eq!(
        VfOpenOptions::new()
            .open(&mut fs, "/invalid")
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    let mut options = VfOpenOptions::new();
    options.read(true).truncate(true);
    assert_eq!(
        options.open(&mut fs, "/invalid").err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );

    let mut options = VfOpenOptions::new();
    options.read(true);
    assert_eq!(
        options.open(&mut fs, "/missing").err().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn owned_client_supports_multiple_live_files_and_typed_requests() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use vnfs::{FsClient, OpenFlags, OpenRequest};

    let client = FsClient::new(dummy());
    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let mut first = client.open_with(OpenRequest::new("/first", flags)).unwrap();
    let mut second = client
        .open_with(OpenRequest::new("/second", flags))
        .unwrap();

    client
        .write_many(&[
            first.write_request_at(0, b"one"),
            second.write_request_at(0, b"two"),
        ])
        .unwrap();
    let read_results = client
        .read_many(&[first.read_request_at(0, 3), second.read_request_at(0, 3)])
        .unwrap();
    assert_eq!(read_results[0].data, b"one");
    assert_eq!(read_results[1].data, b"two");
    let mut one_buffer = [0; 3];
    let mut two_buffer = [0; 3];
    let lengths = client
        .read_many_into(&mut [
            first.read_request_at_into(0, &mut one_buffer),
            second.read_request_at_into(0, &mut two_buffer),
        ])
        .unwrap();
    assert_eq!(lengths, [3, 3]);
    assert_eq!(&one_buffer, b"one");
    assert_eq!(&two_buffer, b"two");
    let other_client = FsClient::new(dummy());
    assert_eq!(
        other_client
            .read_many(&[first.read_request_at(0, 1)])
            .unwrap_err()
            .err_no(),
        vnfs::ERR_INVAL
    );
    first.flush().unwrap();
    second.flush().unwrap();
    first.seek(SeekFrom::Start(0)).unwrap();
    second.seek(SeekFrom::Start(0)).unwrap();
    let mut one = String::new();
    let mut two = String::new();
    first.read_to_string(&mut one).unwrap();
    second.read_to_string(&mut two).unwrap();
    assert_eq!((one.as_str(), two.as_str()), ("one", "two"));

    first.close().unwrap();
    second.close().unwrap();
}

#[test]
fn native_client_covers_idiomatic_file_and_namespace_workflows() {
    use vnfs::{FsClient, VfType};

    let client = FsClient::new(dummy());
    client.create_dir_all("/tree/nested").unwrap();
    client.create_dir_all("/../../root-clamped").unwrap();
    assert!(client.metadata("/root-clamped").unwrap().is_dir());
    client.remove_dir("/root-clamped").unwrap();
    client.write("/tree/nested/file", b"hello").unwrap();
    assert_eq!(client.read_to_string("/tree/nested/file").unwrap(), "hello");

    let metadata = client.metadata("/tree/nested/file").unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.len(), 5);
    assert!(!metadata.permissions().readonly());

    let entries = client.read_dir("/tree/nested").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file_name().unwrap(), "file");
    assert_eq!(entries[0].file_type(), VfType::Regular);

    let file = client
        .open_options()
        .read(true)
        .write(true)
        .open("/tree/nested/file")
        .unwrap();
    assert_eq!(file.write_at(b"a", 1).unwrap(), 1);
    let mut bytes = [0; 5];
    assert_eq!(file.read_at(&mut bytes, 0).unwrap(), 5);
    assert_eq!(&bytes, b"hallo");
    assert_eq!(file.metadata().unwrap().len(), 5);
    file.set_len(4).unwrap();
    file.set_permissions(vnfs::Permissions::from_mode(0o600))
        .unwrap();
    let metadata = file.metadata().unwrap();
    assert_eq!(metadata.len(), 4);
    assert_eq!(metadata.permissions().mode(), 0o600);
    file.close().unwrap();
    client
        .set_metadata("/tree/nested/file")
        .permissions(vnfs::Permissions::from_mode(0o400))
        .len(4)
        .apply()
        .unwrap();
    let metadata = client.metadata("/tree/nested/file").unwrap();
    assert_eq!(metadata.len(), 4);
    assert_eq!(metadata.permissions().mode(), 0o400);

    client
        .rename("/tree/nested/file", "/tree/nested/renamed")
        .unwrap();
    client
        .copy("/tree/nested/renamed", "/tree/nested/copied")
        .unwrap();
    assert_eq!(client.read("/tree/nested/copied").unwrap(), b"hall");
    client.symlink("renamed", "/tree/nested/symlink").unwrap();
    assert_eq!(
        client.read_link("/tree/nested/symlink").unwrap(),
        Path::new("renamed")
    );
    assert!(
        client
            .symlink_metadata("/tree/nested/symlink")
            .unwrap()
            .is_symlink()
    );
    client
        .hard_link("/tree/nested/renamed", "/tree/nested/hardlink")
        .unwrap();
    assert_eq!(client.read("/tree/nested/hardlink").unwrap(), b"hall");
    assert_eq!(
        client
            .remove_dir("/tree/nested/hardlink")
            .unwrap_err()
            .err_no(),
        vnfs::ERR_NOTDIR
    );
    assert_eq!(
        client.remove_file("/tree/nested").unwrap_err().err_no(),
        vnfs::ERR_ISDIR
    );
    client.remove_dir_all("/tree").unwrap();
}

#[test]
fn native_open_options_and_batch_outcomes_are_composable() {
    use vnfs::{FsClient, OpOutcome};

    let client = FsClient::new(dummy());
    let files = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open_many(&["/one", "/two"])
        .unwrap();

    let writes = client
        .write_many_outcomes(&[
            files[0].write_request_at(0, b"one"),
            files[1].write_request_at(0, b"two"),
        ])
        .unwrap();
    assert!(writes.is_complete_success());
    assert!(writes.first_error().is_none());
    assert!(writes.indeterminate_indices().next().is_none());
    assert_eq!(writes.into_values().unwrap().len(), 2);

    let reads = client
        .read_many_outcomes(&[
            files[0].read_request_at(0, 3),
            files[1].read_request_at(0, 3),
        ])
        .unwrap();
    let values = reads.into_values().unwrap();
    assert_eq!(values[0].data, b"one");
    assert_eq!(values[1].data, b"two");
    client.close_many(files).unwrap();

    let invalid = client
        .open_options()
        .open_many_outcomes(&["/invalid"])
        .unwrap();
    assert!(matches!(invalid.operations(), [OpOutcome::Failed(_)]));

    client.write("/present", b"ok").unwrap();
    let partial = client
        .open_options()
        .read(true)
        .open_many_outcomes(&["/present", "/missing", "/later"])
        .unwrap();
    assert!(matches!(partial.operations()[0], OpOutcome::Success(_)));
    assert!(matches!(partial.operations()[1], OpOutcome::Failed(_)));
    assert!(matches!(partial.operations()[2], OpOutcome::NotAttempted));
}

#[test]
fn outcome_api_preserves_known_prefix_and_unattempted_suffix() {
    use vnfs::{AttrMask, OpOutcome, VfAttrs, VfFile};

    let mut fs = dummy();
    fs.writev(&[
        vnfs::WriteOp::from_path("/first", VfOffset::At(0), Vec::new()).with_creation(),
        vnfs::WriteOp::from_path("/third", VfOffset::At(0), Vec::new()).with_creation(),
    ])
    .unwrap();
    let outcome = fs.removev_outcomes(&[
        VfFile::from_path("/first"),
        VfFile::from_path("/missing"),
        VfFile::from_path("/third"),
    ]);
    assert!(matches!(outcome.operations()[0], OpOutcome::Success(())));
    assert!(matches!(outcome.operations()[1], OpOutcome::Failed(_)));
    assert!(matches!(outcome.operations()[2], OpOutcome::NotAttempted));
    assert!(fs.exists_path("/third").unwrap());

    let mut attrs = [
        VfAttrs {
            file: VfFile::from_path("/third"),
            masks: AttrMask::SIZE,
            ..VfAttrs::default()
        },
        VfAttrs {
            file: VfFile::from_path("/missing"),
            masks: AttrMask::SIZE,
            ..VfAttrs::default()
        },
        VfAttrs {
            file: VfFile::from_path("/third"),
            masks: AttrMask::SIZE,
            ..VfAttrs::default()
        },
    ];
    let outcome = fs.getattrsv_outcomes(&mut attrs);
    assert!(matches!(outcome.operations()[0], OpOutcome::Success(_)));
    assert!(matches!(outcome.operations()[1], OpOutcome::Failed(_)));
    assert!(matches!(outcome.operations()[2], OpOutcome::NotAttempted));
}

#[test]
fn native_scalar_contract_separates_metadata_query_from_update() {
    use vnfs::{
        AttrMask, FileSystem, MetadataQuery, OpenFlags, OpenRequest, SetAttributes, VfFile,
    };

    let mut fs = dummy();
    let file = FileSystem::open_one(
        &mut fs,
        &OpenRequest::new("/metadata", OpenFlags::WRITE | OpenFlags::CREATE),
    )
    .unwrap();
    FileSystem::close_one(&mut fs, &file).unwrap();

    let attrs = FileSystem::metadata(
        &mut fs,
        MetadataQuery::new(
            VfFile::from_path("/metadata"),
            AttrMask::MODE | AttrMask::SIZE,
        ),
    )
    .unwrap();
    assert!(attrs.returned.contains(AttrMask::MODE | AttrMask::SIZE));

    let mut update = SetAttributes::new(VfFile::from_path("/metadata"));
    update.mode = Some(0o640);
    FileSystem::set_attributes(&mut fs, update).unwrap();
    assert_eq!(fs.stat_path("/metadata").unwrap().mode & 0o777, 0o640);
}
