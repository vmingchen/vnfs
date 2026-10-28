use super::prr_test_support::{FaultBackend, TestRoot, client};
use super::*;

#[test]
fn small_copy_identity_failures_prevent_mutation_and_keep_original_indices() {
    Python::initialize();
    for (position, code) in [
        (0, ERR_ACCES),
        (1, ERR_ACCES),
        (0, ERR_NOENT),
        (1, libc::EIO as u32),
    ] {
        let root = TestRoot::new();
        let source = root.0.join("source");
        std::fs::write(&source, b"protected").unwrap();
        std::fs::hard_link(&source, root.0.join("alias")).unwrap();
        std::fs::write(root.0.join("other"), b"healthy").unwrap();
        let mut backend = FaultBackend::new(&root.0, None);
        backend
            .attrs_errors
            .push_back(Some(VfError::errno(position, code)));
        let writes = backend.write_calls.clone();
        Python::attach(|py| {
            let (copied, errors) = client(backend)
                .copy_many(
                    py,
                    vec![
                        ("/missing".into(), "/unused".into()),
                        ("/source".into(), "/alias".into()),
                        ("/other".into(), "/new".into()),
                    ],
                )
                .unwrap();
            assert_eq!(copied, vec![None, None, Some(7)]);
            assert_eq!(
                error_codes(&errors),
                HashMap::from([(0, ERR_NOENT), (1, code)])
            );
        });
        assert_eq!(*writes.lock().unwrap(), vec![vec![PathBuf::from("/new")]]);
        assert_eq!(std::fs::read(&source).unwrap(), b"protected");
        assert_eq!(std::fs::read(root.0.join("new")).unwrap(), b"healthy");
    }
}

#[test]
fn small_copy_allows_missing_destinations_but_still_rejects_aliases() {
    Python::initialize();
    let root = TestRoot::new();
    let source = root.0.join("source");
    std::fs::write(&source, b"protected").unwrap();
    std::fs::hard_link(&source, root.0.join("alias")).unwrap();
    let backend = FaultBackend::new(&root.0, None);
    let writes = backend.write_calls.clone();
    Python::attach(|py| {
        let (copied, errors) = client(backend)
            .copy_many(
                py,
                vec![
                    ("/source".into(), "/alias".into()),
                    ("/source".into(), "/new".into()),
                ],
            )
            .unwrap();
        assert_eq!(copied, vec![None, Some(9)]);
        assert_eq!(error_codes(&errors), HashMap::from([(0, ERR_SAME_FILE)]));
    });
    assert_eq!(*writes.lock().unwrap(), vec![vec![PathBuf::from("/new")]]);
    assert_eq!(std::fs::read(root.0.join("new")).unwrap(), b"protected");
}

#[test]
fn single_small_copy_allows_missing_destination_identity_attributes() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("source"), b"contents").unwrap();
    let backend = FaultBackend::new(&root.0, None);
    let attrs = backend.attrs_calls.clone();
    let writes = backend.write_calls.clone();
    Python::attach(|py| {
        let (copied, errors) = client(backend)
            .copy_many(py, vec![("/source".into(), "/new".into())])
            .unwrap();
        assert_eq!(copied, vec![Some(8)]);
        assert!(errors.is_empty());
    });
    assert_eq!(
        *attrs.lock().unwrap(),
        vec![
            vec![PathBuf::from("/source"), "/new".into()],
            vec![PathBuf::from("/source")],
        ]
    );
    assert_eq!(*writes.lock().unwrap(), vec![vec![PathBuf::from("/new")]]);
    assert_eq!(std::fs::read(root.0.join("new")).unwrap(), b"contents");
}
