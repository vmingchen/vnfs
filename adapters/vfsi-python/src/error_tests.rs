use super::*;

#[test]
fn bulk_failures_preserve_native_status_and_original_request_positions() {
    Python::initialize();
    Python::attach(|py| {
        let root = prr_test_support::TestRoot::new();
        let mut backend = prr_test_support::FaultBackend::new(&root.0, None);
        backend.attrs_errors.extend([
            Some(VfError::nfs(0, 2)),
            Some(VfError::smb(0, 0xc000_0257).with_context("vgetattrs", "/second")),
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
        assert_eq!(calls.lock().unwrap().len(), 2);
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
