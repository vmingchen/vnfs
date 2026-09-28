//! Tests for the `std::fs`-backed [`DummyVecFs`]. These need no NFS server:
//! the suite runs against a temporary directory, proving the `VecFs` API
//! works on non-NFS filesystems too.

use vfsi_sync::test_support as common;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use vnfs::backend::DummyVecFs;
use vnfs::backend::{VecFs, VecFsExt, VfOffset};

#[cfg(feature = "test-faults")]
use std::sync::Arc;
#[cfg(feature = "test-faults")]
use vnfs::backend::internal::faults::{FaultScript, OpenFaultPoint};

/// A `DummyVecFs` rooted at a fresh unique temp directory.
fn dummy() -> DummyVecFs {
    static NEXT_DUMMY_ID: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "vnfs_dummy_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT_DUMMY_ID.fetch_add(1, Ordering::Relaxed)
    ));
    DummyVecFs::new(root)
}

#[test]
fn one_shot_file_vectors_roundtrip_and_limit_allocations() {
    use vnfs::{FsClient, ReadAllOptions};

    let client = FsClient::new(dummy());
    client
        .write_files(&[
            ("/file-1", b"hello".as_slice()),
            ("/file-2", b"world".as_slice()),
        ])
        .unwrap();
    assert_eq!(
        client.read_files(&["/file-1", "/file-2"]).unwrap(),
        vec![b"hello".to_vec(), b"world".to_vec()]
    );
    let error = client
        .read_files_with_options(
            &["/file-1", "/file-2"],
            ReadAllOptions::new().max_total_bytes(9),
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(error.path(), Some(Path::new("/file-2")));
    // The failed bounded read leaves the client usable.
    assert_eq!(
        client.read_files(&["/file-1"]).unwrap(),
        vec![b"hello".to_vec()]
    );
}

#[test]
fn one_shot_file_vectors_handle_empty_batches_and_replace_files() {
    use vnfs::FsClient;

    let client = FsClient::new(dummy());
    client.write_files::<&str, &[u8]>(&[]).unwrap();
    assert!(client.read_files::<&str>(&[]).unwrap().is_empty());
    client
        .write_files(&[("/file", b"longer".as_slice())])
        .unwrap();
    client.write_files(&[("/file", b"x".as_slice())]).unwrap();
    assert_eq!(client.read_files(&["/file"]).unwrap(), vec![b"x".to_vec()]);

    let error = client
        .write_files(&[
            ("/file", b"first".as_slice()),
            ("/file", b"second".as_slice()),
        ])
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert_eq!(error.path(), Some(Path::new("/file")));
    assert_eq!(client.read_files(&["/file"]).unwrap(), vec![b"x".to_vec()]);
}

#[test]
fn application_directory_vectors_preserve_fields_and_limits() {
    use vnfs::{FsClient, MetadataFields, ReadDirOptions, WalkOptions};

    let client = FsClient::new(dummy());
    client.create_dir("/a").unwrap();
    client.create_dir("/b").unwrap();
    client.write("/a/one", b"1").unwrap();
    client.write("/b/two", b"22").unwrap();

    let fields = MetadataFields::MODE | MetadataFields::SIZE | MetadataFields::BLOCKS;
    let listed = client
        .read_dirs_with_options(&["/a", "/b"], fields, ReadDirOptions::new())
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].path, Path::new("/a"));
    assert_eq!(listed[0].entries[0].path(), Path::new("/a/one"));
    assert_eq!(listed[1].entries[0].metadata().len(), 2);
    assert!(listed[0].entries[0].metadata().mode().is_some());
    assert!(listed[0].entries[0].metadata().blocks().is_some());
    assert_eq!(listed[0].entries[0].metadata().device_id(), None);

    let error = client
        .read_dirs_with_options(&["/a", "/b"], fields, ReadDirOptions::new().max_entries(1))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    let repeated = client
        .read_dirs_with_options(&["/a", "/a"], fields, ReadDirOptions::new())
        .unwrap();
    assert_eq!(repeated.len(), 2);
    assert_eq!(repeated[0], repeated[1]);
    let error = client
        .read_dirs_with_options(&["/a", "/a"], fields, ReadDirOptions::new().max_entries(1))
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));

    let tree = client
        .walk_with_options("/", fields, WalkOptions::new())
        .unwrap();
    assert!(
        tree.iter()
            .any(|directory| directory.path == Path::new("/a"))
    );
    assert!(
        tree.iter()
            .any(|directory| directory.path == Path::new("/b"))
    );
    let error = client
        .walk_with_options("/", fields, WalkOptions::new().max_entries(1))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
}

#[test]
fn application_metadata_and_batch_mutations() {
    use vnfs::{FsClient, MetadataFields};

    let client = FsClient::new(dummy());
    client.write("/source-1", b"abc").unwrap();
    client.write("/source-2", b"defg").unwrap();
    client.symlink("/source-1", "/link").unwrap();
    let metadata = client
        .symlink_metadata_with_fields("/link", MetadataFields::MODE | MetadataFields::BLOCKS)
        .unwrap();
    assert!(metadata.is_symlink());
    assert!(metadata.mode().is_some());
    assert_eq!(metadata.device_id(), None);

    client
        .copy_files(&[("/source-1", "/copy-1"), ("/source-2", "/copy-2")])
        .unwrap();
    assert_eq!(client.read("/copy-1").unwrap(), b"abc");
    assert_eq!(client.read("/copy-2").unwrap(), b"defg");
    client.remove_paths(&["/copy-1", "/copy-2"], false).unwrap();
    assert!(client.read("/copy-1").is_err());
}

#[test]
fn application_directory_cohorts_preserve_global_error_index() {
    use vnfs::{FsClient, MetadataFields, ReadDirOptions};

    let client = FsClient::new(dummy());
    let mut paths = Vec::new();
    for index in 0..32 {
        let path = format!("/d{index}");
        client.create_dir(&path).unwrap();
        paths.push(path);
    }
    paths.push(paths[0].clone());
    let listings = client
        .read_dirs_with_options(&paths, MetadataFields::MODE, ReadDirOptions::new())
        .unwrap();
    assert_eq!(listings[0], listings[32]);
    paths.push("/missing".to_string());
    let error = client
        .read_dirs_with_options(&paths, MetadataFields::MODE, ReadDirOptions::new())
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(33));
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
    use vnfs::backend::{ReadOp, VecFs, WriteOp};

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
fn single_file_stream_is_bounded_ordered_and_cancellable() {
    use vnfs::{FsClient, ReadStreamOptions, VfError};

    let client = FsClient::new(dummy());
    let payload: Vec<u8> = (0..(2 * 1024 * 1024 + 37))
        .map(|index| (index % 251) as u8)
        .collect();
    client.write("/stream", &payload).unwrap();

    let mut actual = Vec::new();
    let mut next_offset = 0u64;
    client
        .read_stream_with_options(
            "/stream",
            ReadStreamOptions::new().chunk_size(64 * 1024),
            |offset, chunk| {
                assert_eq!(offset, next_offset);
                assert!(!chunk.is_empty());
                assert!(chunk.len() <= 64 * 1024);
                next_offset += chunk.len() as u64;
                actual.extend_from_slice(chunk);
                Ok(true)
            },
        )
        .unwrap();
    assert_eq!(actual, payload);

    let mut default_bytes = 0usize;
    client
        .read_stream("/stream", |offset, chunk| {
            assert_eq!(offset, default_bytes as u64);
            assert!(chunk.len() <= vnfs::backend::DEFAULT_READ_STREAM_CHUNK_BYTES);
            default_bytes += chunk.len();
            Ok(true)
        })
        .unwrap();
    assert_eq!(default_bytes, payload.len());

    let mut seen = 0usize;
    client
        .read_stream_with_options(
            "/stream",
            ReadStreamOptions::new().chunk_size(1234),
            |_, chunk| {
                seen += chunk.len();
                Ok(false)
            },
        )
        .unwrap();
    assert_eq!(seen, 1234);

    let error = client
        .read_stream("/stream", |_, _| Err(VfError::client(0, libc::EIO as u32)))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EIO as u32);

    client.write("/empty", &[]).unwrap();
    let mut empty_callbacks = 0;
    client
        .read_stream("/empty", |_, _| {
            empty_callbacks += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(empty_callbacks, 0);

    let error = client
        .read_stream_with_options(
            "/does-not-exist",
            ReadStreamOptions::new().chunk_size(0),
            |_, _| Ok(true),
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EINVAL as u32);
}

#[test]
fn read_allv_default_rejects_more_than_sixteen_mibibytes() {
    use vnfs::FsClient;
    use vnfs::backend::{
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, VecFs, VfFile, VfOffset, WriteOp,
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
    use vnfs::backend::{ReadOp, VecFs, VfOffset};

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
    use vnfs::backend::{ReadOp, VecFs, VfOffset, WriteOp};

    let mut fs = dummy();
    let raw = b"n\xffb";
    let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec()));
    fs.writev(&[WriteOp::from_os_path(&path, VfOffset::At(0), b"data".to_vec()).with_creation()])
        .unwrap();
    let listed = fs
        .listdir(Path::new("/"), vnfs::backend::AttrMask::stat(), 0, false)
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
fn rm_contents_keeps_the_directory() {
    let mut fs = dummy();
    fs.mkdir_path("/keep", 0o755).unwrap();
    fs.mkdir_path("/keep/sub", 0o755).unwrap();
    for path in ["/keep/a", "/keep/sub/b"] {
        fs.writev(&[
            vnfs::backend::WriteOp::from_path(path, VfOffset::At(0), b"x".to_vec()).with_creation(),
        ])
        .unwrap();
    }
    fs.rm_contents_path("/keep").unwrap();
    assert!(fs.exists_path("/keep").unwrap(), "root is kept");
    assert!(
        fs.listdir(
            std::path::Path::new("/keep"),
            vnfs::backend::AttrMask::default(),
            0,
            false
        )
        .unwrap()
        .is_empty(),
        "contents are gone"
    );
}

#[test]
fn rm_contents_rejects_a_symlink_to_a_directory() {
    let mut fs = dummy();
    fs.mkdir_path("/target", 0o755).unwrap();
    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/target/keep", VfOffset::At(0), b"k".to_vec())
            .with_creation(),
    ])
    .unwrap();
    fs.symlink_path("target", "link").unwrap();

    assert!(fs.rm_contents_path("/link").is_err());
    assert!(fs.exists_path("/target/keep").unwrap());
}

#[test]
fn recursive_rm_removes_a_symlink_not_its_target() {
    let mut fs = dummy();
    fs.mkdir_path("/target", 0o755).unwrap();
    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/target/keep", VfOffset::At(0), b"k".to_vec())
            .with_creation(),
    ])
    .unwrap();
    fs.symlink_path("target", "link").unwrap();

    fs.rm(&[Path::new("/link")], true).unwrap();
    assert!(!fs.exists_path("/link").unwrap());
    assert!(fs.exists_path("/target/keep").unwrap());
}

#[test]
fn ensure_empty_dir_creates_empties_and_rejects_files() {
    let mut fs = dummy();
    fs.ensure_empty_dir_path("/made").unwrap();
    assert!(fs.exists_path("/made").unwrap());
    assert!(
        fs.listdir(
            Path::new("/made"),
            vnfs::backend::AttrMask::default(),
            0,
            false
        )
        .unwrap()
        .is_empty()
    );

    fs.mkdir_path("/full", 0o755).unwrap();
    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/full/child", VfOffset::At(0), b"x".to_vec())
            .with_creation(),
    ])
    .unwrap();
    fs.ensure_empty_dir_path("/full").unwrap();
    assert!(
        fs.listdir(
            Path::new("/full"),
            vnfs::backend::AttrMask::default(),
            0,
            false
        )
        .unwrap()
        .is_empty()
    );

    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/afile", VfOffset::At(0), b"f".to_vec()).with_creation(),
    ])
    .unwrap();
    assert!(fs.ensure_empty_dir_path("/afile").is_err());
    assert!(fs.exists_path("/afile").unwrap());
}

#[test]
fn open_dir_handle_empties_contents() {
    let mut fs = dummy();
    fs.mkdir_path("/d", 0o755).unwrap();
    fs.mkdir_path("/d/sub", 0o755).unwrap();
    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/d/a", VfOffset::At(0), b"a".to_vec()).with_creation(),
    ])
    .unwrap();
    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/d/sub/b", VfOffset::At(0), b"b".to_vec())
            .with_creation(),
    ])
    .unwrap();

    let handle = fs.open_dir(Path::new("/d")).unwrap();
    fs.rm_dir_contents(&handle).unwrap();
    fs.close_dir(&handle).unwrap();
    assert!(fs.exists_path("/d").unwrap());
    assert!(
        fs.listdir(
            Path::new("/d"),
            vnfs::backend::AttrMask::default(),
            0,
            false
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn generic_remover_rejects_options_it_cannot_honor() {
    let mut fs = dummy();
    fs.mkdir_path("/d", 0o755).unwrap();
    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/d/keep", VfOffset::At(0), b"x".to_vec())
            .with_creation(),
    ])
    .unwrap();
    let handle = fs.open_dir(Path::new("/d")).unwrap();
    for options in [
        vnfs::backend::RemoveOptions::new().continue_on_error(true),
        vnfs::backend::RemoveOptions::new().batch(2),
        vnfs::backend::RemoveOptions::new().retries(0),
    ] {
        assert!(
            fs.rm_with_options(&[Path::new("/d/keep")], false, options)
                .is_err()
        );
        assert!(
            fs.rm_contents_with_options(Path::new("/d"), options)
                .is_err()
        );
        assert!(fs.rm_dir_contents_with_options(&handle, options).is_err());
    }
    assert!(
        fs.exists_path("/d/keep").unwrap(),
        "rejected options must not mutate"
    );
    fs.close_dir(&handle).unwrap();
}

#[test]
fn standard_io_handle_is_raii_and_seekable() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use vnfs::backend::VfOpenOptions;

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
fn legacy_handle_try_close_leaves_an_inert_handle() {
    use std::io::{Read, Write};
    use vnfs::backend::VfOpenOptions;

    let mut fs = dummy();
    let mut options = VfOpenOptions::new();
    options.read(true).write(true).create(true);
    let mut file = options.open(&mut fs, "/try-close").unwrap();
    file.try_close().unwrap();
    file.try_close().unwrap();
    assert!(file.try_descriptor().is_err());
    assert_eq!(
        file.read(&mut [0u8; 1]).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        file.write(b"x").unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
fn standard_open_options_validate_access_modes() {
    use vnfs::backend::VfOpenOptions;

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
        vnfs::backend::ERR_INVAL
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
        vnfs::backend::ERR_NOTDIR
    );
    assert_eq!(
        client.remove_file("/tree/nested").unwrap_err().err_no(),
        vnfs::backend::ERR_ISDIR
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
    fs.writev(&[vnfs::backend::WriteOp::from_path(
        "/exists",
        VfOffset::At(0),
        b"existing".to_vec(),
    )
    .with_creation()])
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
        vnfs::backend::WriteOp::from_path("/kept", VfOffset::At(0), b"data".to_vec())
            .with_creation(),
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
    use vnfs::backend::VfFile;

    let mut fs = dummy();
    fs.writev(&[
        vnfs::backend::WriteOp::from_path("/first", VfOffset::At(0), Vec::new()).with_creation(),
        vnfs::backend::WriteOp::from_path("/third", VfOffset::At(0), Vec::new()).with_creation(),
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
    use vnfs::backend::{AttrMask, FileSystem, MetadataQuery, SetAttributes, VfFile};
    use vnfs::{OpenFlags, OpenRequest};

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
    use vnfs::backend::{AttrMask, WriteOp};
    use vnfs::{FsClient, ReadDirOptions, WalkOptions};

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

#[test]
fn directory_visitor_supports_limits_early_stop_and_callback_errors() {
    use vnfs::backend::WriteOp;
    use vnfs::{FsClient, ReadDirOptions, VfError};

    let mut fs = dummy();
    fs.ensure_dir(Path::new("/tree"), 0o755).unwrap();
    let writes: Vec<_> = (0..8)
        .map(|index| {
            WriteOp::from_path(&format!("/tree/item-{index}"), VfOffset::At(0), Vec::new())
                .with_creation()
        })
        .collect();
    fs.writev(&writes).unwrap();
    let client = FsClient::new(fs);

    let mut seen = 0;
    let error = client
        .visit_dir_with_options("/tree", ReadDirOptions::new().max_entries(3), |_| {
            seen += 1;
            Ok(true)
        })
        .unwrap_err();
    assert_eq!(seen, 3);
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let mut first = None;
    client
        .visit_dir_with_options("/tree", ReadDirOptions::unlimited(), |entry| {
            first = Some(entry.path().to_path_buf());
            Ok(false)
        })
        .unwrap();
    assert!(first.unwrap().starts_with("/tree"));

    let mut all = Vec::new();
    client
        .visit_dir_with_options("/tree", ReadDirOptions::unlimited(), |entry| {
            all.push(entry.path().to_path_buf());
            Ok(true)
        })
        .unwrap();
    assert_eq!(all.len(), 8);

    let error = client
        .visit_dir("/tree", |_| Err(VfError::client(0, libc::ECANCELED as u32)))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::ECANCELED as u32);

    let error = client
        .visit_dir_with_options("/tree", ReadDirOptions::new().max_path_bytes(1), |_| {
            panic!("over-budget entry must not reach the callback")
        })
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
}
