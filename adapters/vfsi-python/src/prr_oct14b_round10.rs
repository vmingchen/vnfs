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
        assert_eq!(copied, vec![Some(6), None, Some(6)]);
        assert_eq!(error_codes(&errors), HashMap::from([(1, ERR_NOENT)]));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![vec![PathBuf::from("/x"), "/missing/y".into(), "/z".into()]]
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
fn first_copy_error_keeps_successful_independent_suffix() {
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
fn acknowledged_copy_does_not_require_destination_verification_reads() {
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
    let reads = backend.read_all_calls.clone();
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
        assert_eq!(copied, vec![Some(6), None, Some(6)]);
        assert_eq!(error_codes(&errors), HashMap::from([(1, ERR_NOENT)]));
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(
            reads.lock().unwrap().iter().flatten().all(|path| ![
                PathBuf::from("/x"),
                "/missing/y".into(),
                "/z".into()
            ]
            .contains(path))
        );
    });
}

#[test]
fn copy_item_transport_maps_original_copy_index_without_verification() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend
        .read_all_errors_after_write
        .push_back(VfError::transport(Some(0), "verification disconnected"));
    backend.write_item_errors.push((
        0,
        vfsi_core::OpError::unknown(VfError::transport(None, "WRITE disconnected"), None),
    ));
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
fn copy_item_transport_keeps_original_position_without_semantic_replay() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_all_errors_after_write.extend([
        VfError::errno(0, libc::EACCES as u32),
        VfError::transport(Some(1), "verification disconnected beside semantic failure"),
    ]);
    backend.write_item_errors.extend([
        (
            0,
            vfsi_core::OpError::failed(VfError::errno(None, ERR_ACCES), None),
        ),
        (
            1,
            vfsi_core::OpError::unknown(
                VfError::transport(None, "WRITE disconnected beside semantic failure"),
                None,
            ),
        ),
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
