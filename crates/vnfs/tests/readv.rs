#![cfg(all(feature = "auto", target_os = "linux"))]
use vnfs::{ErrorKind, FileHandle, Fs, FsExt, Mounted, ReadOp, ReadOptions, ResourceLimits};

fn mixed<C: Fs>(fs: &C) {
    fs.write_files(&[
        ("/a", b"abcdef".as_slice()),
        ("/b", b"xyz"),
        ("/empty", b""),
    ])
    .unwrap();
    let file = fs.open("/a").unwrap();
    let mut buffer = [0xcc; 4];
    let results = fs
        .readv_with_options(
            [
                ReadOp::whole("/b"),
                ReadOp::range(&file, 2, 2),
                ReadOp::into(&file, 5, &mut buffer),
                ReadOp::whole("/empty"),
            ],
            ReadOptions::new().max_total_bytes(9),
        )
        .unwrap();
    assert_eq!(results[0].data(), Some(b"xyz".as_slice()));
    assert_eq!(results[1].data(), Some(b"cd".as_slice()));
    assert_eq!(results[2].data(), None);
    assert_eq!(results[2].read(), 1);
    assert!(results[2].eof());
    assert_eq!(results[3].data(), Some(b"".as_slice()));
    assert_eq!(
        results.iter().map(|r| r.offset()).collect::<Vec<_>>(),
        [0, 2, 5, 0]
    );
    assert_eq!(buffer, [b'f', 0xcc, 0xcc, 0xcc]);
    buffer.fill(0); // No result retains the borrow, even while results remain live.
    assert_eq!(results[2].read(), 1);
    let mut file = file;
    let mut first = [0];
    std::io::Read::read_exact(&mut file, &mut first).unwrap();
    assert_eq!(&first, b"a");
    file.close().unwrap();
}
#[test]
fn mixed_destinations_on_mounted_and_auto() {
    let temp = tempfile::tempdir().unwrap();
    mixed(&Mounted::new(temp.path()).unwrap());
    mixed(&vnfs::Auto::new(temp.path()).unwrap());
}
#[test]
fn budgets_are_shared_and_preflight_buffer_lengths() {
    let temp = tempfile::tempdir().unwrap();
    let fs = Mounted::new(temp.path())
        .unwrap()
        .with_limits(ResourceLimits {
            max_read_bytes: 4,
            ..Default::default()
        });
    fs.write_files(&[("/a", b"abc"), ("/b", b"xyz")]).unwrap();
    assert_eq!(
        fs.readv([ReadOp::whole("/a"), ReadOp::whole("/b")])
            .unwrap_err()
            .kind(),
        ErrorKind::FileTooLarge
    );
    assert_eq!(
        fs.readv_with_options(
            [ReadOp::whole("/a"), ReadOp::whole("/b")],
            ReadOptions::new().max_total_bytes(6)
        )
        .unwrap()
        .len(),
        2
    );
    let file = fs.open("/a").unwrap();
    let mut buffer = [0xaa; 3];
    let error = fs
        .readv([
            ReadOp::into(&file, 0, &mut buffer),
            ReadOp::range(&file, 0, 2),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(buffer, [0xaa; 3]);
    let error = fs
        .readv([ReadOp::whole("/b"), ReadOp::into(&file, 0, &mut buffer)])
        .unwrap_err();
    assert_eq!(error.index(), Some(0));
    assert_eq!(&buffer, b"abc"); // Reads are not atomic; earlier buffers may be filled.
    assert!(
        fs.readv_with_options([], ReadOptions::new().max_total_bytes(0))
            .unwrap()
            .is_empty()
    );
}
#[test]
fn failure_indices_and_buffer_borrows_survive_consumption() {
    let temp = tempfile::tempdir().unwrap();
    let fs = Mounted::new(temp.path()).unwrap();
    fs.write("/a", b"abc").unwrap();
    let file = fs.open("/a").unwrap();
    let mut buffer = [0; 1];
    let error = fs
        .readv([
            ReadOp::whole("/a"),
            ReadOp::into(&file, 0, &mut buffer),
            ReadOp::whole("/missing"),
        ])
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert_eq!(error.index(), Some(2));
    buffer.fill(7);
    let other = Mounted::new(temp.path()).unwrap();
    let foreign = other.open("/a").unwrap();
    let error = fs
        .readv([
            ReadOp::whole("/missing"),
            ReadOp::into(&foreign, 0, &mut buffer),
        ])
        .unwrap_err();
    assert_eq!(error.index(), Some(1));
    assert_eq!(buffer, [7]);
}
#[test]
fn zero_length_buffers_and_owned_reads_keep_distinct_data_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let fs = Mounted::new(temp.path()).unwrap();
    fs.write("/a", b"abc").unwrap();
    let file = fs.open("/a").unwrap();
    let results = fs
        .readv([ReadOp::range(&file, 0, 0), ReadOp::into(&file, 0, &mut [])])
        .unwrap();
    assert_eq!(results[0].data(), Some([].as_slice()));
    assert_eq!(results[1].data(), None);
    assert_eq!(results.iter().map(|r| r.read()).collect::<Vec<_>>(), [0, 0]);
}

#[test]
fn path_conversions_and_iterators_construct_whole_file_operations() {
    let temp = tempfile::tempdir().unwrap();
    let fs = Mounted::new(temp.path()).unwrap();
    fs.write_files(&[("/a", b"abc"), ("/b", b"xyz")]).unwrap();
    let results = fs
        .readv(["/a".into(), std::path::Path::new("/b").into()])
        .unwrap();
    assert_eq!(results.iter().map(|r| r.read()).collect::<Vec<_>>(), [3, 3]);
    let results = fs
        .readv(["/a", "/b"].into_iter().map(ReadOp::whole))
        .unwrap();
    assert_eq!(results[0].data(), Some(b"abc".as_slice()));
    assert_eq!(results[1].data(), Some(b"xyz".as_slice()));
}
#[test]
fn external_clients_can_inspect_sources_without_private_fields() {
    let temp = tempfile::tempdir().unwrap();
    let fs = Mounted::new(temp.path()).unwrap();
    fs.write("/a", b"abc").unwrap();
    let file = fs.open("/a").unwrap();
    let whole: ReadOp<'_, vnfs::MountedFile> = ReadOp::whole("/a");
    assert_eq!(whole.whole_file_path(), Some(std::path::Path::new("/a")));
    assert!(whole.range_ref().is_none());
    let range = ReadOp::range(&file, 0, 1);
    assert!(range.range_ref().is_some());
    assert!(range.whole_file_path().is_none());
    let mut bytes = [0; 1];
    let mut into = ReadOp::into(&file, 0, &mut bytes);
    assert!(into.buffer_request_mut().is_some());
    assert!(into.range_ref().is_none());
}

#[test]
fn result_constructors_preserve_storage_invariants_without_retaining_borrows() {
    let owned = vnfs::ReadResult::owned(17, vec![1, 2, 3], true);
    assert_eq!(owned.offset(), 17);
    assert_eq!(owned.read(), 3);
    assert!(owned.eof());
    assert!(!owned.is_buffered());
    assert_eq!(owned.data(), Some([1, 2, 3].as_slice()));
    assert_eq!(owned.into_data(), Some(vec![1, 2, 3]));
    let borrowed = vnfs::ReadResult::buffered(5, 2, false);
    assert!(borrowed.is_buffered());
    assert_eq!(borrowed.read(), 2);
    assert!(!borrowed.eof());
    assert_eq!(borrowed.data(), None);
    assert_eq!(borrowed.into_data(), None);
    assert_eq!(vnfs::ReadResult::owned(0, Vec::new(), true).read(), 0);
}
