//! PRR: every filtered vector reports transport failures in original request order.
use super::prr_test_support::{FaultBackend, TestRoot, client};
use super::*;

fn assert_transport_index(py: Python<'_>, error: PyErr, expected: Option<usize>) {
    assert!(error.is_instance_of::<PyConnectionError>(py));
    let actual = error
        .value(py)
        .getattr("index")
        .ok()
        .map(|index| index.extract::<usize>().unwrap());
    assert_eq!(actual, expected);
}

#[test]
fn copy_write_transport_maps_filtered_source_indices_without_replay() {
    Python::initialize();
    for (subset_index, expected) in [
        (Some(0), Some(1)),
        (Some(1), Some(2)),
        (None, None),
        (Some(99), Some(99)),
    ] {
        let root = TestRoot::new();
        for name in ["a", "b"] {
            std::fs::write(root.0.join(name), b"ok").unwrap();
        }
        let mut backend = FaultBackend::new(&root.0, None);
        backend.write_error = Some(VfError::transport(subset_index, "write disconnected"));
        let calls = backend.write_calls.clone();
        Python::attach(|py| {
            let error = client(backend)
                .copy_many(
                    py,
                    vec![
                        ("/missing".into(), "/unused".into()),
                        ("/a".into(), "/x".into()),
                        ("/b".into(), "/y".into()),
                    ],
                )
                .unwrap_err();
            assert_transport_index(py, error, expected);
        });
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(!root.0.join("x").exists());
        assert!(!root.0.join("y").exists());
    }
}

#[test]
fn copy_write_transport_maps_again_after_an_earlier_semantic_failure() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b", "c"] {
        std::fs::write(root.0.join(name), b"ok").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.write_errors.extend([
        VfError::errno(0, ERR_ACCES),
        VfError::transport(Some(0), "later write disconnected"),
    ]);
    let calls = backend.write_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .copy_many(
                py,
                vec![
                    ("/missing".into(), "/unused".into()),
                    ("/a".into(), "/x".into()),
                    ("/b".into(), "/y".into()),
                    ("/c".into(), "/z".into()),
                ],
            )
            .unwrap_err();
        assert_transport_index(py, error, Some(2));
    });
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            vec![PathBuf::from("/x"), "/y".into(), "/z".into()],
            vec![PathBuf::from("/y"), "/z".into()],
        ]
    );
}

#[test]
fn descriptor_read_transport_maps_after_semantic_filtering() {
    Python::initialize();
    let root = TestRoot::new();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.read_errors.extend([
        VfError::errno(0, ERR_NOENT),
        VfError::transport(Some(1), "read disconnected"),
    ]);
    let calls = backend.read_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .pread_many(py, vec![11, 22, 33], vec![0; 3], vec![2; 3])
            .unwrap_err();
        assert_transport_index(py, error, Some(2));
    });
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[test]
fn attribute_transport_maps_after_semantic_filtering_for_both_follow_modes() {
    let root = TestRoot::new();
    for follow in [false, true] {
        let mut backend = FaultBackend::new(&root.0, None);
        backend.attrs_errors.extend([
            Some(VfError::errno(0, ERR_NOENT)),
            Some(VfError::transport(Some(1), "attrs disconnected")),
        ]);
        let error = attrs_many_impl(
            &mut backend,
            &["/missing".into(), "/a".into(), "/b".into()],
            AttrMask::SIZE,
            follow,
        )
        .unwrap_err();
        assert!(error.is_transport());
        assert_eq!(error.index(), Some(2));
        assert_eq!(backend.attrs_calls.lock().unwrap().len(), 2);
    }
}

#[test]
fn range_size_lookup_transport_uses_original_path_index() {
    Python::initialize();
    let root = TestRoot::new();
    let mut backend = FaultBackend::new(&root.0, None);
    backend.attrs_errors.extend([
        Some(VfError::errno(0, ERR_NOENT)),
        Some(VfError::transport(Some(0), "range attrs disconnected")),
    ]);
    let reads = backend.read_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .read_many(py, vec!["/missing".into(), "/a".into()], vec![0; 2], None)
            .unwrap_err();
        assert_transport_index(py, error, Some(1));
    });
    assert!(reads.lock().unwrap().is_empty());
}

#[test]
fn copy_identity_transport_maps_flattened_and_filtered_attribute_indices() {
    Python::initialize();
    let root = TestRoot::new();
    for name in ["a", "b"] {
        std::fs::write(root.0.join(name), b"ok").unwrap();
    }
    let mut backend = FaultBackend::new(&root.0, None);
    backend.attrs_errors.extend([
        Some(VfError::errno(0, ERR_NOENT)),
        Some(VfError::transport(Some(1), "copy identity disconnected")),
    ]);
    let writes = backend.write_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .copy_many(
                py,
                vec![
                    ("/missing".into(), "/unused".into()),
                    ("/a".into(), "/x".into()),
                    ("/absent".into(), "/unused2".into()),
                    ("/b".into(), "/y".into()),
                ],
            )
            .unwrap_err();
        assert_transport_index(py, error, Some(3));
    });
    assert!(writes.lock().unwrap().is_empty());
}

fn oversized_backend(root: &TestRoot) -> FaultBackend {
    std::fs::write(root.0.join("large"), vec![42; 2048]).unwrap();
    FaultBackend::new(&root.0, None)
}
fn oversized_pairs() -> Vec<(PathBuf, PathBuf)> {
    vec![
        ("/missing".into(), "/unused".into()),
        ("/large".into(), "/destination".into()),
    ]
}

#[test]
fn oversized_source_attribute_transport_maps_back_to_copy_index() {
    Python::initialize();
    let root = TestRoot::new();
    let mut backend = oversized_backend(&root);
    backend.attrs_errors.push_back(Some(VfError::transport(
        Some(0),
        "source attrs disconnected",
    )));
    Python::attach(|py| {
        let error = client(backend)
            .copy_many(py, oversized_pairs())
            .unwrap_err();
        assert_transport_index(py, error, Some(1));
    });
    assert!(!root.0.join("destination").exists());
}

#[test]
fn oversized_destination_attribute_transport_maps_back_to_copy_index() {
    Python::initialize();
    let root = TestRoot::new();
    let mut backend = oversized_backend(&root);
    backend.attrs_errors.extend([
        None,
        Some(VfError::transport(
            Some(0),
            "destination attrs disconnected",
        )),
    ]);
    Python::attach(|py| {
        let error = client(backend)
            .copy_many(py, oversized_pairs())
            .unwrap_err();
        assert_transport_index(py, error, Some(1));
    });
    assert!(!root.0.join("destination").exists());
}

#[test]
fn oversized_extent_transport_maps_back_to_copy_index_without_replay() {
    Python::initialize();
    let root = TestRoot::new();
    let mut backend = oversized_backend(&root);
    backend
        .extent_errors
        .push_back(VfError::transport(Some(0), "extent disconnected"));
    let calls = backend.extent_calls.clone();
    Python::attach(|py| {
        let error = client(backend)
            .copy_many(py, oversized_pairs())
            .unwrap_err();
        assert_transport_index(py, error, Some(1));
    });
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(!root.0.join("destination").exists());
}

#[test]
fn oversized_extent_transport_maps_again_after_semantic_filtering() {
    Python::initialize();
    let root = TestRoot::new();
    let mut backend = oversized_backend(&root);
    std::fs::write(root.0.join("other"), vec![43; 2048]).unwrap();
    backend.extent_errors.extend([
        VfError::errno(0, ERR_ACCES),
        VfError::transport(Some(0), "later extent disconnected"),
    ]);
    let calls = backend.extent_calls.clone();
    let error = Python::attach(|py| {
        client(backend)
            .copy_many(
                py,
                vec![
                    ("/missing".into(), "/unused".into()),
                    ("/large".into(), "/destination".into()),
                    ("/absent".into(), "/unused2".into()),
                    ("/other".into(), "/destination2".into()),
                ],
            )
            .unwrap_err()
    });
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].len(), 1);
    let expected = if calls[1][0] == Path::new("/large") {
        1
    } else {
        3
    };
    Python::attach(|py| assert_transport_index(py, error, Some(expected)));
}

#[test]
fn ordinary_descriptor_metadata_delegates_without_faults() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("a"), b"abcdefgh").unwrap();
    std::fs::write(root.0.join("b"), b"xyz").unwrap();
    use vfsi_sync::backend::HandleBackend;
    let mut backend = FaultBackend::new(&root.0, None);
    let calls = backend.attrs_calls.clone();
    let a = backend
        .inner
        .open_raw_impl(Path::new("/a"), libc::O_RDONLY, 0o644)
        .unwrap()
        .fd()
        .unwrap() as i64;
    let b = backend
        .inner
        .open_raw_impl(Path::new("/b"), libc::O_RDONLY, 0o644)
        .unwrap()
        .fd()
        .unwrap() as i64;
    Python::attach(|py| {
        let client = client(backend);
        let attrs = client.fstat(py, a).unwrap();
        assert_eq!(
            attrs
                .bind(py)
                .get_item("size")
                .unwrap()
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            8
        );
        let attrs = client.fstat_many(py, vec![b, a]).unwrap();
        let sizes: Vec<u64> = attrs
            .iter()
            .map(|attrs| {
                attrs
                    .bind(py)
                    .get_item("size")
                    .unwrap()
                    .unwrap()
                    .extract()
                    .unwrap()
            })
            .collect();
        assert_eq!(sizes, vec![3, 8]);
        client.close(py, a).unwrap();
        client.close(py, b).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                vec![PathBuf::from(format!("fd:{a}"))],
                vec![
                    PathBuf::from(format!("fd:{b}")),
                    PathBuf::from(format!("fd:{a}"))
                ],
            ]
        );
    });
}

#[test]
fn out_of_range_descriptor_does_not_close_a_valid_handle() {
    Python::initialize();
    let root = TestRoot::new();
    std::fs::write(root.0.join("safe"), b"safe-data").unwrap();

    use vfsi_sync::backend::HandleBackend;
    let mut backend = FaultBackend::new(&root.0, None);
    let fd = backend
        .inner
        .open_raw_impl(Path::new("/safe"), libc::O_RDONLY, 0)
        .unwrap()
        .fd()
        .unwrap() as i64;
    let client = client(backend);

    Python::attach(|py| {
        let invalid_fd = fd + (1_i64 << 32);
        let error = client.close(py, invalid_fd).unwrap_err();
        assert!(error.is_instance_of::<PyOSError>(py));

        // The out-of-range value must not wrap around and close the valid fd.
        assert_eq!(client.pread(py, fd, 9, 0).unwrap(), b"safe-data");
        client.close(py, fd).unwrap();
    });
}
