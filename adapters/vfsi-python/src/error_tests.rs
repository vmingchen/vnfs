use super::*;
use vfsi_sync::backend::HandleBackend;

#[test]
fn bulk_failures_preserve_native_status_and_original_request_positions() {
    Python::initialize();
    Python::attach(|py| {
        let root = prr_test_support::TestRoot::new();
        let mut backend = prr_test_support::FaultBackend::new(&root.0, None);
        backend.attrs_item_errors.extend([
            (0, VfError::nfs(0, 2)),
            (
                1,
                VfError::smb(1, 0xc000_0257).with_context("vgetattrs", "/second"),
            ),
        ]);
        let calls = backend.attrs_calls.clone();
        let (_, errors) = attrs_many_impl(
            &mut backend,
            &["/first".into(), "/second".into()],
            AttrMask::stat(),
            true,
        )
        .unwrap();
        assert_eq!(
            errors[&1].status(),
            Some(vfsi_core::StatusCode::Smb(0xc000_0257))
        );
        assert_eq!(errors[&1].index(), Some(1));
        let dictionary = errors.into_pyobject(py).unwrap();
        let first = dictionary.get_item(0).unwrap().unwrap();
        assert!(first.is_instance_of::<PyFileNotFoundError>());
        let second = dictionary.get_item(1).unwrap().unwrap();
        assert!(second.is_instance_of::<PyNotImplementedError>());
        assert_eq!(
            second.getattr("index").unwrap().extract::<usize>().unwrap(),
            1
        );
        assert_eq!(
            second
                .getattr("status_domain")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "smb"
        );
        assert_eq!(
            second
                .getattr("status_code")
                .unwrap()
                .extract::<u32>()
                .unwrap(),
            0xc000_0257
        );
        assert!(second.str().unwrap().to_string().contains("/second"));
        assert_eq!(calls.lock().unwrap().len(), 1);
    });
}

#[test]
fn scalar_errors_use_portable_categories_and_linux_errno() {
    Python::initialize();
    Python::attach(|py| {
        let source =
            VfError::transport_with_kind(None, vfsi_core::TransportKind::Timeout, "deadline");
        let error = to_py_err(source, Some(Path::new("/file")));
        assert!(error.is_instance_of::<pyo3::exceptions::PyTimeoutError>(py));
        assert_eq!(
            error
                .value(py)
                .getattr("transport_kind")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "timeout"
        );
        let source =
            VfError::transport_with_kind(None, vfsi_core::TransportKind::Auth, "bad credentials");
        assert!(to_py_err(source, None).is_instance_of::<PyPermissionError>(py));
        let source =
            VfError::transport_with_kind(None, vfsi_core::TransportKind::InvalidReply, "bad frame");
        assert!(!to_py_err(source, None).is_instance_of::<PyConnectionError>(py));
        let source = VfError::nfs(7, 63).with_context("vopen", "/file");
        let error = to_py_err(source, None);
        assert_eq!(
            error
                .value(py)
                .getattr("errno")
                .unwrap()
                .extract::<i32>()
                .unwrap(),
            libc::ENAMETOOLONG
        );
        assert_eq!(
            error
                .value(py)
                .getattr("filename")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "/file"
        );
    });
}

#[test]
fn vector_close_errors_preserve_exact_successful_indices() {
    Python::initialize();
    Python::attach(|py| {
        for transport in [false, true] {
            let root = prr_test_support::TestRoot::new();
            let mut backend = prr_test_support::FaultBackend::new(&root.0, None);
            let files: Vec<_> = (0..3)
                .map(|i| {
                    backend
                        .inner
                        .open_raw_impl(
                            Path::new(&format!("/close-{i}")),
                            libc::O_CREAT | libc::O_RDWR,
                            0o600,
                        )
                        .unwrap()
                        .fd()
                        .unwrap() as i64
                })
                .collect();
            let cause = if transport {
                VfError::transport(None, "lost close")
            } else {
                VfError::errno(None, libc::EACCES as u32)
            };
            backend
                .close_item_errors
                .push((0, vfsi_core::OpError::not_attempted(cause)));
            let client = prr_test_support::client(backend);
            let error = client.close_many(py, files, false).unwrap_err();
            assert_eq!(
                error
                    .value(py)
                    .getattr("index")
                    .unwrap()
                    .extract::<usize>()
                    .unwrap(),
                0
            );
            assert_eq!(
                error
                    .value(py)
                    .getattr("_closed_indices")
                    .unwrap()
                    .extract::<Vec<usize>>()
                    .unwrap(),
                [1, 2]
            );
        }
    });
}

#[test]
fn deferred_close_reconciles_a_consumed_owner_without_accepting_normal_invalid_fds() {
    Python::initialize();
    Python::attach(|py| {
        let root = prr_test_support::TestRoot::new();
        let mut backend = prr_test_support::FaultBackend::new(&root.0, None);
        let fd = backend
            .inner
            .open_raw_impl(Path::new("/consumed"), libc::O_CREAT | libc::O_RDWR, 0o600)
            .unwrap()
            .fd()
            .unwrap() as i64;
        backend.consumed_close_error = Some(VfError::errno(None, libc::EIO as u32));
        let client = prr_test_support::client(backend);
        assert!(client.close(py, fd).is_err());
        client.close_many(py, vec![fd], true).unwrap();
        let error = client.close(py, fd).unwrap_err();
        assert_eq!(
            error
                .value(py)
                .getattr("errno")
                .unwrap()
                .extract::<i32>()
                .unwrap(),
            libc::EBADF
        );
        assert!(client.close_many(py, vec![fd], false).is_err());
    });
}

#[test]
fn positioned_python_writes_honor_append_handles_without_false_errors() {
    Python::initialize();
    Python::attach(|py| {
        let root = prr_test_support::TestRoot::new();
        std::fs::write(root.0.join("append"), b"abc").unwrap();
        std::fs::write(root.0.join("position"), b"uvw").unwrap();
        let client = prr_test_support::client(vfsi_posix::backend(&root.0).unwrap());
        let fd = client.open(py, "/append".into(), "ab", None).unwrap();
        client.fseek(py, fd, 1, 0).unwrap();
        assert_eq!(client.pwrite(py, fd, b"x".to_vec(), 0).unwrap(), 1);
        assert_eq!(client.fseek(py, fd, 0, 1).unwrap(), 1);
        assert_eq!(std::fs::read(root.0.join("append")).unwrap(), b"abcx");
        client.close(py, fd).unwrap();
        let fds = client
            .open_many(
                py,
                vec!["/append".into(), "/position".into()],
                vec!["ab".into(), "r+b".into()],
            )
            .unwrap();
        client.fseek(py, fds[0], 1, 0).unwrap();
        client.fseek(py, fds[1], 2, 0).unwrap();
        assert_eq!(
            client
                .pwrite_many(
                    py,
                    fds.clone(),
                    vec![u64::MAX, 1],
                    vec![b"yz".to_vec(), b"Q".to_vec()]
                )
                .unwrap(),
            [2, 1]
        );
        assert_eq!(std::fs::read(root.0.join("append")).unwrap(), b"abcxyz");
        assert_eq!(std::fs::read(root.0.join("position")).unwrap(), b"uQw");
        assert_eq!(client.fseek(py, fds[0], 0, 1).unwrap(), 1);
        assert_eq!(client.fseek(py, fds[1], 0, 1).unwrap(), 2);
        client.close_many(py, fds, false).unwrap();
    });
}

#[test]
fn subset_error_mapping_rejects_indices_outside_the_submitted_subset() {
    let positions = [2, 5];
    for index in [None, Some(0), Some(1), Some(2), Some(5), Some(99)] {
        let source = VfError::nfs(index, 13).with_context("metadata", "/stale");
        let error = remap_subset_error(source.clone(), &positions);
        match index {
            Some(index) if index >= positions.len() => {
                assert_eq!(error.index(), None);
                assert_eq!(error.path(), None);
                assert_eq!(
                    error.transport_kind(),
                    Some(vfsi_core::TransportKind::InvalidReply)
                );
            }
            Some(index) => assert_eq!(error, source.with_index(positions[index])),
            None => assert_eq!(error, source),
        }
    }
}
