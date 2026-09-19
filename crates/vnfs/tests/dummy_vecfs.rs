//! Tests for the `std::fs`-backed [`DummyVecFs`]. These need no NFS server:
//! the suite runs against a temporary directory, proving the `VecFs` API
//! works on non-NFS filesystems too.

use vfsi_sync::test_support as common;

use std::path::Path;
use vnfs::dummy_vecfs::DummyVecFs;
use vnfs::{VecFs, VecFsExt, VfOffset};

#[cfg(feature = "test-faults")]
use std::sync::Arc;
#[cfg(feature = "test-faults")]
use vnfs::internal::faults::{FaultScript, OpenFaultPoint};

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
fn read_allv_default_rejects_more_than_sixteen_mibibytes() {
    use vnfs::{
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, FsClient, VecFs, VfFile,
        VfOffset, WriteOp,
    };

    let mut fs = dummy();
    let path = VfFile::from_path("/too-large");
    fs.writev(&[WriteOp::new(
        path.clone(),
        VfOffset::At(0),
        vec![0; DEFAULT_READ_ALLV_MAX_TOTAL_BYTES + 1],
    )
    .with_creation()])
        .unwrap();

    let error = fs.read_allv(&[path]).unwrap_err();
    assert_eq!(error.index_opt(), Some(0));
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    assert_eq!(DEFAULT_READ_MAX_BYTES, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    let client = FsClient::new(fs);
    let error = client.read("/too-large").unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(error.operation(), Some("read"));
}

#[test]
fn dummy_errors_on_missing_file() {
    use vnfs::{ReadOp, VecFs, VfOffset};

    let mut fs = dummy();
    fs.ensure_dir(Path::new("/data"), 0o755).unwrap();
    let res = fs.readv(&[ReadOp::from_path("/data/missing", VfOffset::At(0), 8)]);
    match res {
        Err(e) => {
            assert_eq!(e.index_opt(), Some(0));
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
        .writev(&[
            first.write_request_at(0, b"one"),
            second.write_request_at(0, b"two"),
        ])
        .unwrap();
    let read_results = client
        .readv(&[first.read_request_at(0, 3), second.read_request_at(0, 3)])
        .unwrap();
    assert_eq!(read_results[0].data, b"one");
    assert_eq!(read_results[1].data, b"two");
    let mut one_buffer = [0; 3];
    let mut two_buffer = [0; 3];
    let lengths = client
        .readv_into(&mut [
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
            .readv(&[first.read_request_at(0, 1)])
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
fn native_vector_operations_are_composable() {
    use vnfs::FsClient;

    let client = FsClient::new(dummy());
    let files = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .openv(&["/one", "/two"])
        .unwrap();

    let writes = client
        .writev(&[
            files[0].write_request_at(0, b"one"),
            files[1].write_request_at(0, b"two"),
        ])
        .unwrap();
    assert_eq!(writes.len(), 2);

    let reads = client
        .readv(&[
            files[0].read_request_at(0, 3),
            files[1].read_request_at(0, 3),
        ])
        .unwrap();
    let values = reads;
    assert_eq!(values[0].data, b"one");
    assert_eq!(values[1].data, b"two");
    client.closev(files).unwrap();

    client.write("/present", b"ok").unwrap();
    let error = client
        .open_options()
        .read(true)
        .openv(&["/present", "/missing", "/later"])
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_fault_before_dispatch_has_no_effects_or_handles() {
    let mut fs = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeDispatch { chunk: 0 },
        vnfs::VfError::transport(None, "injected pre-dispatch failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VecFs::openv(
        &mut fs,
        &[Path::new("/f0"), Path::new("/f1")],
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index_opt(), None);
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(fs.test_open_handle_count(), 0);
    assert!(!fs.exists_path("/f0").unwrap());
    assert!(!fs.exists_path("/f1").unwrap());
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_fault_injection_closes_the_successful_prefix() {
    let mut fs = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 2 },
        vnfs::VfError::transport(None, "injected registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VecFs::openv(
        &mut fs,
        &[Path::new("/f0"), Path::new("/f1"), Path::new("/f2")],
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap_err();
    assert_eq!(error.index_opt(), Some(2));
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(fs.test_open_handle_count(), 0);
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_fault_after_registration_closes_the_injected_handle() {
    let mut fs = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterRegister { index: 1 },
        vnfs::VfError::transport(None, "injected post-registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VecFs::openv(
        &mut fs,
        &[Path::new("/f0"), Path::new("/f1"), Path::new("/f2")],
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(fs.test_open_handle_count(), 0);
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_cleanup_fault_does_not_mask_primary_error_or_leak_handles() {
    let mut fs = dummy();
    fs.writev(&[
        vnfs::WriteOp::from_path("/exists", VfOffset::At(0), b"existing".to_vec()).with_creation(),
    ])
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCleanup { index: 0 },
        vnfs::VfError::transport(None, "injected cleanup failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VecFs::openv(
        &mut fs,
        &[Path::new("/created"), Path::new("/exists")],
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert_eq!(error.err_no(), libc::EEXIST as u32);
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(fs.test_open_handle_count(), 0);
}

#[cfg(feature = "test-faults")]
#[test]
fn recursive_remove_propagates_type_lookup_failure_without_unlinking() {
    let mut fs = dummy();
    fs.writev(&[
        vnfs::WriteOp::from_path("/kept", VfOffset::At(0), b"data".to_vec()).with_creation(),
    ])
    .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRemoveType { index: 0 },
        vnfs::VfError::transport(None, "injected type lookup failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs.rm(&[Path::new("/kept")], true).unwrap_err();
    assert!(error.is_transport());
    assert!(script.is_consumed());
    assert!(fs.exists_path("/kept").unwrap());
}

#[cfg(feature = "test-faults")]
#[test]
fn create_mode_failure_is_reported_instead_of_ignored() {
    let mut fs = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeSetPermissions { index: 0 },
        vnfs::VfError::failure(0, libc::EPERM as u32),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs
        .open(Path::new("/created"), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EPERM as u32);
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
}

#[cfg(feature = "test-faults")]
#[test]
fn mkdir_mode_failure_is_reported_instead_of_ignored() {
    let mut fs = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeSetPermissions { index: 0 },
        vnfs::VfError::failure(0, libc::EPERM as u32),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs.mkdir(Path::new("/created-dir"), 0o700).unwrap_err();
    assert_eq!(error.err_no(), libc::EPERM as u32);
    assert!(script.is_consumed());
}

#[test]
fn strict_vectors_report_failure_index_without_rollback() {
    use vnfs::VfFile;

    let mut fs = dummy();
    fs.writev(&[
        vnfs::WriteOp::from_path("/first", VfOffset::At(0), Vec::new()).with_creation(),
        vnfs::WriteOp::from_path("/third", VfOffset::At(0), Vec::new()).with_creation(),
    ])
    .unwrap();
    let error = fs
        .removev(&[
            VfFile::from_path("/first"),
            VfFile::from_path("/missing"),
            VfFile::from_path("/third"),
        ])
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert!(!fs.exists_path("/first").unwrap());
    assert!(fs.exists_path("/third").unwrap());
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

#[test]
fn allocating_directory_apis_enforce_entry_path_and_depth_limits() {
    use vnfs::{AttrMask, FsClient, ReadDirOptions, WalkOptions, WriteOp};

    let mut fs = dummy();
    fs.ensure_dir(Path::new("/tree/sub"), 0o755).unwrap();
    fs.writev(&[
        WriteOp::from_path("/tree/one", VfOffset::At(0), Vec::new()).with_creation(),
        WriteOp::from_path("/tree/two", VfOffset::At(0), Vec::new()).with_creation(),
    ])
    .unwrap();

    let error = fs
        .walk_with_options(
            Path::new("/tree"),
            AttrMask::stat(),
            WalkOptions::new().max_entries(2),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let error = fs
        .walk_with_options(
            Path::new("/tree"),
            AttrMask::stat(),
            WalkOptions::new().max_depth(0),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let client = FsClient::new(fs);
    let error = client
        .read_dir_with_options("/tree", ReadDirOptions::new().max_entries(1))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let error = client
        .read_dir_with_options("/tree", ReadDirOptions::new().max_path_bytes(1))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    assert_eq!(client.read_dir("/tree").unwrap().len(), 3);
}
