use proptest::prelude::*;
use vfsi_core::{VfError, internal::ManyResults};

#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, FaultScript, OpenFaultPoint};

#[test]
fn complete_results_collect_in_request_order() {
    let results = ManyResults::new(3, vec![Ok(10), Ok(20), Ok(30)]);
    assert_eq!(results.requested_len(), 3);
    assert_eq!(results.reported_len(), 3);
    assert!(results.is_complete());
    assert_eq!(results.try_collect().unwrap(), [10, 20, 30]);
}

#[test]
fn vector_position_overrides_a_stale_backend_error_index() {
    let results = ManyResults::new(
        3,
        vec![Ok(10), Err(VfError::failure(99, libc::EACCES as u32))],
    );
    let error = results.try_collect().unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert_eq!(error.err_no(), libc::EACCES as u32);
}

#[test]
fn short_success_prefix_is_a_transport_contract_error() {
    let error = ManyResults::new(3, vec![Ok(10), Ok(20)])
        .try_collect()
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index_opt(), None);
}

#[test]
fn overlong_backend_result_is_a_contract_error_not_a_panic() {
    let error = ManyResults::new(1, vec![Ok(1), Ok(2)])
        .try_collect()
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index_opt(), None);
    assert!(error.to_string().contains("2 results for 1 requests"));
}

#[test]
fn overlong_open_result_cleans_every_returned_handle() {
    let mut closed = Vec::new();
    let error = ManyResults::new(1, vec![Ok(10), Ok(20)])
        .try_collect_with_cleanup(1, |index, handle| {
            closed.push((index, *handle));
            Ok(())
        })
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index_opt(), None);
    assert_eq!(closed, [(0, 10), (1, 20)]);
}

#[test]
fn backend_cannot_override_the_callers_request_count() {
    let mut closed = Vec::new();
    let error = ManyResults::new(1, vec![Ok(10)])
        .try_collect_with_cleanup(2, |index, handle| {
            closed.push((index, *handle));
            Ok(())
        })
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index_opt(), None);
    assert!(
        error
            .to_string()
            .contains("declared 1 requests for 2 inputs")
    );
    assert_eq!(closed, [(0, 10)]);
}

#[test]
fn strict_open_policy_cleans_successful_prefix_on_failure() {
    let results = ManyResults::new(
        4,
        vec![
            Ok(10),
            Ok(20),
            Err(VfError::failure(88, libc::ENOENT as u32)),
        ],
    );
    let mut closed = Vec::new();
    let error = results
        .try_collect_with_cleanup(4, |_, handle| {
            closed.push(*handle);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(2));
    assert_eq!(closed, [10, 20]);
}

#[test]
fn strict_open_policy_cleans_successes_after_concurrent_failure() {
    // SMB may finish independent requests after an earlier request failed.
    let results = ManyResults::new(
        4,
        vec![
            Ok(10),
            Err(VfError::failure(91, libc::EACCES as u32)),
            Ok(30),
            Ok(40),
        ],
    );
    let mut closed = Vec::new();
    let error = results
        .try_collect_with_cleanup(4, |_, handle| {
            closed.push(*handle);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert_eq!(closed, [10, 30, 40]);
}

#[test]
fn concurrent_failures_report_the_lowest_request_position() {
    let results = ManyResults::new(
        5,
        vec![
            Ok(10),
            Err(VfError::failure(99, libc::EACCES as u32)),
            Ok(30),
            Err(VfError::failure(0, libc::ENOENT as u32)),
            Ok(50),
        ],
    );
    let mut closed = Vec::new();
    let error = results
        .try_collect_with_cleanup(5, |index, handle| {
            closed.push((index, *handle));
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert_eq!(error.err_no(), libc::EACCES as u32);
    assert_eq!(closed, [(0, 10), (2, 30), (4, 50)]);
}

#[test]
fn cleanup_failure_does_not_mask_primary_error() {
    let results = ManyResults::new(
        2,
        vec![Ok(10), Err(VfError::failure(0, libc::ENOENT as u32))],
    );
    let error = results
        .try_collect_with_cleanup(2, |_, _| Err(VfError::transport(None, "close failed")))
        .unwrap_err();
    assert_eq!(error.index_opt(), Some(1));
    assert_eq!(error.err_no(), libc::ENOENT as u32);
}

#[test]
fn short_success_prefix_is_cleaned_before_contract_error() {
    let results = ManyResults::new(3, vec![Ok(10), Ok(20)]);
    let mut closed = Vec::new();
    let error = results
        .try_collect_with_cleanup(3, |_, handle| {
            closed.push(*handle);
            Ok(())
        })
        .unwrap_err();
    assert!(error.is_transport());
    assert_eq!(error.index_opt(), None);
    assert_eq!(closed, [10, 20]);
}

#[cfg(feature = "test-faults")]
#[test]
fn fault_script_fires_once_at_the_exact_point() {
    let point = OpenFaultPoint::BeforeRegister { index: 2 };
    let script = FaultScript::one(
        point.clone(),
        VfError::transport(None, "injected registration failure"),
    );
    script
        .check(&OpenFaultPoint::BeforeRegister { index: 1 })
        .unwrap();
    let error = script.check(&point).unwrap_err();
    assert!(error.is_transport());
    assert!(script.is_consumed());
    script.check(&point).unwrap();
    assert_eq!(script.visited().len(), 3);
}

proptest! {
    #[test]
    fn every_nfs_style_failure_index_is_preserved_and_prefix_is_cleaned(
        requested in 1usize..65,
        selector in any::<usize>(),
    ) {
        let failed = selector % requested;
        let mut outcomes: Vec<Result<usize, VfError>> =
            (0..failed).map(Ok).collect();
        outcomes.push(Err(VfError::failure(usize::MAX, libc::ENOENT as u32)));
        let mut closed = Vec::new();
        let error = ManyResults::new(requested, outcomes)
            .try_collect_with_cleanup(requested, |_, handle| {
                closed.push(*handle);
                Ok(())
            })
            .unwrap_err();
        prop_assert_eq!(error.index_opt(), Some(failed));
        prop_assert_eq!(closed, (0..failed).collect::<Vec<_>>());
    }

    #[test]
    fn every_smb_style_failure_cleans_successes_on_both_sides(
        requested in 1usize..65,
        selector in any::<usize>(),
    ) {
        let failed = selector % requested;
        let outcomes = (0..requested)
            .map(|index| {
                if index == failed {
                    Err(VfError::failure(usize::MAX, libc::EACCES as u32))
                } else {
                    Ok(index)
                }
            })
            .collect();
        let mut closed = Vec::new();
        let error = ManyResults::new(requested, outcomes)
            .try_collect_with_cleanup(requested, |_, handle| {
                closed.push(*handle);
                Ok(())
            })
            .unwrap_err();
        let expected: Vec<_> = (0..requested).filter(|index| *index != failed).collect();
        prop_assert_eq!(error.index_opt(), Some(failed));
        prop_assert_eq!(closed, expected);
    }
}
