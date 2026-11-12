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

#[test]
fn positioned_outcomes_preserve_transport_failures_and_successful_siblings() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("file"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    let file = backend
        .inner
        .open_raw_impl(Path::new("/file"), libc::O_RDONLY, 0)
        .unwrap();
    let fd = file.fd().unwrap() as i64;
    backend.read_item_errors = vec![(1, VfError::transport(None, "lost middle read"))];
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let results = client(backend)
            .preadv(py, vec![fd; 3], vec![0, 2, 4], vec![2; 3])
            .unwrap();
        assert_eq!(results[0].bind(py).extract::<Vec<u8>>().unwrap(), b"ab");
        assert_eq!(results[2].bind(py).extract::<Vec<u8>>().unwrap(), b"ef");
        assert!(results[1].bind(py).is_instance_of::<PyConnectionError>());
        assert_eq!(
            results[1]
                .bind(py)
                .getattr("failure_state")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "unknown"
        );
    });
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn positioned_write_outcomes_keep_progress_and_later_success_without_replay() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("file"), b"------").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    let file = backend
        .inner
        .open_raw_impl(Path::new("/file"), libc::O_WRONLY, 0)
        .unwrap();
    let fd = file.fd().unwrap() as i64;
    backend.write_item_errors = vec![(
        1,
        vfsi_core::OpError::unknown(
            VfError::transport(None, "lost write"),
            Some(vfsi_core::IoProgress::Write { bytes_written: 1 }),
        ),
    )];
    let calls = backend.write_calls.clone();
    Python::attach(|py| {
        let results = client(backend)
            .pwritev(
                py,
                vec![fd; 3],
                vec![0, 2, 4],
                vec![b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()],
            )
            .unwrap();
        assert_eq!(results[0].bind(py).extract::<usize>().unwrap(), 2);
        assert_eq!(results[2].bind(py).extract::<usize>().unwrap(), 2);
        let error = results[1].bind(py);
        assert_eq!(
            error
                .getattr("failure_state")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "unknown"
        );
        assert_eq!(
            error
                .getattr("bytes_written")
                .unwrap()
                .extract::<usize>()
                .unwrap(),
            1
        );
    });
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read(root.0.join("file")).unwrap(), b"ab--ef");
}

#[test]
fn operation_exceptions_preserve_each_failure_state_and_owned_read_prefix() {
    Python::initialize();
    Python::attach(|py| {
        for (state, error) in [
            (
                "not_attempted",
                vfsi_core::OpError::not_attempted(VfError::errno(None, libc::EBADF as u32)),
            ),
            (
                "failed",
                vfsi_core::OpError::failed(VfError::errno(None, libc::EIO as u32), None),
            ),
            (
                "unknown",
                vfsi_core::OpError::unknown(
                    VfError::transport(None, "lost"),
                    Some(vfsi_core::IoProgress::Read(
                        vfsi_core::api::ReadResult::owned(9, b"prefix".to_vec(), false),
                    )),
                ),
            ),
        ] {
            let result = operation_exception(py, 2, error).unwrap();
            assert_eq!(
                result
                    .bind(py)
                    .getattr("failure_state")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                state
            );
            assert_eq!(
                result
                    .bind(py)
                    .getattr("index")
                    .unwrap()
                    .extract::<usize>()
                    .unwrap(),
                2
            );
            if state == "unknown" {
                assert_eq!(
                    result
                        .bind(py)
                        .getattr("read_prefix")
                        .unwrap()
                        .extract::<Vec<u8>>()
                        .unwrap(),
                    b"prefix"
                );
                assert_eq!(
                    result
                        .bind(py)
                        .getattr("read_offset")
                        .unwrap()
                        .extract::<u64>()
                        .unwrap(),
                    9
                );
            } else {
                assert!(!result.bind(py).hasattr("read_prefix").unwrap());
            }
        }
    });
}

#[test]
fn unsupported_outcomes_preserve_the_existing_python_exception_class() {
    Python::initialize();
    Python::attach(|py| {
        let error = operation_exception(
            py,
            0,
            vfsi_core::OpError::not_attempted(VfError::unsupported(None)),
        )
        .unwrap();
        assert!(error.bind(py).is_instance_of::<PyNotImplementedError>());
        assert_eq!(
            error
                .bind(py)
                .getattr("failure_state")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "not_attempted"
        );
    });
}

#[test]
fn independent_whole_file_reads_keep_values_around_missing_paths() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"aaa").unwrap();
    std::fs::write(root.0.join("b"), b"bbb").unwrap();
    let backend = FaultBackend::new(&root.0, None);
    let calls = backend.read_all_calls.clone();
    Python::attach(|py| {
        let results = client(backend)
            .read_allv(py, vec!["/a".into(), "/missing".into(), "/b".into()], None)
            .unwrap();
        assert_eq!(results[0].bind(py).extract::<Vec<u8>>().unwrap(), b"aaa");
        assert_eq!(results[2].bind(py).extract::<Vec<u8>>().unwrap(), b"bbb");
        assert!(results[1].bind(py).is_instance_of::<PyFileNotFoundError>());
        assert_eq!(
            results[1]
                .bind(py)
                .getattr("failure_state")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "failed"
        );
    });
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn whole_file_validation_charges_only_valid_returned_prefixes() {
    use vfsi_core::{FailureState, IoProgress, OpError, TransportKind};
    let root = TestRoot::new();
    std::fs::write(root.0.join("first"), b"").unwrap();
    std::fs::write(root.0.join("healthy"), b"ab").unwrap();
    let paths = vec!["/first".into(), "/healthy".into()];
    for prefix in [
        vfsi_core::api::ReadResult::owned(1, b"xx".to_vec(), false),
        vfsi_core::api::ReadResult::owned(0, b"xxx".to_vec(), false),
        vfsi_core::api::ReadResult::buffered(0, 2, false),
    ] {
        let mut backend = FaultBackend::new(&root.0, None);
        backend.read_all_item_errors = vec![(
            0,
            OpError::failed(
                VfError::errno(None, libc::EIO as u32),
                Some(IoProgress::Read(prefix)),
            ),
        )];
        let mut outcomes = read_all_outcomes_impl(&mut backend, &paths, 2)
            .unwrap()
            .into_iter();
        let error = outcomes.next().unwrap().unwrap_err();
        assert_eq!(error.state(), FailureState::Unknown);
        assert_eq!(
            error.cause().transport_kind(),
            Some(TransportKind::InvalidReply)
        );
        assert!(error.progress().is_none());
        assert_eq!(outcomes.next().unwrap().unwrap(), b"ab");
    }
    // A valid exported failure prefix still consumes the shared budget.
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_all_item_errors = vec![(
        0,
        OpError::failed(
            VfError::errno(None, libc::EIO as u32),
            Some(IoProgress::Read(vfsi_core::api::ReadResult::owned(
                0,
                b"xx".to_vec(),
                false,
            ))),
        ),
    )];
    let outcomes = read_all_outcomes_impl(&mut backend, &paths, 2).unwrap();
    assert_eq!(
        outcomes[0].as_ref().unwrap_err().cause().raw_code(),
        libc::EIO as u32
    );
    assert!(outcomes[0].as_ref().unwrap_err().progress().is_some());
    assert_eq!(
        outcomes[1].as_ref().unwrap_err().cause().transport_kind(),
        Some(TransportKind::InvalidReply)
    );
}
