//! Tests for the `std::fs`-backed [`LocalBackend`]. These need no NFS server:
//! the suite runs against a temporary directory, proving the `VectorBackend` API
//! works on non-NFS filesystems too.

use vfsi_sync::backend::{HandleBackend, VectorBackend};
use vfsi_sync::test_support as common;
use vfsi_sync::test_support::borrow_writes;
use vfsi_sync::*;

use std::path::Path;
use tempfile::TempDir;
use vfsi_local::LocalBackend;
use vfsi_sync::VfOffset;

#[test]
fn application_walk_prunes_before_io_selects_metadata_and_allows_reentry() {
    if common::supervise_with_deadline(
        "application_walk_prunes_before_io_selects_metadata_and_allows_reentry",
    ) {
        return;
    }
    let (_root, backend) = dummy();
    let client = vfsi_sync::FsClient::new(backend);
    client.create_dir_all("/blocked").unwrap();
    client.write("/blocked/hidden", b"hidden").unwrap();
    client.write("/a", b"hello").unwrap();
    let mut visited = Vec::new();
    client
        .listdir(
            "/",
            vfsi_core::api::ListDirOptions::default()
                .recursive(true)
                .fields(vnfs::directory::Attributes::MODE)
                .enter_leave(true)
                .sort_by_name(true),
            |event| {
                visited.push(event.entry.path().to_path_buf());
                assert!(event.entry.attrs().uid().is_none());
                assert!(event.entry.attrs().modified().is_none());
                client.attrs("/a").unwrap();
                if event.kind == vnfs::directory::WalkEventKind::Enter
                    && event.entry.path() == Path::new("/blocked")
                {
                    client.remove_dir_all("/blocked").unwrap();
                    Ok(vnfs::directory::WalkControl::SkipSubtree)
                } else {
                    Ok(vnfs::directory::WalkControl::Continue)
                }
            },
        )
        .unwrap();
    assert_eq!(
        visited,
        ["/", "/a", "/blocked", "/blocked", "/"].map(std::path::PathBuf::from)
    );
}

#[test]
fn bounded_open_file_collection_keeps_identity_across_rename_and_replacement() {
    use std::io::Read;
    let (_root, backend) = dummy();
    let client =
        vfsi_sync::FsClient::new(backend).with_limits(ResourceLimits::new().max_read_bytes(6));
    client.write("/original", b"opened").unwrap();
    let file = client.open("/original").unwrap();
    client.rename("/original", "/moved").unwrap();
    client.write("/original", b"replacement").unwrap();
    let mut io = client.std_io(&file);
    let mut output = Vec::new();
    assert_eq!(io.read_to_end(&mut output).unwrap(), 6);
    assert_eq!(output, b"opened");
    assert_eq!(io.read_to_end(&mut output).unwrap(), 0);
    let limited = client
        .clone()
        .with_limits(ResourceLimits::new().max_read_bytes(5));
    assert_eq!(
        limited
            .std_io(&file)
            .read_to_end(&mut Vec::new())
            .unwrap_err()
            .kind(),
        vnfs::error::ErrorKind::FileTooLarge
    );
    drop(io);
    file.close().unwrap();
    assert_eq!(
        std::fs::read(_root.path().join("original")).unwrap(),
        b"replacement"
    );
}

#[test]
fn native_paged_tree_honors_budget_overrides_and_allows_reentry() {
    if common::supervise_with_deadline(
        "native_paged_tree_honors_budget_overrides_and_allows_reentry",
    ) {
        return;
    }
    use vfsi_sync::FsClient;
    use vnfs::directory::{ListDirOptions, TraversalCompletion};
    use vnfs::files::ResourceLimits;
    let (_root, backend) = dummy();
    let client = FsClient::new(backend).with_limits(ResourceLimits::new().max_directory_entries(3));
    client.create_dir_all("/tree/sub").unwrap();
    client.write("/tree/a", b"a").unwrap();
    client.write("/tree/sub/b", b"b").unwrap();
    client.symlink("sub", "/tree/link").unwrap();
    let mut seen = Vec::new();
    let clone = client.clone();
    let error = client
        .listdir(
            "/tree",
            vfsi_core::api::ListDirOptions::new().recursive(true),
            |entry| {
                clone.symlink_attrs(entry.entry.path()).unwrap();
                seen.push(entry.entry.path().to_path_buf());
                Ok(vfsi_core::api::WalkControl::Continue)
            },
        )
        .unwrap_err();
    assert_eq!(error.kind(), vnfs::error::ErrorKind::FileTooLarge);
    assert_eq!(seen.len(), 3);
    seen.clear();
    let options = ListDirOptions::new().recursive(true).max_entries(10);
    assert_eq!(
        client
            .listdir("/tree", options, |entry| {
                seen.push(entry.entry.path().to_path_buf());
                Ok(vfsi_core::api::WalkControl::Continue)
            })
            .unwrap(),
        TraversalCompletion::Complete
    );
    assert_eq!(seen.len(), 4);
    seen.sort();
    assert_eq!(
        seen,
        ["/tree/a", "/tree/link", "/tree/sub", "/tree/sub/b"].map(std::path::PathBuf::from)
    );
    client.remove_dir_all("/tree").unwrap();
}

#[cfg(feature = "test-faults")]
use std::sync::Arc;
#[cfg(feature = "test-faults")]
use vfsi_sync::internal::faults::{FaultScript, OpenFaultPoint};

/// A `LocalBackend` rooted at a fresh unique temp directory.
fn dummy() -> (TempDir, LocalBackend) {
    let root = TempDir::new().unwrap();
    let backend = vfsi_posix::backend(root.path()).unwrap();
    (root, backend)
}

#[test]
fn one_shot_file_vectors_roundtrip_and_bound_returned_bytes() {
    use vfsi_sync::FsClient;
    use vfsi_sync::ReadOptions;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
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
            ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(9)),
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
    use vfsi_sync::FsClient;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
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
    assert_eq!(error.index(), Some(1));
    assert_eq!(error.path(), Some(Path::new("/file")));
    assert_eq!(client.read_files(&["/file"]).unwrap(), vec![b"x".to_vec()]);
}

#[test]
fn application_directory_vectors_preserve_fields_and_limits() {
    use vfsi_sync::FsClient;
    use vnfs::directory::{Attributes, ListDirOptions};

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    client.create_dir("/a").unwrap();
    client.create_dir("/b").unwrap();
    client.write("/a/one", b"1").unwrap();
    client.write("/b/two", b"22").unwrap();

    let fields = Attributes::MODE | Attributes::SIZE | Attributes::BLOCKS;
    let listed = client
        .read_dirs_with_options(&["/a", "/b"], ListDirOptions::new().fields(fields))
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0][0].path, Path::new("/a"));
    assert_eq!(listed[0][0].entries[0].path(), Path::new("/a/one"));
    assert_eq!(listed[1][0].entries[0].attrs().len().unwrap(), 2);
    assert!(listed[0][0].entries[0].attrs().mode().is_some());
    assert!(listed[0][0].entries[0].attrs().blocks().is_some());
    assert_eq!(listed[0][0].entries[0].attrs().device_id(), None);

    let error = client
        .read_dirs_with_options(
            &["/a", "/b"],
            ListDirOptions::new().fields(fields).max_entries(1),
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    let repeated = client
        .read_dirs_with_options(&["/a", "/a"], ListDirOptions::new().fields(fields))
        .unwrap();
    assert_eq!(repeated.len(), 2);
    assert_eq!(repeated[0], repeated[1]);
    let error = client
        .read_dirs_with_options(
            &["/a", "/a"],
            ListDirOptions::new().fields(fields).max_entries(1),
        )
        .unwrap_err();
    assert_eq!(error.index(), Some(1));

    let tree = client
        .read_dirs_with_options(&["/"], ListDirOptions::new().fields(fields).recursive(true))
        .map(|mut trees| trees.remove(0))
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
        .read_dirs_with_options(
            &["/"],
            ListDirOptions::new()
                .max_entries(1)
                .fields(fields)
                .recursive(true),
        )
        .map(|mut trees| trees.remove(0))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
}

#[test]
fn application_metadata_and_batch_mutations() {
    use vfsi_sync::FsClient;
    use vnfs::directory::Attributes;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    client.write("/source-1", b"abc").unwrap();
    client.write("/source-2", b"defg").unwrap();
    client.symlink("/source-1", "/link").unwrap();
    let metadata = client
        .attrs_with_options(
            "/link",
            vnfs::directory::AttrsOptions::new()
                .fields(Attributes::MODE | Attributes::BLOCKS)
                .follow_symlinks(false),
        )
        .unwrap();
    assert!(metadata.is_symlink());
    assert!(metadata.mode().is_some());
    assert_eq!(metadata.device_id(), None);

    client
        .vcopy(
            &[("/source-1", "/copy-1"), ("/source-2", "/copy-2")],
            vnfs::files::CopyOption::default(),
        )
        .unwrap();
    assert_eq!(client.read("/copy-1").unwrap(), b"abc");
    assert_eq!(client.read("/copy-2").unwrap(), b"defg");
    client
        .copy_with_options(
            "/link",
            "/link-copy",
            vnfs::files::CopyOption::new().follow_source_symlinks(false),
        )
        .unwrap();
    assert_eq!(
        client.read_link("/link-copy").unwrap(),
        Path::new("/source-1")
    );
    assert!(client.symlink_attrs("/link-copy").unwrap().is_symlink());
    client
        .vremove(
            &["/copy-1", "/copy-2"],
            vnfs::directory::RemoveMode::Entry,
            Default::default(),
        )
        .unwrap();
    assert!(client.read("/copy-1").is_err());
}

#[test]
fn application_directory_cohorts_preserve_global_error_index() {
    use vfsi_sync::FsClient;
    use vnfs::directory::Attributes;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    let mut paths = Vec::new();
    for index in 0..32 {
        let path = format!("/d{index}");
        client.create_dir(&path).unwrap();
        paths.push(path);
    }
    paths.push(paths[0].clone());
    let listings = client
        .read_dirs_with_options(&paths, ListDirOptions::new().fields(Attributes::MODE))
        .unwrap();
    assert_eq!(listings[0], listings[32]);
    paths.push("/missing".to_string());
    let error = client
        .read_dirs_with_options(&paths, ListDirOptions::new().fields(Attributes::MODE))
        .unwrap_err();
    assert_eq!(error.index(), Some(33));
}

#[test]
fn dummy_full_suite() {
    let (_root, mut fs) = dummy();
    common::run_suite(&mut fs, "/");
}

#[test]
fn dummy_getcwd() {
    let (_root, mut fs) = dummy();
    assert_eq!(fs.getcwd(), Path::new("/"));
    fs.ensure_dir_impl(Path::new("/a/b"), 0o755).unwrap();
    fs.chdir(Path::new("/a/b")).unwrap();
    assert_eq!(fs.getcwd(), Path::new("/a/b"));
}

#[test]
fn single_file_stream_is_bounded_ordered_and_cancellable() {
    use vfsi_sync::FsClient;
    use vnfs::Error as VfError;
    use vnfs::files::StreamOptions;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    let payload: Vec<u8> = (0..(2 * 1024 * 1024 + 37))
        .map(|index| (index % 251) as u8)
        .collect();
    client.write("/stream", &payload).unwrap();

    let mut actual = Vec::new();
    let mut next_offset = 0u64;
    client
        .read_stream_with_options(
            "/stream",
            StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(64 * 1024).unwrap()),
            |offset, chunk| {
                assert_eq!(offset, next_offset);
                assert!(!chunk.is_empty());
                assert!(chunk.len() <= 64 * 1024);
                next_offset += chunk.len() as u64;
                actual.extend_from_slice(chunk);
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .unwrap();
    assert_eq!(actual, payload);

    let mut default_bytes = 0usize;
    client
        .read_stream("/stream", |offset, chunk| {
            assert_eq!(offset, default_bytes as u64);
            assert!(chunk.len() <= vfsi_sync::DEFAULT_READ_STREAM_CHUNK_BYTES);
            default_bytes += chunk.len();
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    assert_eq!(default_bytes, payload.len());

    let mut seen = 0usize;
    client
        .read_stream_with_options(
            "/stream",
            StreamOptions::new().chunk_size(std::num::NonZeroUsize::new(1234).unwrap()),
            |_, chunk| {
                seen += chunk.len();
                Ok(std::ops::ControlFlow::Break(()))
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
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    assert_eq!(empty_callbacks, 0);
}

#[test]
fn read_allv_default_rejects_more_than_sixteen_mibibytes() {
    use vfsi_sync::FsClient;
    use vfsi_sync::{
        DEFAULT_READ_ALLV_MAX_TOTAL_BYTES, DEFAULT_READ_MAX_BYTES, VfFile, VfOffset, WriteOp,
    };

    let (_root, mut fs) = dummy();
    let path = VfFile::from_path("/too-large");
    fs.vwrite_impl(&borrow_writes(&[WriteOp::new(
        path.clone(),
        VfOffset::At(0),
        vec![0; DEFAULT_READ_ALLV_MAX_TOTAL_BYTES + 1],
    )
    .with_creation()]))
        .unwrap();

    let error = fs.vread_all_impl(&[path]).unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    assert_eq!(DEFAULT_READ_MAX_BYTES, DEFAULT_READ_ALLV_MAX_TOTAL_BYTES);
    let client = FsClient::new(fs);
    let error = client.read("/too-large").unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);
    assert_eq!(error.operation(), Some("read"));
}

#[test]
fn dummy_errors_on_missing_file() {
    use vfsi_sync::{ReadOp, VfOffset};

    let (_root, mut fs) = dummy();
    fs.ensure_dir_impl(Path::new("/data"), 0o755).unwrap();
    let res = fs.vread_impl(&[ReadOp::from_path("/data/missing", VfOffset::At(0), 8)]);
    match res {
        Err(e) => {
            assert_eq!(e.index(), Some(0));
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
    let mut fs = vfsi_posix::backend(root.clone()).unwrap();

    fs.ensure_dir_impl(Path::new(&format!("/{}", name)), 0o755)
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
    use vfsi_sync::{ReadOp, VfOffset, WriteOp};

    let (_root, mut fs) = dummy();
    let raw = b"n\xffb";
    let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec()));
    fs.vwrite_impl(&borrow_writes(&[WriteOp::from_os_path(
        &path,
        VfOffset::At(0),
        b"data".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    let listed = fs
        .listdir_impl(Path::new("/"), vfsi_sync::AttrMask::stat(), 0, false)
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
        .vread_impl(&[ReadOp::from_os_path(&path, VfOffset::At(0), 4)])
        .unwrap()[0];
    assert_eq!(r.data, b"data");
}

#[test]
fn native_path_adapters_preserve_relative_paths() {
    let (_root, mut fs) = dummy();
    fs.mkdir_raw_impl(Path::new("/x"), 0o755).unwrap();
    assert!(fs.exists_impl(Path::new("/x")).unwrap());
    assert_eq!(
        fs.stat_impl(Path::new("/x")).unwrap().ftype,
        vnfs::directory::FileType::Directory
    );
    fs.chdir(Path::new("/x")).unwrap();
    fs.symlink_raw_impl(Path::new("missing"), Path::new("dangling"))
        .unwrap();
    assert!(fs.exists_impl(Path::new("dangling")).unwrap());
    fs.unlink_impl(Path::new("dangling")).unwrap();
    vfsi_sync::backend::helpers::remove_tree(&mut fs, Path::new("/x")).unwrap();
}

#[test]
fn rm_contents_keeps_the_directory() {
    let (_root, mut fs) = dummy();
    fs.mkdir_raw_impl(Path::new("/keep"), 0o755).unwrap();
    fs.mkdir_raw_impl(Path::new("/keep/sub"), 0o755).unwrap();
    for path in ["/keep/a", "/keep/sub/b"] {
        fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
            path,
            VfOffset::At(0),
            b"x".to_vec(),
        )
        .with_creation()]))
            .unwrap();
    }
    fs.remove_dir_contents_path_impl(Path::new("/keep"))
        .unwrap();
    assert!(fs.exists_impl(Path::new("/keep")).unwrap(), "root is kept");
    assert!(
        fs.listdir_impl(
            std::path::Path::new("/keep"),
            vfsi_sync::AttrMask::default(),
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
    let (_root, mut fs) = dummy();
    fs.mkdir_raw_impl(Path::new("/target"), 0o755).unwrap();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/target/keep",
        VfOffset::At(0),
        b"k".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    fs.symlink_raw_impl(Path::new("target"), Path::new("link"))
        .unwrap();

    assert!(
        fs.remove_dir_contents_path_impl(Path::new("/link"))
            .is_err()
    );
    assert!(fs.exists_impl(Path::new("/target/keep")).unwrap());
}

#[test]
fn recursive_rm_removes_a_symlink_not_its_target() {
    let (_root, mut fs) = dummy();
    fs.mkdir_raw_impl(Path::new("/target"), 0o755).unwrap();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/target/keep",
        VfOffset::At(0),
        b"k".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    fs.symlink_raw_impl(Path::new("target"), Path::new("link"))
        .unwrap();

    fs.remove_paths_impl(&[Path::new("/link")], true).unwrap();
    assert!(!fs.exists_impl(Path::new("/link")).unwrap());
    assert!(fs.exists_impl(Path::new("/target/keep")).unwrap());
}

#[test]
fn ensure_empty_dir_creates_empties_and_rejects_files() {
    let (_root, mut fs) = dummy();
    fs.ensure_empty_dir_impl(Path::new("/made")).unwrap();
    assert!(fs.exists_impl(Path::new("/made")).unwrap());
    assert!(
        fs.listdir_impl(Path::new("/made"), vfsi_sync::AttrMask::default(), 0, false)
            .unwrap()
            .is_empty()
    );

    fs.mkdir_raw_impl(Path::new("/full"), 0o755).unwrap();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/full/child",
        VfOffset::At(0),
        b"x".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    fs.ensure_empty_dir_impl(Path::new("/full")).unwrap();
    assert!(
        fs.listdir_impl(Path::new("/full"), vfsi_sync::AttrMask::default(), 0, false)
            .unwrap()
            .is_empty()
    );

    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/afile",
        VfOffset::At(0),
        b"f".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    assert!(fs.ensure_empty_dir_impl(Path::new("/afile")).is_err());
    assert!(fs.exists_impl(Path::new("/afile")).unwrap());
}

#[test]
fn open_dir_handle_empties_contents() {
    let (_root, mut fs) = dummy();
    fs.mkdir_raw_impl(Path::new("/d"), 0o755).unwrap();
    fs.mkdir_raw_impl(Path::new("/d/sub"), 0o755).unwrap();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/d/a",
        VfOffset::At(0),
        b"a".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/d/sub/b",
        VfOffset::At(0),
        b"b".to_vec(),
    )
    .with_creation()]))
        .unwrap();

    let handle = fs.open_dir_impl(Path::new("/d")).unwrap();
    fs.remove_dir_contents_handle_impl(&handle).unwrap();
    fs.close_dir_impl(&handle).unwrap();
    assert!(fs.exists_impl(Path::new("/d")).unwrap());
    assert!(
        fs.listdir_impl(Path::new("/d"), vfsi_sync::AttrMask::default(), 0, false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn owned_directory_handle_refuses_path_only_backend() {
    use vfsi_sync::FsClient;
    use vnfs::directory::RemoveOptions;
    use vnfs::files::VfsiExt;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    client.create_dir("/d").unwrap();
    client.write("/d/keep", b"x").unwrap();
    let error = client.open_dir_handle("/d").unwrap_err();
    assert_eq!(error.err_no(), vfsi_sync::VF_ERR_UNSUPPORTED);
    assert_eq!(client.read("/d/keep").unwrap(), b"x");

    let options = RemoveOptions::new().continue_on_error(true);
    assert!(VfsiExt::remove_dir_contents_with_options(&client, "/d", options).is_err());
    assert!(VfsiExt::remove_dir_all_with_options(&client, "/d", options).is_err());
    assert!(client.vremove_impl(&["/d/keep"], false, options).is_err());
    assert_eq!(client.read("/d/keep").unwrap(), b"x");
    VfsiExt::remove_dir_all_with_options(&client, "/d", RemoveOptions::default()).unwrap();
}

#[test]
fn generic_remover_rejects_options_it_cannot_honor() {
    let (_root, mut fs) = dummy();
    fs.mkdir_raw_impl(Path::new("/d"), 0o755).unwrap();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/d/keep",
        VfOffset::At(0),
        b"x".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    let handle = fs.open_dir_impl(Path::new("/d")).unwrap();
    for options in [
        vfsi_sync::RemoveOptions::new().continue_on_error(true),
        vfsi_sync::RemoveOptions::new().batch(2),
        vfsi_sync::RemoveOptions::new().retries(0),
    ] {
        assert!(
            fs.remove_paths_with_options_impl(&[Path::new("/d/keep")], false, options)
                .is_err()
        );
        assert!(
            fs.remove_dir_contents_path_with_options_impl(Path::new("/d"), options)
                .is_err()
        );
        assert!(
            fs.remove_dir_contents_handle_with_options_impl(&handle, options)
                .is_err()
        );
    }
    assert!(
        fs.exists_impl(Path::new("/d/keep")).unwrap(),
        "rejected options must not mutate"
    );
    fs.close_dir_impl(&handle).unwrap();
}

#[test]
fn standard_io_adapter_borrows_an_owned_raii_handle() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    {
        let file = client
            .open_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open("/standard-io")
            .unwrap();
        {
            let mut io = client.std_io(&file);
            io.write_all(b"abcdef").unwrap();
            io.seek(SeekFrom::Start(2)).unwrap();
            let mut out = [0; 3];
            io.read_exact(&mut out).unwrap();
            assert_eq!(&out, b"cde");
        }
        assert!(!file.is_closed());
        assert_eq!(client.attrs(Target::file(&file)).unwrap().len(), Some(6));
    }
    // Dropped handles release their ownership and allow deferred close to drain.
    assert!(client.into_inner().is_ok());
}

#[test]
fn closed_handle_rejects_standard_io() {
    use std::io::{Read, Write};
    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    let mut file = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .open("/try-close")
        .unwrap();
    file.try_close().unwrap();
    file.try_close().unwrap();
    assert!(file.is_closed());
    for error in [
        client.std_io(&file).read(&mut [0u8; 1]).unwrap_err(),
        client.std_io(&file).write(b"x").unwrap_err(),
    ] {
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<VfError>()
                .unwrap()
                .err_no(),
            libc::EBADF as u32
        );
    }
}

#[test]
fn standard_open_options_validate_access_modes() {
    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    assert_eq!(
        client.open_options().open("/invalid").unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        client
            .open_options()
            .read(true)
            .truncate(true)
            .open("/invalid")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        client
            .open_options()
            .read(true)
            .open("/missing")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn owned_client_supports_multiple_live_files_and_typed_requests() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use vfsi_sync::FsClient;
    use vnfs::files::{OpenFlags, OpenOp};

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let first = client.open_with(OpenOp::new("/first", flags)).unwrap();
    let second = client.open_with(OpenOp::new("/second", flags)).unwrap();

    client
        .vwrite_native(&[
            first.write_request_at(0, b"one"),
            second.write_request_at(0, b"two"),
        ])
        .unwrap();
    let read_results = client
        .vread_native(&[first.read_request_at(0, 3), second.read_request_at(0, 3)])
        .unwrap();
    assert_eq!(read_results[0].data, b"one");
    assert_eq!(read_results[1].data, b"two");
    let mut one_buffer = [0; 3];
    let mut two_buffer = [0; 3];
    let lengths = client
        .vread_into_native(&mut [
            first.read_request_at_into(0, &mut one_buffer),
            second.read_request_at_into(0, &mut two_buffer),
        ])
        .unwrap();
    assert_eq!(
        lengths.iter().map(|result| result.read).collect::<Vec<_>>(),
        [3, 3]
    );
    assert_eq!(&one_buffer, b"one");
    assert_eq!(&two_buffer, b"two");
    let (_other_root, other_backend) = dummy();
    let other_client = FsClient::new(other_backend);
    assert_eq!(
        other_client
            .vread_native(&[first.read_request_at(0, 1)])
            .unwrap_err()
            .err_no(),
        vfsi_sync::ERR_INVAL
    );
    let mut first_io = client.std_io(&first);
    let mut second_io = client.std_io(&second);
    first_io.flush().unwrap();
    second_io.flush().unwrap();
    first_io.seek(SeekFrom::Start(0)).unwrap();
    second_io.seek(SeekFrom::Start(0)).unwrap();
    let mut one = String::new();
    let mut two = String::new();
    first_io.read_to_string(&mut one).unwrap();
    second_io.read_to_string(&mut two).unwrap();
    assert_eq!((one.as_str(), two.as_str()), ("one", "two"));

    drop(first_io);
    drop(second_io);
    first.close().unwrap();
    second.close().unwrap();
}

#[test]
fn native_client_covers_idiomatic_file_and_namespace_workflows() {
    use vfsi_sync::FsClient;
    use vnfs::directory::FileType as VfType;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    client.create_dir_all("/tree/nested").unwrap();
    client.create_dir_all("/../../root-clamped").unwrap();
    assert!(client.attrs("/root-clamped").unwrap().is_dir());
    client.remove_dir("/root-clamped").unwrap();
    client.write("/tree/nested/file", b"hello").unwrap();
    assert_eq!(client.read_to_string("/tree/nested/file").unwrap(), "hello");

    let metadata = client.attrs("/tree/nested/file").unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.len(), Some(5));
    assert!(!metadata.permissions().unwrap().readonly());

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
    assert_eq!(
        client
            .vwrite(
                &[vnfs::files::WriteOp::at(&file, 1, b"a")],
                Default::default()
            )
            .unwrap()[0]
            .written,
        1
    );
    let mut bytes = [0; 5];
    assert_eq!(
        client
            .vread(
                [vnfs::files::ReadOp::into(&file, 0, &mut bytes)],
                Default::default()
            )
            .unwrap()[0]
            .read(),
        5
    );
    assert_eq!(&bytes, b"hallo");
    assert_eq!(
        client
            .attrs(vfsi_core::Target::file(&file))
            .unwrap()
            .len()
            .unwrap(),
        5
    );
    client.truncate(vfsi_core::Target::file(&file), 4).unwrap();
    client
        .chmod(
            vfsi_core::Target::file(&file),
            vnfs::directory::Permissions::from_mode(0o600),
        )
        .unwrap();
    let metadata = client.attrs(vfsi_core::Target::file(&file)).unwrap();
    assert_eq!(metadata.len(), Some(4));
    assert_eq!(metadata.permissions().unwrap().mode(), 0o600);
    file.close().unwrap();
    client
        .vsetattrs(&[vnfs::directory::SetAttrsOp::new("/tree/nested/file")
            .permissions(vnfs::directory::Permissions::from_mode(0o400))
            .len(4)])
        .unwrap();
    let metadata = client.attrs("/tree/nested/file").unwrap();
    assert_eq!(metadata.len(), Some(4));
    assert_eq!(metadata.permissions().unwrap().mode(), 0o400);

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
            .symlink_attrs("/tree/nested/symlink")
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
        vfsi_sync::ERR_NOTDIR
    );
    assert_eq!(
        client.remove_file("/tree/nested").unwrap_err().err_no(),
        vfsi_sync::ERR_ISDIR
    );
    client.remove_dir_all("/tree").unwrap();
}

#[test]
fn native_vector_operations_are_composable() {
    use vfsi_sync::FsClient;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    let files = client
        .open_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .vopen(&["/one", "/two"])
        .unwrap();

    let writes = client
        .vwrite_native(&[
            files[0].write_request_at(0, b"one"),
            files[1].write_request_at(0, b"two"),
        ])
        .unwrap();
    assert_eq!(writes.len(), 2);

    let reads = client
        .vread_native(&[
            files[0].read_request_at(0, 3),
            files[1].read_request_at(0, 3),
        ])
        .unwrap();
    let values = reads;
    assert_eq!(values[0].data, b"one");
    assert_eq!(values[1].data, b"two");
    client.vclose_owned(files).unwrap();

    client.write("/present", b"ok").unwrap();
    let error = client
        .open_options()
        .read(true)
        .vopen(&["/present", "/missing", "/later"])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_fault_before_dispatch_has_no_effects_or_handles() {
    let (_root, mut fs) = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeDispatch { chunk: 0 },
        vnfs::Error::transport(None, "injected pre-dispatch failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VectorBackend::vopen_raw_impl(
        &mut fs,
        &[Path::new("/f0"), Path::new("/f1")],
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index(), None);
    assert!(
        script.is_consumed(),
        "unused faults: {:?}",
        script.remaining()
    );
    assert_eq!(fs.test_open_handle_count(), 0);
    assert!(!fs.exists_impl(Path::new("/f0")).unwrap());
    assert!(!fs.exists_impl(Path::new("/f1")).unwrap());
}

#[cfg(feature = "test-faults")]
#[test]
fn openv_fault_injection_closes_the_successful_prefix() {
    let (_root, mut fs) = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRegister { index: 2 },
        vnfs::Error::transport(None, "injected registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VectorBackend::vopen_raw_impl(
        &mut fs,
        &[Path::new("/f0"), Path::new("/f1"), Path::new("/f2")],
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(2));
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
    let (_root, mut fs) = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::AfterRegister { index: 1 },
        vnfs::Error::transport(None, "injected post-registration failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VectorBackend::vopen_raw_impl(
        &mut fs,
        &[Path::new("/f0"), Path::new("/f1"), Path::new("/f2")],
        &[libc::O_CREAT | libc::O_RDWR; 3],
        &[0o644; 3],
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(1));
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
    let (_root, mut fs) = dummy();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/exists",
        VfOffset::At(0),
        b"existing".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeCleanup { index: 0 },
        vnfs::Error::transport(None, "injected cleanup failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = VectorBackend::vopen_raw_impl(
        &mut fs,
        &[Path::new("/created"), Path::new("/exists")],
        &[libc::O_CREAT | libc::O_EXCL | libc::O_RDWR; 2],
        &[0o644; 2],
    )
    .unwrap_err();
    assert_eq!(error.index(), Some(1));
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
    let (_root, mut fs) = dummy();
    fs.vwrite_impl(&borrow_writes(&[vfsi_sync::WriteOp::from_path(
        "/kept",
        VfOffset::At(0),
        b"data".to_vec(),
    )
    .with_creation()]))
        .unwrap();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeRemoveType { index: 0 },
        vnfs::Error::transport(None, "injected type lookup failure"),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs
        .remove_paths_impl(&[Path::new("/kept")], true)
        .unwrap_err();
    assert!(error.is_transport());
    assert!(script.is_consumed());
    assert!(fs.exists_impl(Path::new("/kept")).unwrap());
}

#[cfg(feature = "test-faults")]
#[test]
fn create_mode_failure_is_reported_instead_of_ignored() {
    let (_root, mut fs) = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeSetPermissions { index: 0 },
        vnfs::Error::failure(0, libc::EPERM as u32),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs
        .open_raw_impl(Path::new("/created"), libc::O_CREAT | libc::O_RDWR, 0o600)
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EPERM as u32);
    assert!(script.is_consumed());
    assert_eq!(fs.test_open_handle_count(), 0);
}

#[cfg(feature = "test-faults")]
#[test]
fn mkdir_mode_failure_is_reported_instead_of_ignored() {
    let (_root, mut fs) = dummy();
    let script = Arc::new(FaultScript::one(
        OpenFaultPoint::BeforeSetPermissions { index: 0 },
        vnfs::Error::failure(0, libc::EPERM as u32),
    ));
    fs.set_fault_injector(script.clone());
    let error = fs
        .mkdir_raw_impl(Path::new("/created-dir"), 0o700)
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EPERM as u32);
    assert!(script.is_consumed());
}

#[test]
fn strict_vectors_report_failure_index_without_rollback() {
    use vfsi_sync::VfFile;

    let (_root, mut fs) = dummy();
    fs.vwrite_impl(&borrow_writes(&[
        vfsi_sync::WriteOp::from_path("/first", VfOffset::At(0), Vec::new()).with_creation(),
        vfsi_sync::WriteOp::from_path("/third", VfOffset::At(0), Vec::new()).with_creation(),
    ]))
    .unwrap();
    let error = fs
        .vremove_impl(&[
            VfFile::from_path("/first"),
            VfFile::from_path("/missing"),
            VfFile::from_path("/third"),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert!(!fs.exists_impl(Path::new("/first")).unwrap());
    assert!(fs.exists_impl(Path::new("/third")).unwrap());
}

#[test]
fn native_scalar_contract_separates_metadata_query_from_update() {
    use vfsi_sync::api::AttrsOptions;
    use vfsi_sync::backend::HandleBackend;
    use vfsi_sync::{AttrMask, SetAttrsOp, Target};
    use vnfs::files::{OpenFlags, OpenOp};

    let (_root, mut fs) = dummy();
    let file = HandleBackend::open_impl(
        &mut fs,
        &OpenOp::new("/metadata", OpenFlags::WRITE | OpenFlags::CREATE),
    )
    .unwrap();
    HandleBackend::close_impl(&mut fs, &file).unwrap();

    let attrs = HandleBackend::metadata_impl(
        &mut fs,
        Target::Path(Path::new("/metadata")),
        AttrsOptions::new().fields(AttrMask::MODE | AttrMask::SIZE),
    )
    .unwrap();
    assert!(attrs.returned.contains(AttrMask::MODE | AttrMask::SIZE));

    let update = SetAttrsOp::new(Target::Path(Path::new("/metadata")))
        .permissions(vnfs::directory::Permissions::from_mode(0o640));
    HandleBackend::set_attributes_impl(&mut fs, &update).unwrap();
    assert_eq!(
        fs.stat_impl(Path::new("/metadata")).unwrap().mode & 0o777,
        0o640
    );
}

#[test]
fn allocating_directory_apis_enforce_entry_path_and_depth_limits() {
    use vfsi_sync::FsClient;
    use vfsi_sync::{AttrMask, WriteOp};
    use vnfs::directory::ListDirOptions;

    let (_root, mut fs) = dummy();
    fs.ensure_dir_impl(Path::new("/tree/sub"), 0o755).unwrap();
    fs.vwrite_impl(&borrow_writes(&[
        WriteOp::from_path("/tree/one", VfOffset::At(0), Vec::new()).with_creation(),
        WriteOp::from_path("/tree/two", VfOffset::At(0), Vec::new()).with_creation(),
    ]))
    .unwrap();

    let error = fs
        .walk_with_options_impl(
            Path::new("/tree"),
            AttrMask::stat(),
            ListDirOptions::new().recursive(true).max_entries(2),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let error = fs
        .walk_with_options_impl(
            Path::new("/tree"),
            AttrMask::stat(),
            ListDirOptions::new().recursive(true).max_depth(0),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let client = FsClient::new(fs);
    let error = client
        .read_dir_with_options("/tree", ListDirOptions::new().max_entries(1))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    let error = client
        .read_dir_with_options("/tree", ListDirOptions::new().max_path_bytes(1))
        .unwrap_err();
    assert_eq!(error.err_no(), libc::EFBIG as u32);

    assert_eq!(client.read_dir("/tree").unwrap().len(), 3);
}

#[test]
fn directory_visitor_callback_can_reenter_client_and_drop_a_file() {
    if common::supervise_with_deadline(
        "directory_visitor_callback_can_reenter_client_and_drop_a_file",
    ) {
        return;
    }
    use std::sync::mpsc;
    use std::time::Duration;
    use vfsi_sync::FsClient;

    let (_root, backend) = dummy();
    let client = FsClient::new(backend);
    client.create_dir("/tree").unwrap();
    // First page, one full continuation, and a final partial page.
    for index in 0..130 {
        client
            .write(format!("/tree/item-{index:04}"), b"x")
            .unwrap();
    }
    let held_file = client.open("/tree/item-0000").unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut held_file = Some(held_file);
        let mut seen = 0;
        let result = client.listdir("/tree", vfsi_core::api::ListDirOptions::new(), |entry| {
            // Both operations acquire the same backend mutex. In particular,
            // dropping an owned file must not block directory enumeration.
            assert_eq!(client.attrs(entry.entry.path())?.len(), Some(1));
            if let Some(file) = held_file.take() {
                drop(file);
            }
            seen += 1;
            Ok(vfsi_core::api::WalkControl::Continue)
        });
        sender.send((result, seen)).unwrap();
    });
    let (result, seen) = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("directory callback deadlocked on its own client");
    result.unwrap();
    assert_eq!(seen, 130);
    worker.join().unwrap();
}
