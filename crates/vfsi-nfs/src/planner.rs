//! Failure attribution and recovery policy for vectorized NFS compounds.
//!
//! NFS stops a COMPOUND at its first failing operation.  Consequently, a
//! short response does not mean that every omitted vector item failed: one
//! item failed and the suffix was never executed.  This module keeps that
//! distinction explicit and centralizes the rules for deciding what may be
//! retried or continued.

use std::{collections::HashMap, fmt};

use nfsv41_sys::{nfsstat4_NFS4ERR_RESOURCE, nfsstat4_NFS4ERR_TOO_MANY_OPS};

use crate::compound::CompoundRes;

/// Per-session, per-operation-shape compound size. Successful work is the
/// only probe: a full compound allows the *next* compound to be a little
/// larger, so learning never adds a network round trip of its own.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AdaptiveBatchSizer {
    current: usize,
    ceiling: usize,
}

impl AdaptiveBatchSizer {
    pub(crate) fn new(conservative_start: usize, negotiated_capacity: usize) -> Self {
        let ceiling = negotiated_capacity.max(1);
        Self {
            current: conservative_start.max(1).min(ceiling),
            ceiling,
        }
    }

    /// A nonzero caller cap is a hard maximum, not a request to exceed the
    /// session's learned safe size.
    pub(crate) fn next(&self, remaining: usize, caller_cap: usize) -> usize {
        let cap = if caller_cap == 0 {
            self.current
        } else {
            self.current.min(caller_cap)
        };
        remaining.min(cap)
    }

    pub(crate) fn current(&self) -> usize {
        self.current
    }

    pub(crate) fn succeeded(&mut self, attempted: usize, more_work: bool) {
        if more_work && attempted == self.current {
            // Additive growth avoids jumping from a working batch straight
            // to the negotiated maximum on servers with smaller real limits.
            self.current = self.current.saturating_add(8).min(self.ceiling);
        }
    }

    /// Shrink after a server resource rejection. For an ordered mutation,
    /// `confirmed_prefix` items already succeeded and must not be replayed.
    /// The caller advances by the returned count before retrying the suffix.
    pub(crate) fn rejected(&mut self, attempted: usize, confirmed_prefix: usize) -> usize {
        let confirmed = confirmed_prefix.min(attempted.saturating_sub(1));
        if confirmed > 0 {
            self.ceiling = self.ceiling.min(confirmed);
            self.current = self.current.min(self.ceiling);
        } else if attempted > 1 {
            self.ceiling = self.ceiling.min(attempted - 1);
            self.current = self.current.min((attempted / 2).max(1));
        }
        confirmed
    }
}

/// One capability learner for every compound shape in a session. The COMPOUND
/// tag is the shape key, so a large READ payload cannot reduce the budget for
/// metadata-only REMOVE or LOOKUP requests. Builders consult this before
/// packing work; dispatch observes real replies but never replays mutations.
#[derive(Debug)]
pub(crate) struct AdaptiveCompoundLimits {
    negotiated_ops: usize,
    shapes: HashMap<Vec<u8>, AdaptiveBatchSizer>,
}

impl AdaptiveCompoundLimits {
    pub(crate) fn new(negotiated_ops: usize) -> Self {
        Self {
            negotiated_ops,
            shapes: HashMap::new(),
        }
    }

    fn starting_ops(tag: &[u8], negotiated_ops: usize) -> usize {
        match tag {
            b"removev" => 26, // SEQUENCE + PUTFH + 24 REMOVEs
            b"lookupv" => 73, // SEQUENCE + 24 × (PUTFH, LOOKUP, GETFH)
            // These shapes can retry a semantic resource rejection without
            // replaying a mutation. Leave potentially mutating shapes at the
            // already negotiated maximum rather than probing upward into an
            // avoidable partial side effect.
            b"lookup_typev" | b"getattrv" | b"getattrv1" | b"readlinkv" | b"readv" | b"readv1"
            | b"readv2" | b"readdir_children" | b"readdir_pages" => 96,
            _ => negotiated_ops,
        }
    }

    pub(crate) fn limit(&self, tag: &[u8]) -> usize {
        self.shapes.get(tag).map_or_else(
            || Self::starting_ops(tag, self.negotiated_ops).min(self.negotiated_ops),
            |size| size.next(usize::MAX, 0),
        )
    }

    pub(crate) fn observe(&mut self, tag: &[u8], sent_ops: usize, status: u32, reply_ops: usize) {
        if let Some(limit) = self.shapes.get_mut(tag) {
            Self::record(limit, sent_ops, status, reply_ops);
            return;
        }
        let mut limit = AdaptiveBatchSizer::new(
            Self::starting_ops(tag, self.negotiated_ops),
            self.negotiated_ops,
        );
        Self::record(&mut limit, sent_ops, status, reply_ops);
        self.shapes.insert(tag.to_vec(), limit);
    }

    fn record(limit: &mut AdaptiveBatchSizer, sent_ops: usize, status: u32, reply_ops: usize) {
        if status == NFS_OK {
            // Builders reserve headroom for path resolution. Treat a compound
            // within 16 operations of the current cap as useful evidence.
            if sent_ops >= limit.current().saturating_sub(16) {
                limit.succeeded(limit.current(), true);
            }
        } else if status == nfsstat4_NFS4ERR_RESOURCE || status == nfsstat4_NFS4ERR_TOO_MANY_OPS {
            // NFS4ERR_RESOURCE / NFS4ERR_TOO_MANY_OPS. A failed reply's last
            // operation did not complete; earlier operations may have done so.
            limit.rejected(sent_ops, reply_ops.saturating_sub(1));
        }
    }
}

pub(crate) const NFS_OK: u32 = 0;

/// Minimal response interface, separated from the XDR-backed response so the
/// planner can be tested with deterministic synthetic replies.
pub(crate) trait ReplyView {
    fn status(&self) -> u32;
    fn nops(&self) -> usize;
    fn op_status(&self, index: usize) -> u32;
}

impl ReplyView for CompoundRes {
    fn status(&self) -> u32 {
        self.status()
    }

    fn nops(&self) -> usize {
        self.nops()
    }

    fn op_status(&self, index: usize) -> u32 {
        self.op_status(index)
    }
}

/// A mutation compound can be resent with a smaller budget only when the
/// reply proves no mutating operation ran. All `batch_ops` mutation shapes
/// begin with a non-mutating PUTFH after the implicit SEQUENCE. A failure
/// inside OPEN/WRITE/CLOSE/etc. is *not* safe to replay, even at item zero.
pub(crate) fn resource_rejected_before_mutation<R: ReplyView>(reply: &R) -> bool {
    if reply.status() != nfsstat4_NFS4ERR_RESOURCE
        && reply.status() != nfsstat4_NFS4ERR_TOO_MANY_OPS
    {
        return false;
    }
    match reply.nops() {
        0 | 1 => true,
        2 => reply.op_status(1) != NFS_OK,
        _ => false,
    }
}

/// Maps response-operation positions to caller-visible vector items.
#[derive(Debug)]
pub(crate) struct ExecutionMap {
    /// `(caller index, first op, end op exclusive)` per vector item.
    pub ranges: Vec<(usize, usize, usize)>,
    /// Next response index. Index zero is the implicit SEQUENCE operation.
    pub next: usize,
}

impl Default for ExecutionMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionMap {
    pub(crate) fn new() -> Self {
        Self {
            ranges: Vec::new(),
            next: 1,
        }
    }

    pub(crate) fn begin(&mut self, caller: usize) {
        self.ranges.push((caller, self.next, self.next));
    }

    pub(crate) fn note_ops(&mut self, count: usize) {
        self.next = self.next.saturating_add(count);
    }

    pub(crate) fn end(&mut self) {
        if let Some(last) = self.ranges.last_mut() {
            last.2 = self.next;
        }
    }

    pub(crate) fn range(&self, caller: usize) -> Option<(usize, usize, usize)> {
        self.ranges.iter().copied().find(|r| r.0 == caller)
    }

    pub(crate) fn analyze<R: ReplyView>(
        &self,
        reply: &R,
    ) -> Result<ExecutionReport, MalformedReply> {
        if reply.nops() == 0 {
            if reply.status() != NFS_OK {
                return Ok(ExecutionReport {
                    completed: Vec::new(),
                    failure: None,
                    unexecuted: self.ranges.iter().map(|r| r.0).collect(),
                    compound_failure: Some(reply.status()),
                    compound_failure_op: None,
                });
            }
            return Err(MalformedReply::MissingSequence);
        }
        if reply.op_status(0) != NFS_OK {
            return Ok(ExecutionReport {
                completed: Vec::new(),
                failure: None,
                unexecuted: self.ranges.iter().map(|r| r.0).collect(),
                compound_failure: Some(reply.op_status(0)),
                compound_failure_op: Some(0),
            });
        }

        let mut completed = Vec::new();
        let mut failure = None;
        let mut unexecuted = Vec::new();

        for (position, &(caller, start, end)) in self.ranges.iter().enumerate() {
            if start >= reply.nops() {
                unexecuted.extend(self.ranges[position..].iter().map(|r| r.0));
                break;
            }

            let mut failed_op = None;
            for op_index in start..end.min(reply.nops()) {
                let status = reply.op_status(op_index);
                if status != NFS_OK {
                    failed_op = Some(ItemFailure {
                        caller,
                        op_index,
                        status,
                    });
                    break;
                }
            }

            if let Some(item_failure) = failed_op {
                // RFC 5661 requires execution to stop at the first failed op.
                if reply.nops() != item_failure.op_index + 1 {
                    return Err(MalformedReply::OperationsAfterFailure {
                        failed_op: item_failure.op_index,
                        actual: reply.nops(),
                    });
                }
                if reply.status() != item_failure.status {
                    return Err(MalformedReply::StatusMismatch {
                        compound: reply.status(),
                        operation: item_failure.status,
                    });
                }
                failure = Some(item_failure);
                unexecuted.extend(self.ranges[position + 1..].iter().map(|r| r.0));
                break;
            }

            if end > reply.nops() {
                return Err(MalformedReply::TruncatedItem {
                    caller,
                    expected_end: end,
                    actual: reply.nops(),
                });
            }
            completed.push(caller);
        }

        let (compound_failure, compound_failure_op) =
            if failure.is_none() && reply.status() != NFS_OK {
                // A failing trailing op (for example CLOSE) can intentionally sit
                // outside all item ranges. The caller decides how to handle it.
                let op_index = reply.nops() - 1;
                let operation = reply.op_status(op_index);
                if operation != reply.status() {
                    return Err(MalformedReply::StatusMismatch {
                        compound: reply.status(),
                        operation,
                    });
                }
                (Some(reply.status()), Some(op_index))
            } else {
                (None, None)
            };

        Ok(ExecutionReport {
            completed,
            failure,
            unexecuted,
            compound_failure,
            compound_failure_op,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ItemFailure {
    pub caller: usize,
    pub op_index: usize,
    pub status: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExecutionReport {
    pub completed: Vec<usize>,
    pub failure: Option<ItemFailure>,
    pub unexecuted: Vec<usize>,
    pub compound_failure: Option<u32>,
    /// Response-op index for a SEQUENCE or trailing-op failure. `None` means
    /// the server rejected the compound before returning any operation.
    pub compound_failure_op: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestSafety {
    ReadOnly,
    IdempotentMutation,
    NonIdempotentMutation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureCause {
    /// An item returned an ordinary NFS status. Independent suffix items were
    /// not executed and may be submitted in a fresh compound.
    ItemStatus,
    /// The request exceeded a server-side compound resource limit.
    ResourceLimit,
    /// No response was received. `exact_replay` means the NFS session slot can
    /// replay the identical request and return its cached response.
    Transport { exact_replay: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    ContinueSuffix,
    /// Replay the identical request on the same NFS session slot.
    ReplaySame,
    /// Establish a fresh session before logically retrying a side-effect-free
    /// request; the current slot is poisoned after an ambiguous transport.
    ReconnectAndRetry,
    SplitAndRetry,
    ReturnFailure,
    Ambiguous,
}

/// Decide recovery without conflating "failed" and "never executed" items.
pub(crate) fn recovery_action(
    safety: RequestSafety,
    cause: FailureCause,
    independent_suffix: bool,
) -> RecoveryAction {
    match cause {
        FailureCause::ItemStatus if independent_suffix => RecoveryAction::ContinueSuffix,
        FailureCause::ItemStatus => RecoveryAction::ReturnFailure,
        FailureCause::ResourceLimit => RecoveryAction::SplitAndRetry,
        FailureCause::Transport { exact_replay: true } => RecoveryAction::ReplaySame,
        FailureCause::Transport {
            exact_replay: false,
        } => match safety {
            RequestSafety::ReadOnly => RecoveryAction::ReconnectAndRetry,
            RequestSafety::IdempotentMutation | RequestSafety::NonIdempotentMutation => {
                RecoveryAction::Ambiguous
            }
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MalformedReply {
    MissingSequence,
    TruncatedItem {
        caller: usize,
        expected_end: usize,
        actual: usize,
    },
    OperationsAfterFailure {
        failed_op: usize,
        actual: usize,
    },
    StatusMismatch {
        compound: u32,
        operation: u32,
    },
}

impl fmt::Display for MalformedReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSequence => write!(f, "response omitted SEQUENCE"),
            Self::TruncatedItem {
                caller,
                expected_end,
                actual,
            } => write!(
                f,
                "response truncated caller item {caller}: expected through op {expected_end}, got {actual} ops"
            ),
            Self::OperationsAfterFailure { failed_op, actual } => write!(
                f,
                "response contains operations after failed op {failed_op} ({actual} total ops)"
            ),
            Self::StatusMismatch {
                compound,
                operation,
            } => write!(
                f,
                "compound status {compound} does not match failing operation status {operation}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct MockReply {
        status: u32,
        ops: Vec<u32>,
    }

    impl ReplyView for MockReply {
        fn status(&self) -> u32 {
            self.status
        }

        fn nops(&self) -> usize {
            self.ops.len()
        }

        fn op_status(&self, index: usize) -> u32 {
            self.ops[index]
        }
    }

    fn fixed_map(items: usize, ops_per_item: usize) -> ExecutionMap {
        let mut map = ExecutionMap::new();
        for caller in 0..items {
            map.begin(caller);
            map.note_ops(ops_per_item);
            map.end();
        }
        map
    }

    #[test]
    fn only_pre_mutation_resource_replies_allow_a_smaller_retry() {
        let resource = nfsstat4_NFS4ERR_RESOURCE;
        assert!(resource_rejected_before_mutation(&MockReply {
            status: resource,
            ops: vec![],
        }));
        assert!(resource_rejected_before_mutation(&MockReply {
            status: resource,
            ops: vec![resource],
        }));
        assert!(resource_rejected_before_mutation(&MockReply {
            status: resource,
            ops: vec![0, resource],
        }));
        // An OPEN could have run before a failing GETFH, or the failure
        // could be in a later item after a successful mutation.
        assert!(!resource_rejected_before_mutation(&MockReply {
            status: resource,
            ops: vec![0, 0, resource],
        }));
        assert!(!resource_rejected_before_mutation(&MockReply {
            status: resource,
            ops: vec![0, 0, 0, 0, resource],
        }));
        assert!(!resource_rejected_before_mutation(&MockReply {
            status: 5,
            ops: vec![0, 5],
        }));
    }

    #[test]
    fn reports_completed_failed_and_unexecuted_separately() {
        let map = fixed_map(4, 3);
        // SEQUENCE + item 0 + item 1's first two ops, the second one fails.
        let reply = MockReply {
            status: 2,
            ops: vec![0, 0, 0, 0, 0, 2],
        };
        let report = map.analyze(&reply).unwrap();
        assert_eq!(report.completed, vec![0]);
        assert_eq!(
            report.failure,
            Some(ItemFailure {
                caller: 1,
                op_index: 5,
                status: 2,
            })
        );
        assert_eq!(report.unexecuted, vec![2, 3]);
        assert_eq!(report.compound_failure, None);
    }

    #[test]
    fn exhaustive_failure_positions_preserve_prefix_and_suffix() {
        for items in 1..16 {
            for ops_per_item in 1..8 {
                let map = fixed_map(items, ops_per_item);
                for failed_item in 0..items {
                    for within_item in 0..ops_per_item {
                        let failed_op = 1 + failed_item * ops_per_item + within_item;
                        let mut ops = vec![0; failed_op + 1];
                        ops[failed_op] = 5;
                        let report = map.analyze(&MockReply { status: 5, ops }).unwrap();
                        assert_eq!(report.completed, (0..failed_item).collect::<Vec<_>>());
                        assert_eq!(report.failure.unwrap().caller, failed_item);
                        assert_eq!(
                            report.unexecuted,
                            (failed_item + 1..items).collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mock_executor_resumes_without_reexecuting_the_prefix() {
        let total = 6usize;
        let failures = [1usize, 4];
        let mut cursor = 0usize;
        let mut executed = vec![0usize; total];
        let mut observed_failures = Vec::new();
        let mut submissions = Vec::new();

        while cursor < total {
            submissions.push(cursor..total);
            let mut map = ExecutionMap::new();
            for caller in cursor..total {
                map.begin(caller);
                map.note_ops(2);
                map.end();
            }
            let next_failure = failures.iter().copied().find(|item| *item >= cursor);
            let reply = match next_failure {
                Some(item) => {
                    let failed_op = 1 + (item - cursor) * 2;
                    let mut ops = vec![0; failed_op + 1];
                    ops[failed_op] = 2;
                    MockReply { status: 2, ops }
                }
                None => MockReply {
                    status: 0,
                    ops: vec![0; 1 + (total - cursor) * 2],
                },
            };
            let report = map.analyze(&reply).unwrap();
            for caller in report.completed {
                executed[caller] += 1;
            }
            if let Some(failure) = report.failure {
                executed[failure.caller] += 1;
                observed_failures.push(failure.caller);
                assert_eq!(
                    recovery_action(RequestSafety::ReadOnly, FailureCause::ItemStatus, true),
                    RecoveryAction::ContinueSuffix
                );
                cursor = failure.caller + 1;
            } else {
                cursor = total;
            }
        }

        assert_eq!(submissions, vec![0..6, 2..6, 5..6]);
        assert_eq!(observed_failures, failures);
        assert_eq!(executed, vec![1; total]);
    }

    #[test]
    fn successful_reply_completes_every_item() {
        let map = fixed_map(3, 4);
        let report = map
            .analyze(&MockReply {
                status: 0,
                ops: vec![0; 13],
            })
            .unwrap();
        assert_eq!(report.completed, vec![0, 1, 2]);
        assert!(report.failure.is_none());
        assert!(report.unexecuted.is_empty());
    }

    #[test]
    fn sequence_failure_leaves_all_items_unexecuted() {
        let report = fixed_map(3, 2)
            .analyze(&MockReply {
                status: 10026,
                ops: vec![10026],
            })
            .unwrap();
        assert_eq!(report.compound_failure, Some(10026));
        assert_eq!(report.compound_failure_op, Some(0));
        assert_eq!(report.unexecuted, vec![0, 1, 2]);
    }

    #[test]
    fn trailing_failure_does_not_erase_completed_items() {
        let map = fixed_map(2, 2);
        // One extra trailing operation, such as CLOSE, fails after both item
        // ranges completed successfully.
        let report = map
            .analyze(&MockReply {
                status: 10025,
                ops: vec![0, 0, 0, 0, 0, 10025],
            })
            .unwrap();
        assert_eq!(report.completed, vec![0, 1]);
        assert_eq!(report.compound_failure, Some(10025));
        assert_eq!(report.compound_failure_op, Some(5));
        assert!(report.failure.is_none());
        assert!(report.unexecuted.is_empty());
    }

    #[test]
    fn pre_sequence_resource_rejection_is_recoverable() {
        let report = fixed_map(4, 3)
            .analyze(&MockReply {
                status: 10070,
                ops: vec![],
            })
            .unwrap();
        assert_eq!(report.compound_failure, Some(10070));
        assert_eq!(report.compound_failure_op, None);
        assert_eq!(report.unexecuted, vec![0, 1, 2, 3]);
        assert!(report.completed.is_empty());
        assert!(report.failure.is_none());
    }

    #[test]
    fn malformed_replies_are_rejected() {
        let map = fixed_map(2, 2);
        assert_eq!(
            map.analyze(&MockReply {
                status: 0,
                ops: vec![],
            }),
            Err(MalformedReply::MissingSequence)
        );
        assert_eq!(
            map.analyze(&MockReply {
                status: 5,
                ops: vec![0, 0],
            }),
            Err(MalformedReply::TruncatedItem {
                caller: 0,
                expected_end: 3,
                actual: 2,
            })
        );
        assert_eq!(
            map.analyze(&MockReply {
                status: 5,
                ops: vec![0, 5, 0],
            }),
            Err(MalformedReply::OperationsAfterFailure {
                failed_op: 1,
                actual: 3,
            })
        );
        assert_eq!(
            map.analyze(&MockReply {
                status: 6,
                ops: vec![0, 5],
            }),
            Err(MalformedReply::StatusMismatch {
                compound: 6,
                operation: 5,
            })
        );
    }

    #[test]
    fn recovery_policy_is_failure_and_side_effect_aware() {
        assert_eq!(
            recovery_action(RequestSafety::ReadOnly, FailureCause::ItemStatus, true),
            RecoveryAction::ContinueSuffix
        );
        assert_eq!(
            recovery_action(
                RequestSafety::NonIdempotentMutation,
                FailureCause::ItemStatus,
                false
            ),
            RecoveryAction::ReturnFailure
        );
        assert_eq!(
            recovery_action(
                RequestSafety::ReadOnly,
                FailureCause::Transport {
                    exact_replay: false
                },
                false
            ),
            RecoveryAction::ReconnectAndRetry
        );
        for safety in [
            RequestSafety::IdempotentMutation,
            RequestSafety::NonIdempotentMutation,
        ] {
            assert_eq!(
                recovery_action(
                    safety,
                    FailureCause::Transport {
                        exact_replay: false
                    },
                    false
                ),
                RecoveryAction::Ambiguous
            );
            assert_eq!(
                recovery_action(
                    safety,
                    FailureCause::Transport { exact_replay: true },
                    false
                ),
                RecoveryAction::ReplaySame
            );
        }
        assert_eq!(
            recovery_action(RequestSafety::ReadOnly, FailureCause::ResourceLimit, false),
            RecoveryAction::SplitAndRetry
        );
    }

    #[test]
    fn planners_are_isolated_when_tests_run_concurrently() {
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                std::thread::spawn(move || {
                    let map = fixed_map(32, 3);
                    let failed_item = worker * 4;
                    let failed_op = 1 + failed_item * 3 + 1;
                    let mut ops = vec![0; failed_op + 1];
                    ops[failed_op] = 70 + worker as u32;
                    let report = map
                        .analyze(&MockReply {
                            status: 70 + worker as u32,
                            ops,
                        })
                        .unwrap();
                    (failed_item, report)
                })
            })
            .collect();
        for worker in workers {
            let (failed_item, report) = worker.join().unwrap();
            assert_eq!(report.failure.unwrap().caller, failed_item);
            assert_eq!(report.completed.len(), failed_item);
            assert_eq!(report.unexecuted.len(), 31 - failed_item);
        }
    }

    #[test]
    fn adaptive_batch_learns_only_from_followup_work() {
        let mut remove = AdaptiveBatchSizer::new(24, 100);
        assert_eq!(remove.next(100, 0), 24);
        remove.succeeded(24, false);
        assert_eq!(remove.next(100, 0), 24);
        remove.succeeded(24, true);
        assert_eq!(remove.next(100, 0), 32);
        assert_eq!(remove.next(100, 5), 5);
        remove.succeeded(5, true);
        assert_eq!(remove.next(100, 0), 32);
    }

    #[test]
    fn adaptive_batch_shrinks_and_never_retries_a_rejected_size() {
        let mut remove = AdaptiveBatchSizer::new(24, 100);
        remove.succeeded(24, true);
        assert_eq!(remove.rejected(32, 0), 0);
        assert_eq!(remove.next(100, 0), 16);
        for _ in 0..20 {
            let take = remove.next(100, 0);
            assert!(take < 32);
            remove.succeeded(take, true);
        }
        assert_eq!(remove.next(100, 0), 31);
    }

    #[test]
    fn adaptive_batch_retains_a_confirmed_mutation_prefix() {
        let mut remove = AdaptiveBatchSizer::new(24, 100);
        assert_eq!(remove.rejected(24, 17), 17);
        assert_eq!(remove.next(100, 0), 17);
        remove.succeeded(17, true);
        assert_eq!(remove.next(100, 0), 17);
    }

    #[test]
    fn adaptive_batch_honors_negotiated_limit_and_is_shape_local() {
        let mut remove = AdaptiveBatchSizer::new(24, 7);
        let lookup = AdaptiveBatchSizer::new(24, 40);
        assert_eq!(remove.next(100, 0), 7);
        assert_eq!(lookup.next(100, 0), 24);
        remove.rejected(7, 0);
        assert_eq!(lookup.next(100, 0), 24);
    }

    #[test]
    fn every_compound_shape_learns_without_extra_requests() {
        let mut limits = AdaptiveCompoundLimits::new(256);
        assert_eq!(limits.limit(b"removev"), 26);
        assert_eq!(limits.limit(b"lookupv"), 73);
        assert_eq!(limits.limit(b"readv1"), 96);
        limits.observe(b"readv1", 82, NFS_OK, 82);
        assert_eq!(limits.limit(b"readv1"), 104);
        assert_eq!(limits.limit(b"removev"), 26);
        limits.observe(b"removev", 26, NFS_OK, 26);
        assert_eq!(limits.limit(b"removev"), 34);
    }

    #[test]
    fn mutating_shapes_are_not_upward_probed_but_learn_from_rejection() {
        let mut limits = AdaptiveCompoundLimits::new(256);
        assert_eq!(limits.limit(b"openv1"), 256);
        limits.observe(b"openv1", 200, NFS_OK, 200);
        assert_eq!(limits.limit(b"openv1"), 256);
        limits.observe(b"openv1", 200, nfsstat4_NFS4ERR_RESOURCE, 90);
        assert_eq!(limits.limit(b"openv1"), 89);
    }

    #[test]
    fn resource_feedback_is_shape_local_and_preserves_the_prefix() {
        let mut limits = AdaptiveCompoundLimits::new(256);
        limits.observe(b"writev1", 90, nfsstat4_NFS4ERR_RESOURCE, 40);
        assert_eq!(limits.limit(b"writev1"), 39);
        assert_eq!(limits.limit(b"readv1"), 96);
        limits.observe(b"readv1", 96, nfsstat4_NFS4ERR_TOO_MANY_OPS, 0);
        assert_eq!(limits.limit(b"readv1"), 48);
    }

    #[test]
    fn negotiated_limit_always_wins() {
        let mut limits = AdaptiveCompoundLimits::new(30);
        assert_eq!(limits.limit(b"openv1"), 30);
        for _ in 0..10 {
            limits.observe(b"openv1", 30, NFS_OK, 30);
        }
        assert_eq!(limits.limit(b"openv1"), 30);
    }
}
