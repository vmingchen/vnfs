use vfsi_core::api::ReadResult;
use vfsi_core::{FailureState, IoProgress, OpError, OpResult, VecResult};

#[test]
fn independent_results_keep_successes_on_both_sides_of_failure() -> Result<(), std::io::Error> {
    let results: VecResult<u32> = Ok(vec![
        Ok(10),
        Err(OpError::failed(
            std::io::Error::from_raw_os_error(libc::ENOENT),
            None,
        )),
        Ok(30),
    ]);
    let results = results?;
    assert_eq!(*results[0].as_ref().unwrap(), 10);
    assert_eq!(*results[2].as_ref().unwrap(), 30);
    let error = results[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(
        vfsi_core::native_status(error.cause()),
        Some(vfsi_core::StatusCode::Errno(libc::ENOENT as u32))
    );
    Ok(())
}
#[test]
fn unsubmitted_and_unknown_are_distinct_even_with_the_same_cause() {
    let cause =
        vfsi_core::internal::errors::transport(vfsi_core::TransportKind::Other, "lost reply");
    let mut causes = vfsi_core::internal::errors::fanout(cause, 2);
    let untouched = OpError::not_attempted(causes.next().unwrap());
    let unknown = OpError::unknown(
        causes.next().unwrap(),
        Some(IoProgress::Write { bytes_written: 17 }),
    );
    assert_eq!(untouched.state(), FailureState::NotAttempted);
    assert_eq!(untouched.progress(), None);
    assert_eq!(unknown.state(), FailureState::Unknown);
    assert_eq!(
        unknown.progress(),
        Some(&IoProgress::Write { bytes_written: 17 })
    );
}
#[test]
fn owned_and_borrowed_read_prefixes_survive_errors() {
    for read in [
        ReadResult::owned(8, vec![1, 2, 3], false),
        ReadResult::buffered(8, 3, false),
    ] {
        let error = OpError::failed(
            std::io::Error::from_raw_os_error(libc::EIO),
            Some(IoProgress::Read(read.clone())),
        );
        assert_eq!(error.progress(), Some(&IoProgress::Read(read)));
    }
}
#[test]
fn native_cause_context_and_source_survive_normalization() {
    use std::error::Error;
    let cause = vfsi_core::internal::errors::with_context(
        std::io::Error::from_raw_os_error(libc::EACCES),
        "vread",
        "/file",
    );
    let error = OpError::failed(cause, None);
    assert_eq!(
        vfsi_core::internal::errors::path(error.cause()),
        Some(std::path::Path::new("/file"))
    );
    assert_eq!(
        vfsi_core::internal::errors::operation(error.cause()),
        Some("vread")
    );
    assert_eq!(
        vfsi_core::native_status(error.cause()),
        Some(vfsi_core::StatusCode::Errno(libc::EACCES as u32))
    );
    assert!(error.source().unwrap().source().is_some());
    let selected = vfsi_core::IndexedError::at(2, error.into_cause());
    assert_eq!(selected.index(), Some(2));
    assert_eq!(
        selected.cause().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
#[test]
fn outcome_storage_is_bounded_and_successes_need_no_error_allocation() {
    use std::mem::size_of;
    assert!(size_of::<OpError>() <= size_of::<std::io::Error>() + 2 * size_of::<usize>());
    assert!(size_of::<OpResult<usize>>() <= size_of::<OpError>() + size_of::<usize>());
    let empty: VecResult<()> = Ok(Vec::new());
    assert!(matches!(empty, Ok(values) if values.is_empty()));
}

#[test]
fn all_success_collector_uses_input_order_and_checks_cardinality() {
    use vfsi_core::internal::collect_vec_result;
    let result = collect_vec_result::<u8>(
        Ok(vec![
            Err(OpError::failed(
                std::io::Error::from_raw_os_error(libc::EACCES),
                None,
            )),
            Err(OpError::failed(
                std::io::Error::from_raw_os_error(libc::ENOENT),
                None,
            )),
        ]),
        2,
    )
    .unwrap_err();
    assert_eq!(result.index(), Some(0));
    assert_eq!(result.cause().raw_os_error(), Some(libc::EACCES));
    let malformed = collect_vec_result(Ok(vec![Ok(7)]), 2).unwrap_err();
    assert_eq!(malformed.index(), None);
    assert_eq!(
        vfsi_core::transport_kind(malformed.cause()),
        Some(vfsi_core::TransportKind::InvalidReply)
    );
    assert_eq!(
        collect_vec_result(Ok(vec![Ok(7), Ok(8)]), 2).unwrap(),
        [7, 8]
    );
}

#[test]
fn all_success_collector_drops_owned_siblings_on_failure_or_bad_shape() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use vfsi_core::internal::collect_vec_result;
    #[derive(Debug)]
    struct Owned(Arc<AtomicUsize>);
    impl Drop for Owned {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let result = collect_vec_result(
        Ok(vec![
            Ok(Owned(dropped.clone())),
            Err(OpError::failed(
                std::io::Error::from_raw_os_error(libc::EIO),
                None,
            )),
            Ok(Owned(dropped.clone())),
        ]),
        3,
    );
    assert!(result.is_err());
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert!(collect_vec_result(Ok(vec![Ok(Owned(dropped.clone()))]), 2).is_err());
    assert_eq!(dropped.load(Ordering::SeqCst), 3);
}

#[test]
fn consuming_error_parts_moves_owned_progress_without_copying() {
    let data = vec![1, 2, 3];
    let pointer = data.as_ptr();
    let error = OpError::unknown(
        vfsi_core::internal::errors::transport(vfsi_core::TransportKind::Other, "lost suffix"),
        Some(IoProgress::Read(ReadResult::owned(4, data, false))),
    );
    let (cause, state, progress) = error.into_parts();
    assert!(vfsi_core::transport_kind(&(cause)).is_some());
    assert_eq!(state, FailureState::Unknown);
    let Some(IoProgress::Read(read)) = progress else {
        panic!("missing read")
    };
    assert_eq!(read.data().unwrap().as_ptr(), pointer);
    assert_eq!(read.data().unwrap(), &[1, 2, 3]);
}

#[test]
fn write_prefix_merging_retains_knowledge_and_rejects_invalid_progress() {
    use vfsi_core::internal::add_write_prefix;
    use vfsi_core::{FailureState, IoProgress, OpError};
    for state in [
        FailureState::NotAttempted,
        FailureState::Failed,
        FailureState::Unknown,
    ] {
        let cause = std::io::Error::from_raw_os_error(libc::EIO);
        let error = match state {
            FailureState::NotAttempted => OpError::not_attempted(cause),
            FailureState::Failed => OpError::failed(cause, None),
            FailureState::Unknown => OpError::unknown(cause, None),
        };
        let merged = add_write_prefix(error, 2, 5);
        assert_eq!(
            merged.state(),
            if state == FailureState::NotAttempted {
                FailureState::Failed
            } else {
                state
            }
        );
        assert_eq!(
            merged.progress(),
            Some(&IoProgress::Write { bytes_written: 2 })
        );
    }
    for progress in [
        IoProgress::Write { bytes_written: 4 },
        IoProgress::Stream { next_offset: 4 },
        IoProgress::Read(vfsi_core::api::ReadResult::owned(
            0,
            b"wrong".to_vec(),
            false,
        )),
    ] {
        let merged = add_write_prefix(
            OpError::failed(std::io::Error::from_raw_os_error(libc::EIO), Some(progress)),
            2,
            5,
        );
        assert_eq!(merged.state(), FailureState::Unknown);
        assert_eq!(
            merged.progress(),
            Some(&IoProgress::Write { bytes_written: 2 })
        );
    }
}

#[test]
fn accepted_subgroup_normalization_retains_known_owners_and_quarantines_bad_counts() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Owned(Arc<AtomicUsize>);
    impl Drop for Owned {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let owned = || Owned(drops.clone());
    let result = vfsi_core::internal::vec_result_slots(
        Ok(vec![
            Ok(owned()),
            Err(OpError::failed(
                std::io::Error::from_raw_os_error(libc::EACCES),
                None,
            )),
            Ok(owned()),
        ]),
        3,
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(result[0].is_ok() && result[2].is_ok());
    assert_eq!(
        vfsi_core::native_status(result[1].as_ref().err().unwrap().cause()),
        Some(vfsi_core::StatusCode::Errno(libc::EACCES as u32))
    );
    drop(result);
    assert_eq!(drops.load(Ordering::SeqCst), 2);

    let result = vfsi_core::internal::vec_result_slots(Ok(vec![Ok(owned())]), 3);
    assert!(result[0].is_ok());
    for result in &result[1..] {
        assert_eq!(
            result.as_ref().err().unwrap().state(),
            FailureState::Unknown
        );
    }
    drop(result);
    assert_eq!(drops.load(Ordering::SeqCst), 3);

    let result = vfsi_core::internal::vec_result_slots(Ok(vec![Ok(owned()), Ok(owned())]), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 5);
    assert_eq!(
        result[0].as_ref().err().unwrap().state(),
        FailureState::Unknown
    );
    let result = vfsi_core::internal::vec_result_slots::<Owned>(
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
        2,
    );
    assert!(
        result
            .iter()
            .all(|result| result.as_ref().err().unwrap().state() == FailureState::NotAttempted)
    );
}

#[test]
fn selected_attribution_distinguishes_outer_rejection_from_input_zero() {
    use vfsi_core::internal::collect_vec_result;
    let outer = collect_vec_result::<()>(
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported)),
        1,
    )
    .unwrap_err();
    assert_eq!(outer.index(), None);
    let selected = collect_vec_result::<()>(
        Ok(vec![Err(OpError::not_attempted(std::io::Error::from(
            std::io::ErrorKind::Unsupported,
        )))]),
        1,
    )
    .unwrap_err();
    assert_eq!(selected.index(), Some(0));
    assert_eq!(selected.map_index(|index| index + 7).index(), Some(7));
    assert_eq!(outer.map_index(|index| index + 7).index(), None);
}

#[test]
fn accepted_rejection_fanout_keeps_the_original_opaque_source_alive() {
    use std::{
        error::Error,
        fmt,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    #[derive(Debug)]
    struct Opaque(Arc<AtomicUsize>);
    impl fmt::Display for Opaque {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("group failure")
        }
    }
    impl Error for Opaque {}
    impl Drop for Opaque {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let cause = std::io::Error::other(Opaque(drops.clone()));
    let mut slots = vfsi_core::internal::vec_result_slots::<()>(Err(cause), 3);
    for slot in &slots {
        let error = slot.as_ref().unwrap_err();
        assert_eq!(error.state(), FailureState::NotAttempted);
        let original = error
            .cause()
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert!(original.get_ref().unwrap().is::<Opaque>());
    }
    let last = slots.pop().unwrap();
    drop(slots);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(last);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
