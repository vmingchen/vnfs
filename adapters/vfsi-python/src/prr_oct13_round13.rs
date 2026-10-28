use super::prr_test_support::{FaultBackend, TestRoot, client};
use super::*;

#[test]
fn ranged_read_transport_never_becomes_semantic_recovery() {
    Python::initialize();
    for index in [None, Some(0), Some(1), Some(2), Some(99)] {
        let root = TestRoot::new();
        for name in ["a", "b", "c"] {
            std::fs::write(root.0.join(name), b"ok").unwrap();
        }
        let backend = FaultBackend::new(
            &root.0,
            Some(VfError::transport(index, "injected read transport")),
        );
        let calls = backend.read_calls.clone();
        let client = client(backend);
        Python::attach(|py| {
            let error = client
                .read_many(
                    py,
                    vec!["/a".into(), "/b".into(), "/c".into()],
                    vec![0; 3],
                    Some(vec![Some(2); 3]),
                )
                .unwrap_err();
            assert!(error.is_instance_of::<PyConnectionError>(py));
            if let Some(index) = index {
                assert_eq!(
                    error
                        .value(py)
                        .getattr("index")
                        .unwrap()
                        .extract::<usize>()
                        .unwrap(),
                    index
                );
            }
            assert_eq!(calls.lock().unwrap().len(), 1);
        });
    }
}

#[test]
fn ranged_read_semantic_failure_still_recovers_unaffected_paths() {
    Python::initialize();
    for index in 0..3 {
        let root = TestRoot::new();
        for name in ["a", "b", "c"] {
            std::fs::write(root.0.join(name), name.as_bytes()).unwrap();
        }
        let backend = FaultBackend::new(&root.0, Some(VfError::errno(index, ERR_NOENT)));
        let calls = backend.read_calls.clone();
        let client = client(backend);
        Python::attach(|py| {
            let (data, errors) = client
                .read_many(
                    py,
                    vec!["/a".into(), "/b".into(), "/c".into()],
                    vec![0; 3],
                    Some(vec![Some(1); 3]),
                )
                .unwrap();
            assert_eq!(error_codes(&errors), HashMap::from([(index, ERR_NOENT)]));
            for (position, name) in ["a", "b", "c"].iter().enumerate() {
                assert_eq!(
                    data[position],
                    if position == index {
                        None
                    } else {
                        Some(name.as_bytes().to_vec())
                    }
                );
            }
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1].len(), 2);
            assert!(!calls[1].contains(&PathBuf::from(format!("/{}", ["a", "b", "c"][index]))));
        });
    }
}
