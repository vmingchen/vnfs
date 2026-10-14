use super::prr_test_support::{FaultBackend, TestRoot, client};
use super::*;

#[test]
fn positive_short_ranges_complete_at_frozen_offsets() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_limit = Some(2);
    let requests = backend.read_requests.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .read_many(py, vec!["/a".into()], vec![1], Some(vec![Some(5)]))
            .unwrap();
        assert!(errors.is_empty());
        assert_eq!(data, vec![Some(b"bcde".to_vec())]);
        assert_eq!(*requests.lock().unwrap(), vec![vec![(1, 4)], vec![(3, 2)]]);
    });
}

#[test]
fn completed_and_empty_siblings_are_not_replayed() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_limit = Some(2);
    let requests = backend.read_requests.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .read_many(
                py,
                vec!["/a".into(); 3],
                vec![0, 1, 2],
                Some(vec![Some(2), Some(5), Some(2)]),
            )
            .unwrap();
        assert!(errors.is_empty());
        assert_eq!(
            data,
            vec![Some(b"ab".to_vec()), Some(b"bcde".to_vec()), Some(vec![])]
        );
        assert_eq!(
            *requests.lock().unwrap(),
            vec![vec![(0, 2), (1, 4), (2, 0)], vec![(3, 2)]]
        );
    });
}

#[test]
fn explicit_eof_completes_a_short_range() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abc").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_limit = Some(2);
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .read_many(py, vec!["/a".into()], vec![0], Some(vec![Some(10)]))
            .unwrap();
        assert!(errors.is_empty());
        assert_eq!(data, vec![Some(b"abc".to_vec())]);
        assert_eq!(calls.lock().unwrap().len(), 2);
    });
}

#[test]
fn zero_progress_without_eof_is_an_indexed_error() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abc").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_limit = Some(0);
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .read_many(py, vec!["/a".into()], vec![0], Some(vec![Some(3)]))
            .unwrap();
        assert_eq!(data, vec![None]);
        assert_eq!(errors, HashMap::from([(0, libc::EIO as u32)]));
        assert_eq!(calls.lock().unwrap().len(), 1);
    });
}

#[test]
fn continuation_semantic_errors_use_original_indices_and_discard_partial_data() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(&root.0, Some(VfError::failure(0, ERR_NOENT)));
    backend.read_error_on_call = 1;
    backend.read_limit = Some(2);
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .read_many(
                py,
                vec!["/a".into(); 2],
                vec![0; 2],
                Some(vec![Some(2), Some(6)]),
            )
            .unwrap();
        assert_eq!(data, vec![Some(b"ab".to_vec()), None]);
        assert_eq!(errors, HashMap::from([(1, ERR_NOENT)]));
        assert_eq!(calls.lock().unwrap().len(), 2);
    });
}

#[test]
fn continuation_transport_errors_abort_and_name_original_index() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(
        &root.0,
        Some(VfError::transport(Some(0), "continuation lost")),
    );
    backend.read_error_on_call = 1;
    backend.read_limit = Some(2);
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .read_many(
                py,
                vec!["/a".into(); 2],
                vec![0; 2],
                Some(vec![Some(2), Some(6)]),
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
        assert_eq!(calls.lock().unwrap().len(), 2);
    });
}

#[test]
fn size_resolved_before_growth_remains_the_range_end() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abcdef").unwrap();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_limit = Some(2);
    backend.grow_before_read = Some((root.0.join("a"), b"new-tail".to_vec()));
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let (data, errors) = client(backend)
            .read_many(py, vec!["/a".into()], vec![0], None)
            .unwrap();
        assert_eq!(data, vec![Some(b"abcdef".to_vec())]);
        assert!(errors.is_empty());
        assert_eq!(calls.lock().unwrap().len(), 3);
    });
}
