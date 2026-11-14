use std::{
    error::Error,
    io::{self, ErrorKind as K},
};
use vfsi_core::{IndexedError, IoProgress, OpError, api::ReadResult};

#[test]
fn unknown_attribution_stays_unknown_without_calling_the_mapper() {
    let error = IndexedError::unattributed(K::InvalidInput.into());
    let error = error.map_index(|_| panic!("unknown input must not become zero"));
    assert_eq!(error.index(), None);
    assert_eq!(error.cause().kind(), K::InvalidInput);
}

#[test]
fn input_zero_and_remapping_live_only_in_the_wrapper() {
    let error = IndexedError::at(0, io::Error::from_raw_os_error(libc::EACCES));
    assert_eq!(error.index(), Some(0));
    let error = error
        .map_index(|index| index + 9)
        .with_context("getattrs_all", "/denied");
    assert_eq!(error.index(), Some(9));
    assert_eq!(
        vfsi_core::native_status(error.cause()),
        Some(vfsi_core::StatusCode::Errno(libc::EACCES as u32))
    );
    assert_eq!(
        vfsi_core::internal::errors::path(error.cause()),
        Some(std::path::Path::new("/denied"))
    );
    assert!(!error.cause().to_string().contains("input 9"));
    assert!(error.to_string().starts_with("input 9:"));
}

#[test]
fn source_and_consuming_parts_keep_the_original_error_object() {
    #[derive(Debug)]
    struct Opaque;
    impl std::fmt::Display for Opaque {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("original")
        }
    }
    impl Error for Opaque {}
    let cause = io::Error::new(K::PermissionDenied, Opaque);
    let pointer = cause.get_ref().unwrap() as *const _ as *const ();
    let indexed = IndexedError::at(3, cause);
    let source = indexed
        .source()
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(source.get_ref().unwrap() as *const _ as *const (), pointer);
    let (index, cause) = indexed.into_parts();
    assert_eq!(index, Some(3));
    assert_eq!(cause.get_ref().unwrap() as *const _ as *const (), pointer);
}

#[test]
fn lossy_selection_discards_progress_but_keeps_the_selected_input_cause() {
    let result = vfsi_core::internal::collect_vec_result::<u8>(
        Ok(vec![
            Ok(1),
            Err(OpError::unknown(
                io::Error::from_raw_os_error(libc::EIO),
                Some(IoProgress::Read(ReadResult::owned(0, vec![1, 2], false))),
            )),
            Err(OpError::failed(
                io::Error::from_raw_os_error(libc::EACCES),
                None,
            )),
        ]),
        3,
    )
    .unwrap_err();
    assert_eq!(result.index(), Some(1));
    assert_eq!(result.into_cause().raw_os_error(), Some(libc::EIO));
}

#[test]
fn malformed_cardinality_is_unattributed_even_with_a_known_failed_slot() {
    let result = vfsi_core::internal::collect_vec_result::<u8>(
        Ok(vec![Err(OpError::failed(
            io::Error::from_raw_os_error(libc::EACCES),
            None,
        ))]),
        2,
    )
    .unwrap_err();
    assert_eq!(result.index(), None);
    assert_eq!(
        vfsi_core::transport_kind(result.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
}
