use std::path::Path;

use vfsi_core::{
    BatchOutcome, ErrorDomain, OpOutcome, OutcomeCertainty, RetryClass, StatusCode, VfError,
};

#[test]
fn semantic_batch_failure_retains_prefix_and_suffix_state() {
    let outcome = BatchOutcome::from_fail_fast(4, Err(VfError::failure(2, libc::ENOENT as u32)));
    assert!(matches!(outcome.operations()[0], OpOutcome::Success(())));
    assert!(matches!(outcome.operations()[1], OpOutcome::Success(())));
    assert!(matches!(outcome.operations()[2], OpOutcome::Failed(_)));
    assert!(matches!(outcome.operations()[3], OpOutcome::NotAttempted));
}

#[test]
fn transport_failure_is_ambiguous_and_requires_reconciliation() {
    let error = VfError::transport(None, "connection reset").with_context("rename", "/target");
    assert_eq!(error.index_opt(), None);
    assert_eq!(error.domain(), ErrorDomain::Transport);
    assert_eq!(error.status(), None);
    assert_eq!(error.certainty(), OutcomeCertainty::Indeterminate);
    assert_eq!(error.retry_class(), RetryClass::ReconcileFirst);
    assert_eq!(error.operation(), Some("rename"));
    assert_eq!(error.path(), Some(Path::new("/target")));
}

#[test]
fn indexed_transport_failure_keeps_a_proven_completed_prefix() {
    let outcome = BatchOutcome::from_fail_fast(
        3,
        Err(VfError::transport(Some(1), "later compound lost its reply")),
    );
    assert!(matches!(outcome.operations()[0], OpOutcome::Success(())));
    assert!(matches!(
        outcome.operations()[1],
        OpOutcome::Indeterminate(_)
    ));
    assert!(matches!(
        outcome.operations()[2],
        OpOutcome::Indeterminate(_)
    ));
}

#[test]
fn nfs_status_is_not_conflated_with_errno() {
    let error = VfError::from_rpc(vfsi_core::RpcError::op(3, 10044), Some(7));
    assert_eq!(error.domain(), ErrorDomain::Nfs);
    assert_eq!(error.status(), Some(StatusCode::Nfs(10044)));
    assert_eq!(error.index_opt(), Some(7));
}
