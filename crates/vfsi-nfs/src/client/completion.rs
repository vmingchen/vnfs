//! Independent completion decoding for fixed-width logical operations in NFS compounds.
use vfsi_core::{OpError, OpResult, VfError};

/// Preserve every complete logical result before a failed protocol operation.
/// A nonempty vector advances past both successes and the actual failed input.
/// An error rejects execution of the suffix (e.g. a failed SEQUENCE).
pub(super) fn decode_fixed<const OPS: usize, T>(
    requested: usize,
    nops: usize,
    status: u32,
    mut op_status: impl FnMut(usize) -> u32,
    mut extract: impl FnMut(usize) -> OpResult<T>,
) -> Result<Vec<OpResult<T>>, VfError> {
    if nops == 0 {
        return Err(if status == 0 {
            VfError::invalid_reply(None, "missing compound SEQUENCE")
        } else {
            VfError::nfs(None, status)
        });
    }
    let sequence = op_status(0);
    if sequence != 0 {
        return Err(VfError::nfs(None, sequence));
    }
    let mut out = Vec::with_capacity(requested);
    for input in 0..requested {
        let start = 1 + OPS * input;
        for index in start..start + OPS {
            if index >= nops {
                let cause = VfError::invalid_reply(None, "missing logical operation result");
                out.extend((input..requested).map(|_| Err(OpError::unknown(cause.clone(), None))));
                return Ok(out);
            }
            let failed = op_status(index);
            if failed != 0 {
                out.push(Err(OpError::failed(VfError::nfs(None, failed), None)));
                return Ok(out);
            }
        }
        out.push(extract(input));
    }
    Ok(out)
}

/// Finish after infrastructure failure, preserving established input outcomes.
/// Only ranges included in the current submitted compound are uncertain.
pub(super) fn finish_slots<T>(
    mut slots: Vec<Option<OpResult<T>>>,
    submitted: &[(usize, usize, usize)],
    cause: VfError,
) -> Vec<OpResult<T>> {
    for &(input, _, _) in submitted {
        if slots[input].is_none() {
            slots[input] = Some(Err(OpError::unknown(cause.clone(), None)));
        }
    }
    slots
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| Err(OpError::not_attempted(cause.clone()))))
        .collect()
}

impl super::NfsClient {
    /// Pack independent fixed-width logical operations without discarding established
    /// outcomes or replaying successful inputs after a server-side failure.
    pub(super) fn fixed_results<const OPS: usize, T, R>(
        &mut self,
        tag: &[u8],
        ops: &[T],
        byte_budget: usize,
        charge: impl Fn(&T) -> usize,
        mut add: impl FnMut(&mut super::Compound, &T, usize),
        mut extract: impl FnMut(&super::CompoundRes, usize, usize) -> OpResult<R>,
    ) -> vfsi_core::VecResult<R> {
        use vfsi_core::error_from_rpc;
        let safety = if super::batch_is_read_only(tag) {
            super::RequestSafety::ReadOnly
        } else {
            super::RequestSafety::NonIdempotentMutation
        };
        if !ops.is_empty() && self.max_request_arg_bytes() <= crate::compound::SEQUENCE_XDR_BYTES {
            return Err(VfError::nfs(None, nfsv41_sys::nfsstat4_NFS4ERR_REQ_TOO_BIG));
        }
        let mut out = Vec::with_capacity(ops.len());
        let mut dispatched = false;
        let mut max_items = usize::MAX;
        while out.len() < ops.len() {
            let start = out.len();
            let capacity = match self.op_budget_for(tag, 1 + OPS).batch_capacity(OPS) {
                Ok(capacity) => capacity.min(max_items),
                Err(error) => {
                    let cause = error_from_rpc(error, None);
                    if !dispatched {
                        return Err(cause);
                    }
                    out.extend(
                        (start..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))),
                    );
                    break;
                }
            };
            let mut end = start;
            let mut bytes = 0usize;
            while end < ops.len() && end - start < capacity {
                let next = charge(&ops[end]);
                if next > byte_budget.saturating_sub(bytes) {
                    break;
                }
                bytes += next;
                end += 1;
            }
            if end == start {
                let cause = VfError::nfs(None, nfsv41_sys::nfsstat4_NFS4ERR_REP_TOO_BIG);
                if !dispatched {
                    return Err(cause);
                }
                out.extend((start..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))));
                break;
            }
            let mut compound = super::Compound::new();
            compound.tag(tag);
            for op in &ops[start..end] {
                add(&mut compound, op, end - start);
            }
            match self.compound_request_fits(&compound) {
                Ok(false) if end - start > 1 => {
                    max_items = (end - start) / 2;
                    continue;
                }
                Ok(false) => {
                    // One oversized input is a local rejection. Continue with
                    // independent siblings and restore normal packing.
                    out.push(Err(OpError::not_attempted(VfError::nfs(
                        None,
                        nfsv41_sys::nfsstat4_NFS4ERR_REQ_TOO_BIG,
                    ))));
                    max_items = usize::MAX;
                    continue;
                }
                Ok(true) => {}
                Err(error) => {
                    let cause = error_from_rpc(error, None);
                    if !dispatched {
                        return Err(cause);
                    }
                    out.extend(
                        (start..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))),
                    );
                    break;
                }
            }
            if let Err(error) = self.ensure_usable() {
                let cause = error_from_rpc(error, None);
                if !dispatched {
                    return Err(cause);
                }
                out.extend((start..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))));
                break;
            }
            #[cfg(feature = "test-faults")]
            if let Some(injector) = &self.fault_injector
                && let Some(point) = match tag {
                    b"writev" => Some(super::OpenFaultPoint::BeforeWriteCompound { first: start }),
                    b"createv" => {
                        Some(super::OpenFaultPoint::BeforeCreateCompound { first: start })
                    }
                    b"linkv" => Some(super::OpenFaultPoint::BeforeLinkCompound { first: start }),
                    b"renamev" => {
                        Some(super::OpenFaultPoint::BeforeRenameCompound { first: start })
                    }
                    b"copyv" => Some(super::OpenFaultPoint::BeforeCopyCompound { first: start }),
                    b"readdir_pages" | b"readdir_children" => {
                        Some(super::OpenFaultPoint::BeforeReadDirCompound {
                            children: tag == b"readdir_children",
                            first: start,
                            count: end - start,
                        })
                    }
                    b"setattrv" => {
                        Some(super::OpenFaultPoint::BeforeSetattrCompound { first: start })
                    }
                    _ => None,
                }
                && let Err(cause) = injector.check(&point)
            {
                if !dispatched {
                    return Err(cause);
                }
                out.extend((start..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))));
                break;
            }
            dispatched = true;
            let reply = match self.call_compound_with_safety(&mut compound, safety) {
                Ok(reply) => reply,
                Err(error) => {
                    let cause = error_from_rpc(error, None);
                    out.extend((start..end).map(|_| Err(OpError::unknown(cause.clone(), None))));
                    out.extend(
                        (end..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))),
                    );
                    break;
                }
            };
            #[cfg(feature = "test-faults")]
            if let Some(injector) = &self.fault_injector
                && let Some(point) = match tag {
                    b"writev" => {
                        Some(super::OpenFaultPoint::AfterWriteCompoundReply { first: start })
                    }
                    b"readlinkv" => {
                        Some(super::OpenFaultPoint::AfterReadlinkCompoundReply { first: start })
                    }
                    b"readdir_pages" | b"readdir_children" => {
                        Some(super::OpenFaultPoint::AfterReadDirCompoundReply {
                            children: tag == b"readdir_children",
                            first: start,
                            count: end - start,
                        })
                    }
                    b"createv" => {
                        Some(super::OpenFaultPoint::AfterCreateCompoundReply { first: start })
                    }
                    b"linkv" => {
                        Some(super::OpenFaultPoint::AfterLinkCompoundReply { first: start })
                    }
                    b"lookup_typev" => {
                        Some(super::OpenFaultPoint::AfterLookupChunkReply { first: start })
                    }
                    b"setattrv" => {
                        Some(super::OpenFaultPoint::AfterSetattrCompoundReply { first: start })
                    }
                    b"renamev" => {
                        Some(super::OpenFaultPoint::AfterRenameCompoundReply { first: start })
                    }
                    b"copyv" => {
                        Some(super::OpenFaultPoint::AfterCopyCompoundReply { first: start })
                    }
                    _ => None,
                }
                && let Err(cause) = injector.check(&point)
            {
                out.extend((start..end).map(|_| Err(OpError::unknown(cause.clone(), None))));
                out.extend((end..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))));
                break;
            }
            let bad = (0..reply.nops()).find(|&index| reply.op_status(index) != 0);
            let status = bad.map_or(reply.status(), |index| reply.op_status(index));
            if let Some(completed) = super::batch_resource_progress(tag, OPS, status, bad)
                && end - start > 1
            {
                debug_assert_eq!(
                    super::recovery_action(safety, super::FailureCause::ResourceLimit, true),
                    super::RecoveryAction::SplitAndRetry
                );
                let smaller = self
                    .op_budget_for(tag, 1 + OPS)
                    .batch_capacity(OPS)
                    .unwrap_or(0);
                if completed > 0 || (smaller > 0 && smaller < end - start) {
                    // A resource rejection proves the failed pair did not
                    // complete. Retain decoded prefixes and repack only the
                    // untouched/rejected suffix under the learned budget.
                    out.extend(
                        (0..completed.min(end - start))
                            .map(|local| extract(&reply, local, start + local)),
                    );
                    max_items = if completed > 0 { usize::MAX } else { smaller };
                    continue;
                }
            }
            match decode_fixed::<OPS, _>(
                end - start,
                reply.nops(),
                reply.status(),
                |index| reply.op_status(index),
                |local| extract(&reply, local, start + local),
            ) {
                Ok(results) => {
                    if results.len() < end - start {
                        debug_assert_eq!(
                            super::recovery_action(safety, super::FailureCause::ItemStatus, true),
                            super::RecoveryAction::ContinueSuffix
                        );
                    }
                    out.extend(results);
                }
                Err(cause) => {
                    // A failed SEQUENCE executes no logical input. Retain the
                    // existing adaptive resource-budget retry, only if smaller.
                    let smaller = self
                        .op_budget_for(tag, 1 + OPS)
                        .batch_capacity(OPS)
                        .unwrap_or(0);
                    if cause.status().is_some() && smaller > 0 && smaller < end - start {
                        max_items = smaller;
                        continue;
                    }
                    out.extend((start..end).map(|_| {
                        Err(if cause.is_transport() {
                            OpError::unknown(cause.clone(), None)
                        } else {
                            OpError::not_attempted(cause.clone())
                        })
                    }));
                    out.extend(
                        (end..ops.len()).map(|_| Err(OpError::not_attempted(cause.clone()))),
                    );
                    break;
                }
            }
            max_items = usize::MAX;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_operation_mutations_keep_successes_at_every_failure_boundary() {
        for failed in 5..9 {
            let results = decode_fixed::<4, _>(
                3,
                failed + 1,
                13,
                |index| if index == failed { 13 } else { 0 },
                Ok,
            )
            .unwrap();
            assert_eq!(results.len(), 2);
            assert_eq!(results[0], Ok(0));
            assert_eq!(
                results[1].as_ref().unwrap_err().state(),
                vfsi_core::FailureState::Failed
            );
        }
        for nops in 5..9 {
            let results = decode_fixed::<4, _>(3, nops, 0, |_| 0, Ok).unwrap();
            assert_eq!(results[0], Ok(0));
            assert_eq!(results.len(), 3);
            assert!(
                results[1..]
                    .iter()
                    .all(|result| result.as_ref().unwrap_err().state()
                        == vfsi_core::FailureState::Unknown)
            );
        }
    }
    #[test]
    fn destination_error_preserves_validated_siblings_without_replay() {
        let mut extracted = Vec::new();
        let results = decode_fixed::<2, _>(
            3,
            7,
            0,
            |_| 0,
            |index| {
                extracted.push(index);
                if index == 1 {
                    Err(OpError::unknown(
                        VfError::invalid_reply(None, "bad READ length"),
                        None,
                    ))
                } else {
                    Ok(index)
                }
            },
        )
        .unwrap();
        assert_eq!(extracted, [0, 1, 2]);
        assert_eq!(results[0], Ok(0));
        assert_eq!(results[2], Ok(2));
        assert_eq!(
            results[1].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::Unknown
        );
    }

    #[test]
    fn metadata_transport_completion_keeps_prior_values_and_submission_boundaries() {
        let slots = vec![Some(Ok(7u8)), None, None, None];
        let results = finish_slots(
            slots,
            &[(1, 1, 3), (2, 3, 5)],
            VfError::transport(None, "lost reply"),
        );
        assert_eq!(results[0], Ok(7));
        assert_eq!(
            results[1].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::Unknown
        );
        assert_eq!(
            results[2].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::Unknown
        );
        assert_eq!(
            results[3].as_ref().unwrap_err().state(),
            vfsi_core::FailureState::NotAttempted
        );
    }

    #[test]
    fn metadata_success_failure_preserves_prefix_and_excludes_unexecuted_suffix() {
        for statuses in [vec![0, 0, 0, 0, 13], vec![0, 0, 0, 13]] {
            let results = decode_fixed::<2, _>(3, statuses.len(), 13, |i| statuses[i], Ok).unwrap();
            assert_eq!(results.len(), 2);
            assert_eq!(results[0], Ok(0));
            assert_eq!(results[1].as_ref().unwrap_err().cause().raw_code(), 13);
        }
    }
    #[test]
    fn metadata_sequence_rejection_is_not_an_item_failure() {
        assert_eq!(
            decode_fixed::<2, ()>(4, 1, 10008, |_| 10008, |_| Ok(()))
                .unwrap_err()
                .raw_code(),
            10008
        );
        assert!(
            decode_fixed::<2, ()>(4, 0, 0, |_| unreachable!(), |_| Ok(()))
                .unwrap_err()
                .is_transport()
        );
    }
    #[test]
    fn metadata_truncation_preserves_validated_successes() {
        let results = decode_fixed::<2, _>(3, 3, 0, |_| 0, Ok).unwrap();
        assert_eq!(results[0], Ok(0));
        assert_eq!(results.len(), 3);
        for result in &results[1..] {
            assert_eq!(
                result.as_ref().unwrap_err().state(),
                vfsi_core::FailureState::Unknown
            );
        }
    }
    #[test]
    fn metadata_full_reply_keeps_cardinality_and_input_order() {
        let results = decode_fixed::<2, _>(3, 7, 0, |_| 0, |i| Ok(i * 10)).unwrap();
        assert_eq!(results, [Ok(0), Ok(10), Ok(20)]);
    }
}
