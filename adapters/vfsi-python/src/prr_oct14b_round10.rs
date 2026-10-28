use super::prr_test_support::{FaultBackend, TestRoot, client};
use super::*;

#[test]
fn completed_copy_prefix_is_not_replayed_after_later_error() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.replace_after_write_error = Some((root.0.join("x"), b"intervening writer".to_vec()));
    let calls = backend.write_calls.clone();
    Python::attach(|py| {
        let (copied, errors) = client(backend)
            .copy_many(
                py,
                vec![
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/missing/y".into()),
                    ("/c".into(), "/z".into()),
                ],
            )
            .unwrap();
        assert_eq!(
            std::fs::read(root.0.join("x")).unwrap(),
            b"intervening writer"
        );
        assert_eq!(copied, vec![None, None, Some(6)]);
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(0, libc::EIO as u32), (1, ERR_NOENT)])
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                vec![PathBuf::from("/x"), "/missing/y".into(), "/z".into()],
                vec![PathBuf::from("/z")]
            ]
        );
    });
}

#[test]
fn unreported_short_prefix_is_not_assumed_complete_or_replayed() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.write_counts = Some(vec![2, 6, 6]);
    Python::attach(|py| {
        let (copied, errors) = client(backend)
            .copy_many(
                py,
                vec![
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/missing/y".into()),
                    ("/c".into(), "/z".into()),
                ],
            )
            .unwrap();
        assert_eq!(std::fs::read(root.0.join("x")).unwrap(), b"ab");
        assert_eq!(copied, vec![None, None, Some(6)]);
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(0, libc::EIO as u32), (1, ERR_NOENT)])
        );
    });
}

#[test]
fn copy_prefix_errors_remap_after_source_read_filtering() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    Python::attach(|py| {
        let (copied, errors) = client(FaultBackend::new(&root.0, None))
            .copy_many(
                py,
                vec![
                    ("/absent".into(), "/unused".into()),
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/missing/y".into()),
                    ("/c".into(), "/z".into()),
                ],
            )
            .unwrap();
        assert_eq!(copied, vec![None, Some(6), None, Some(6)]);
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(0, ERR_NOENT), (2, ERR_NOENT)])
        );
        assert_eq!(std::fs::read(root.0.join("x")).unwrap(), b"abcdef");
    });
}

#[test]
fn first_copy_error_retries_only_unattempted_suffix() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    Python::attach(|py| {
        let (copied, errors) = client(FaultBackend::new(&root.0, None))
            .copy_many(
                py,
                vec![
                    ("/a".into(), "/missing/y".into()),
                    ("/b".into(), "/z".into()),
                ],
            )
            .unwrap();
        assert_eq!(copied, vec![None, Some(6)]);
        assert_eq!(error_codes(&errors), HashMap::from([(0, ERR_NOENT)]));
    });
}

#[test]
fn prefix_verification_semantic_error_is_not_success_or_mutation_retry() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend
        .read_all_errors_after_write
        .push_back(VfError::errno(0, libc::EACCES as u32));
    let calls = backend.write_calls.clone();
    Python::attach(|py| {
        let (copied, errors) = client(backend)
            .copy_many(
                py,
                vec![
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/missing/y".into()),
                    ("/c".into(), "/z".into()),
                ],
            )
            .unwrap();
        assert_eq!(copied, vec![None, None, Some(6)]);
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(0, libc::EIO as u32), (1, ERR_NOENT)])
        );
        assert_eq!(calls.lock().unwrap()[1], vec![PathBuf::from("/z")]);
    });
}

#[test]
fn prefix_verification_transport_error_maps_original_copy_index() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend
        .read_all_errors_after_write
        .push_back(VfError::transport(Some(0), "verification disconnected"));
    let calls = backend.write_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .copy_many(
                py,
                vec![
                    ("/absent".into(), "/unused".into()),
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/missing/y".into()),
                ],
            )
            .unwrap_err();
        assert!(error.is_instance_of::<PyConnectionError>(py));
        assert_eq!(
            error
                .value(py)
                .getattr("index")
                .unwrap()
                .extract::<usize>()
                .unwrap(),
            1
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    });
}

#[test]
fn prefix_verification_transport_index_survives_semantic_filtering() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_all_errors_after_write.extend([
        VfError::errno(0, libc::EACCES as u32),
        VfError::transport(Some(0), "verification disconnected after semantic failure"),
    ]);
    let calls = backend.write_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .copy_many(
                py,
                vec![
                    ("/absent".into(), "/unused".into()),
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/y".into()),
                    ("/c".into(), "/missing/z".into()),
                ],
            )
            .unwrap_err();
        assert!(error.is_instance_of::<PyConnectionError>(py));
        assert_eq!(
            error
                .value(py)
                .getattr("index")
                .unwrap()
                .extract::<usize>()
                .unwrap(),
            2
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    });
}
