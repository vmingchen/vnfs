use super::prr_test_support::{FaultBackend, TestRoot, client};
use super::*;

#[test]
fn short_and_zero_copy_writes_are_errors_without_replay() {
    Python::initialize();
    for written in [0, 2, 5] {
        let root = TestRoot::new();
        std::fs::write(root.0.join("src"), b"abcdef").unwrap();
        std::fs::write(root.0.join("dst"), b"old-destination").unwrap();
        let mut backend = FaultBackend::new(&root.0, None);
        backend.write_counts = Some(vec![written]);
        let calls = backend.write_calls.clone();
        let client = client(backend);
        Python::attach(|py| {
            let (copied, errors) = client
                .copy_many(py, vec![("/src".into(), "/dst".into())])
                .unwrap();
            assert_eq!(copied, vec![None]);
            assert_eq!(error_codes(&errors), HashMap::from([(0, libc::EIO as u32)]));
            assert_eq!(calls.lock().unwrap().len(), 1);
            assert_eq!(
                std::fs::read(root.0.join("dst")).unwrap(),
                b"abcdef"[..written]
            );
        });
    }
}

#[test]
fn mixed_copy_completion_preserves_success_and_original_error_indices() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.write_counts = Some(vec![2, 6, 0]);
    let calls = backend.write_calls.clone();
    let client = client(backend);
    Python::attach(|py| {
        let (copied, errors) = client
            .copy_many(
                py,
                vec![
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/y".into()),
                    ("/c".into(), "/z".into()),
                ],
            )
            .unwrap();
        assert_eq!(copied, vec![None, Some(6), None]);
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(0, libc::EIO as u32), (2, libc::EIO as u32)])
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read(root.0.join("y")).unwrap(), b"abcdef");
    });
}

#[test]
fn full_and_empty_copy_writes_remain_successful() {
    Python::initialize();
    for payload in [b"abcdef".as_slice(), b"".as_slice()] {
        let root = TestRoot::new();
        std::fs::write(root.0.join("src"), payload).unwrap();
        std::fs::write(root.0.join("dst"), b"old").unwrap();
        let mut backend = FaultBackend::new(&root.0, None);
        backend.write_counts = Some(vec![payload.len()]);
        let calls = backend.write_calls.clone();
        let client = client(backend);
        Python::attach(|py| {
            let (copied, errors) = client
                .copy_many(py, vec![("/src".into(), "/dst".into())])
                .unwrap();
            assert_eq!(copied, vec![Some(payload.len() as u64)]);
            assert!(errors.is_empty());
            assert_eq!(calls.lock().unwrap().len(), 1);
            assert_eq!(std::fs::read(root.0.join("dst")).unwrap(), payload);
        });
    }
}

#[test]
fn short_copy_errors_keep_indices_after_read_failure_filtering() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "c"] {
        std::fs::write(root.0.join(name), b"abcdef").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.write_counts = Some(vec![2, 6]);
    let calls = backend.write_calls.clone();
    let client = client(backend);
    Python::attach(|py| {
        let (copied, errors) = client
            .copy_many(
                py,
                vec![
                    ("/a".into(), "/x".into()),
                    ("/missing".into(), "/y".into()),
                    ("/c".into(), "/z".into()),
                ],
            )
            .unwrap();
        assert_eq!(copied, vec![None, None, Some(6)]);
        assert_eq!(
            error_codes(&errors),
            HashMap::from([(0, libc::EIO as u32), (1, ERR_NOENT)])
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read(root.0.join("z")).unwrap(), b"abcdef");
    });
}

#[test]
fn copy_transport_failure_remains_connection_error_without_replay() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("src"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.write_error = Some(VfError::transport(Some(0), "ambiguous copy write"));
    let calls = backend.write_calls.clone();
    let client = client(backend);
    Python::attach(|py| {
        let error = client
            .copy_many(py, vec![("/src".into(), "/dst".into())])
            .unwrap_err();
        assert!(error.is_instance_of::<PyConnectionError>(py));
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read(root.0.join("src")).unwrap(), b"abcdef");
    });
}
