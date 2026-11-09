use super::prr_test_support::{FaultBackend, TestRoot, client};
use super::*;
use vfsi_sync::backend::HandleBackend;

#[test]
fn ranged_reads_keep_successes_around_multiple_missing_inputs_in_one_call() {
    Python::initialize();
    let root = TestRoot::new();
    for (name, data) in [("a", b"abcd"), ("b", b"efgh"), ("c", b"ijkl")] {
        std::fs::write(root.0.join(name), data).unwrap();
    }
    let backend = FaultBackend::new(&root.0, None);
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .read_many(
                py,
                vec![
                    "/a".into(),
                    "/missing-a".into(),
                    "/b".into(),
                    "/missing-b".into(),
                    "/c".into(),
                ],
                vec![0; 5],
                Some(vec![Some(4); 5]),
            )
            .unwrap();
        assert_eq!(
            data,
            [
                Some(b"abcd".to_vec()),
                None,
                Some(b"efgh".to_vec()),
                None,
                Some(b"ijkl".to_vec())
            ]
        );
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(1, ERR_NOENT), (3, ERR_NOENT)])
        );
    });
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn descriptor_reads_keep_valid_siblings_around_an_invalid_handle_in_one_call() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("file"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    let file = backend
        .inner
        .open_raw_impl(Path::new("/file"), libc::O_RDONLY, 0)
        .unwrap();
    let fd = i64::from(file.fd().unwrap());
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .pread_many(
                py,
                vec![fd, i64::from(i32::MAX), fd],
                vec![0, 0, 4],
                vec![2; 3],
            )
            .unwrap();
        assert_eq!(data, [Some(b"ab".to_vec()), None, Some(b"ef".to_vec())]);
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(1, libc::EBADF as u32)])
        );
    });
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn whole_file_reads_keep_successes_around_missing_inputs_in_one_batch() {
    let root = TestRoot::new();
    for (name, data) in [("a", b"aaa"), ("b", b"bbb"), ("c", b"ccc")] {
        std::fs::write(root.0.join(name), data).unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    let calls = backend.read_all_calls.clone();
    let paths: Vec<_> = ["/a", "/missing-1", "/b", "/missing-2", "/c"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let (data, errors) = read_allv_impl(&mut backend, &paths, 1024).unwrap();
    assert_eq!(
        data,
        [
            Some(b"aaa".to_vec()),
            None,
            Some(b"bbb".to_vec()),
            None,
            Some(b"ccc".to_vec())
        ]
    );
    assert_eq!(
        error_codes(&errors),
        HashMap::from([(1, ERR_NOENT), (3, ERR_NOENT)])
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], paths);
}
