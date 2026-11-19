use super::*;

#[test]
fn native_unsupported_errors_keep_their_diagnostic_at_the_c_boundary() {
    for error in [
        vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(10019)),
        vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Smb(0xc000_0257)),
    ] {
        let result = vfsi_result::from_error(vfsi_core::internal::errors::with_context(
            error,
            "vread",
            "/referral",
        ));
        assert_eq!(result.category, VFSI_ERROR_UNSUPPORTED);
        let bytes: Vec<u8> = result
            .message
            .iter()
            .map(|c| c.to_ne_bytes()[0])
            .take_while(|c| *c != 0)
            .collect();
        let message = String::from_utf8(bytes).unwrap();
        assert!(message.contains("unsupported"), "{message}");
        assert!(message.contains("/referral"), "{message}");
    }
}

#[test]
fn aggregate_unit_failures_keep_the_selected_item_indeterminate_category() {
    use vfsi_core::{IoProgress, OpError};
    for error in [
        OpError::unknown(std::io::ErrorKind::Unsupported.into(), None),
        OpError::unknown(std::io::Error::from_raw_os_error(libc::EACCES), None),
        // Invalid unit-operation byte progress is quarantined as Unknown.
        OpError::failed(
            std::io::ErrorKind::Unsupported.into(),
            Some(IoProgress::Write { bytes_written: 1 }),
        ),
    ] {
        let mut items = [vfsi_result::item_success(0)];
        let root = unit_results("vremove", &mut items, Ok(vec![Err(error)]), |_| {
            panic!("failed input cannot retire ownership")
        });
        assert_eq!(items[0].category, VFSI_ERROR_INDETERMINATE);
        assert_eq!(
            root.category, items[0].category,
            "aggregate diagnostic must not advertise fallback after uncertain effects"
        );
        assert_eq!(root.index, 0);
        assert_eq!(root.err_no, items[0].err_no);
    }
}

#[test]
fn copy_unit_results_distinguish_skipped_dependency_from_failed_copy() {
    use vfsi_core::OpError;

    let mut items = [vfsi_result::item_success(0); 3];
    let mut completed = Vec::new();
    let status = unit_results(
        "copy",
        &mut items,
        Ok(vec![
            Err(OpError::failed(
                std::io::Error::from_raw_os_error(libc::EIO),
                None,
            )),
            Err(OpError::not_attempted(std::io::Error::from_raw_os_error(
                libc::EIO,
            ))),
            Ok(()),
        ]),
        |index| completed.push(index),
    );

    assert_eq!(completed, [2]);
    assert_eq!(status.index, 0);
    assert_eq!(status.category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(items[0].category, VFSI_ERROR_FILESYSTEM);
    assert_eq!(items[1].category, VFSI_ERROR_NOT_ATTEMPTED);
    assert_eq!(items[1].index, 1);
    assert_eq!(items[1].err_no, libc::EIO as u32);
    assert_eq!(items[2].category, VFSI_ERROR_NONE);

    let status = unit_results(
        "copy",
        &mut items[..1],
        Ok(vec![Err(OpError::not_attempted(
            std::io::ErrorKind::Unsupported.into(),
        ))]),
        |_| panic!("skipped input cannot retire ownership"),
    );
    assert_eq!(status.category, VFSI_ERROR_UNSUPPORTED);
    assert_eq!(items[0].category, VFSI_ERROR_NOT_ATTEMPTED);
    assert_eq!(items[0].err_no, status.err_no);
}
