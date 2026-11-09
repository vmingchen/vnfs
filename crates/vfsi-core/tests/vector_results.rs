use vfsi_core::api::ReadResult;
use vfsi_core::{FailureState, IoProgress, OpError, OpResult, VecResult, VfError};

#[test]
fn independent_results_keep_successes_on_both_sides_of_failure() -> Result<(), VfError> {
    let results: VecResult<u32> = Ok(vec![
        Ok(10),
        Err(OpError::failed(
            VfError::errno(99, libc::ENOENT as u32),
            None,
        )),
        Ok(30),
    ]);
    let results = results?;
    assert_eq!(results[0], Ok(10));
    assert_eq!(results[2], Ok(30));
    let error = results[1].as_ref().unwrap_err();
    assert_eq!(error.state(), FailureState::Failed);
    assert_eq!(error.cause().index(), None);
    assert_eq!(
        error.cause().status(),
        Some(vfsi_core::StatusCode::Errno(libc::ENOENT as u32))
    );
    Ok(())
}
#[test]
fn unsubmitted_and_unknown_are_distinct_even_with_the_same_cause() {
    let cause = VfError::transport(None, "lost reply");
    let untouched = OpError::not_attempted(cause.clone());
    let unknown = OpError::unknown(cause, Some(IoProgress::Write { bytes_written: 17 }));
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
            VfError::errno(8, libc::EIO as u32),
            Some(IoProgress::Read(read.clone())),
        );
        assert_eq!(error.progress(), Some(&IoProgress::Read(read)));
    }
}
#[test]
fn native_cause_context_and_source_survive_normalization() {
    use std::error::Error;
    let cause = VfError::from_io(23, std::io::Error::from_raw_os_error(libc::EACCES))
        .with_context("vread", "/file");
    let error = OpError::failed(cause.clone(), None);
    assert_eq!(error.cause().path(), cause.path());
    assert_eq!(error.cause().operation(), cause.operation());
    assert_eq!(error.cause().status(), cause.status());
    assert!(error.source().unwrap().source().is_some());
    let legacy = error.into_cause().with_index(2);
    assert_eq!(legacy.index(), Some(2));
    assert_eq!(legacy.kind(), std::io::ErrorKind::PermissionDenied);
}
#[test]
fn outcome_storage_is_bounded_and_successes_need_no_error_allocation() {
    use std::mem::size_of;
    assert!(size_of::<OpError>() <= size_of::<VfError>() + 2 * size_of::<usize>());
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
                VfError::errno(None, libc::EACCES as u32),
                None,
            )),
            Err(OpError::failed(
                VfError::errno(None, libc::ENOENT as u32),
                None,
            )),
        ]),
        2,
    )
    .unwrap_err();
    assert_eq!(result.index(), Some(0));
    assert_eq!(result.raw_code(), libc::EACCES as u32);
    assert!(
        collect_vec_result(Ok(vec![Ok(7)]), 2)
            .unwrap_err()
            .is_transport()
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
                VfError::errno(None, libc::EIO as u32),
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
        VfError::transport(None, "lost suffix"),
        Some(IoProgress::Read(ReadResult::owned(4, data, false))),
    );
    let (cause, state, progress) = error.into_parts();
    assert!(cause.is_transport());
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
    use vfsi_core::{FailureState, IoProgress, OpError, VfError};
    for state in [
        FailureState::NotAttempted,
        FailureState::Failed,
        FailureState::Unknown,
    ] {
        let cause = VfError::errno(8, libc::EIO as u32);
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
        assert_eq!(merged.cause().index(), None);
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
            OpError::failed(VfError::errno(None, libc::EIO as u32), Some(progress)),
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
                VfError::errno(99, libc::EACCES as u32),
                None,
            )),
            Ok(owned()),
        ]),
        3,
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(result[0].is_ok() && result[2].is_ok());
    assert_eq!(result[1].as_ref().err().unwrap().cause().index(), None);
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
    let result = vfsi_core::internal::vec_result_slots::<Owned>(Err(VfError::unsupported(None)), 2);
    assert!(
        result
            .iter()
            .all(|result| result.as_ref().err().unwrap().state() == FailureState::NotAttempted)
    );
}
