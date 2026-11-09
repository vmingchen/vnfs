//! Independent public OPEN owners, merged path execution, and finalization.
use super::*;
use vfsi_core::{OpError, OpResult, VecResult, VfError, error_from_rpc};

impl NfsClient {
    /// Open independent inputs in merged compounds. Each successful result
    /// transfers one OPEN owner to the caller; failures retain no caller handle.
    /// Transport loss affects only the submitted chunk, never earlier handles.
    pub fn openv_path_compound(&mut self, ops: &[PathOpenOp]) -> VecResult<(NfsFh, stateid4)> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_usable()
            .map_err(|error| error_from_rpc(error, None))?;
        self.drain_deferred_path_closes()
            .map_err(|error| error_from_rpc(error, None))?;
        let owners: Vec<_> = (0..ops.len())
            .map(|_| self.next_public_open_owner_name())
            .collect();
        let mut slots: Vec<Option<OpResult<(NfsFh, stateid4)>>> =
            (0..ops.len()).map(|_| None).collect();
        for (index, op) in ops.iter().enumerate() {
            if split_path_bytes(&op.path).is_err() {
                slots[index] = Some(Err(OpError::not_attempted(VfError::nfs(
                    None,
                    nfsstat4_NFS4ERR_INVAL,
                ))));
            }
        }
        let per_file = 6;
        let reserve = 16;
        let mut global = 0;
        let mut max_items = usize::MAX;
        let mut dispatched = false;
        #[cfg(feature = "test-faults")]
        let mut chunk_index = 0;
        while global < ops.len() {
            while global < ops.len() && slots[global].is_some() {
                global += 1;
            }
            if global == ops.len() {
                break;
            }
            if let Err(error) = self.ensure_usable() {
                return Ok(completion::finish_slots(
                    slots,
                    &[],
                    error_from_rpc(error, None),
                ));
            }
            let chunk_start = global;
            let budget = self
                .op_budget_for(b"openv1", 1 + per_file)
                .merged_limit(reserve, per_file);
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut compound = Compound::new();
            compound.tag(b"openv1");
            let mut payload = 0;
            while global < ops.len()
                && map.ranges.len() < max_items
                && map.next + per_file <= budget
            {
                if slots[global].is_some() {
                    global += 1;
                    continue;
                }
                if payload > 0
                    && self.max_request_arg_bytes() > 0
                    && payload + 256 > self.max_request_arg_bytes()
                {
                    break;
                }
                let op = &ops[global];
                // Paths were validated before building this compound.
                let (leaf, count) = cursor
                    .set_parent(&mut compound, &op.path)
                    .expect("validated path");
                map.begin(global);
                map.note_ops(count);
                match op.create {
                    OpenCreate::Unchecked => compound.open_claim_null_create_mode(
                        0,
                        op.access,
                        OPEN4_SHARE_DENY_NONE,
                        self.session.clientid,
                        &owners[global],
                        leaf,
                        Some(op.mode.unwrap_or(0o644)),
                        op.truncate,
                    ),
                    create => compound.open_claim_null(
                        0,
                        op.access,
                        OPEN4_SHARE_DENY_NONE,
                        self.session.clientid,
                        &owners[global],
                        make_open_how(create),
                        leaf,
                    ),
                }
                map.note_ops(1);
                compound.getfh();
                map.note_ops(1);
                map.end();
                cursor.descend();
                payload += 256;
                global += 1;
            }
            let planning = if map.ranges.is_empty() {
                Err(RpcError::op(0, nfsstat4_NFS4ERR_TOO_MANY_OPS))
            } else {
                self.merged_paths_need_repack(b"openv1", 1 + per_file, &compound, map.ranges.len())
                    .and_then(|repack| {
                        if repack {
                            Ok(true)
                        } else {
                            CompoundBudget::new(self.max_ops)
                                .ensure(&compound)
                                .map(|_| false)
                        }
                    })
            };
            match planning {
                Ok(true) => {
                    max_items = map.ranges.len() - 1;
                    global = chunk_start;
                    continue;
                }
                Ok(false) => {}
                Err(error) => {
                    let cause = error_from_rpc(error, None);
                    if !dispatched {
                        return Err(cause);
                    }
                    return Ok(completion::finish_slots(slots, &[], cause));
                }
            }
            #[cfg(feature = "test-faults")]
            if let Some(injector) = &self.fault_injector
                && let Err(error) =
                    injector.check(&OpenFaultPoint::BeforeOpenChunk { chunk: chunk_index })
            {
                if !dispatched {
                    return Err(error);
                }
                return Ok(completion::finish_slots(slots, &[], error));
            }
            dispatched = true;
            let reply = match self
                .call_compound_with_safety(&mut compound, RequestSafety::NonIdempotentMutation)
            {
                Ok(reply) => reply,
                Err(error) => {
                    return Ok(completion::finish_slots(
                        slots,
                        &map.ranges,
                        error_from_rpc(error, None),
                    ));
                }
            };
            if resource_rejected_before_mutation(&reply)
                && self.can_retry_merged_resource(b"openv1", &reply, budget, reserve, per_file)
            {
                global = chunk_start;
                continue;
            }
            #[cfg(feature = "test-faults")]
            if let Some(injector) = &self.fault_injector
                && let Err(error) =
                    injector.check(&OpenFaultPoint::AfterOpenChunkReply { chunk: chunk_index })
            {
                // Test-only reply loss: the implementation still owns any
                // decoded resources, though the application cannot use them.
                let mut closes = Vec::new();
                reply.for_each_open_result(
                    map.ranges.iter().map(|&(_, start, end)| (start, end)),
                    |fh, stateid| {
                        closes.push(CloseOp {
                            fh: NfsFh::from_nfs_fh(fh),
                            stateid,
                        })
                    },
                );
                self.close_path_or_defer(closes);
                return Ok(completion::finish_slots(slots, &map.ranges, error));
            }
            let report = match map.analyze(&reply) {
                Ok(report) => report,
                Err(error) => {
                    let mut closes = Vec::new();
                    reply.for_each_open_result(
                        map.ranges.iter().map(|&(_, start, end)| (start, end)),
                        |fh, stateid| {
                            closes.push(CloseOp {
                                fh: NfsFh::from_nfs_fh(fh),
                                stateid,
                            })
                        },
                    );
                    self.close_path_or_defer(closes);
                    return Ok(completion::finish_slots(
                        slots,
                        &map.ranges,
                        VfError::invalid_reply(None, format!("malformed OPEN reply: {error}")),
                    ));
                }
            };
            if let Some(status) = report.compound_failure {
                return Ok(completion::finish_slots(
                    slots,
                    &[],
                    VfError::nfs(None, status),
                ));
            }
            let mut verifier_attrs = Vec::new();
            for &(index, start, end) in map.completed_ranges(&report) {
                // Validated responses follow the request's operation sequence.
                let open_index = (start..end)
                    .find(|&index| reply.op(index).resop == nfs_opnum4_NFS4_OP_OPEN)
                    .expect("OPEN range");
                let open = reply.open(open_index);
                if ops[index].create == OpenCreate::Exclusive {
                    verifier_attrs.push((index, verifier_storage_attrs(&open.attrset)));
                } else if ops[index].truncate && ops[index].create == OpenCreate::NoCreate {
                    verifier_attrs.push((index, VerifierStorageAttrs::default()));
                }
                slots[index] = Some(Ok((
                    NfsFh::from_nfs_fh(reply.getfh(open_index + 1)),
                    open.stateid,
                )));
            }
            self.finalize_path_opens(ops, &mut slots, &verifier_attrs);
            if let Some(failure) = report.failure {
                slots[failure.caller] = Some(Err(OpError::failed(
                    VfError::nfs(None, failure.status),
                    None,
                )));
                // RFC COMPOUND stops at that actual protocol operation.
                // Repack only later logical inputs, never a successful OPEN.
                global = failure.caller + 1;
            }
            max_items = usize::MAX;
            #[cfg(feature = "test-faults")]
            {
                chunk_index += 1;
            }
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("every OPEN resolved"))
            .collect())
    }

    fn finalize_path_opens(
        &mut self,
        ops: &[PathOpenOp],
        slots: &mut [Option<OpResult<(NfsFh, stateid4)>>],
        attrs: &[(usize, VerifierStorageAttrs)],
    ) {
        let timestamp = current_timestamp();
        let mut indices = Vec::new();
        let mut updates = Vec::new();
        let mut closes = Vec::new();
        for &(index, storage) in attrs {
            #[cfg(feature = "test-faults")]
            if let Some(injector) = &self.fault_injector
                && let Err(error) = injector.check(&OpenFaultPoint::BeforeOpenFinalize { index })
            {
                let (fh, stateid) = slots[index]
                    .take()
                    .expect("decoded OPEN")
                    .expect("successful OPEN");
                closes.push(CloseOp { fh, stateid });
                slots[index] = Some(Err(OpError::failed(error, None)));
                continue;
            }
            let (fh, stateid) = slots[index]
                .as_ref()
                .expect("decoded OPEN")
                .as_ref()
                .expect("successful OPEN");
            let update = if ops[index].create == OpenCreate::Exclusive {
                let mode = ops[index].mode.map(|mode| mode & 0o7777);
                if mode.is_none() && !storage.mode && !storage.atime && !storage.mtime {
                    continue;
                }
                verifier_cleanup_setattr(fh, storage, mode, timestamp)
            } else {
                // Non-creating OPEN has no createattrs. Truncation is a
                // dependent SETATTR using this input's acquired write state.
                SetattrOp {
                    fh: fh.clone(),
                    mode: None,
                    size: Some(0),
                    uid: None,
                    gid: None,
                    atime: None,
                    mtime: None,
                }
            };
            indices.push(index);
            updates.push((update, *stateid));
        }
        if updates.is_empty() {
            self.close_path_or_defer(closes);
            return;
        }
        let outcomes = self.fixed_results::<2, _, _>(
            b"setattrv",
            &updates,
            usize::MAX,
            |_| 0,
            |compound, (op, stateid), _| {
                compound.putfh(&op.fh.as_nfs_fh());
                compound.setattr_ownership(
                    op.mode,
                    op.size,
                    (op.uid, op.gid),
                    op.atime,
                    op.mtime,
                    stateid,
                );
            },
            |_, _, _| Ok(()),
        );
        let outcomes = outcomes.unwrap_or_else(|cause| {
            updates
                .iter()
                .map(|_| Err(OpError::not_attempted(cause.clone())))
                .collect()
        });
        for (index, outcome) in indices.into_iter().zip(outcomes) {
            let outcome = outcome.map_err(|error| {
                // OPEN itself is confirmed; only its finalization failed.
                if error.state() == vfsi_core::FailureState::Unknown {
                    error
                } else {
                    OpError::failed(error.into_cause(), None)
                }
            });
            if let Err(error) = outcome {
                let (fh, stateid) = slots[index]
                    .take()
                    .expect("decoded OPEN")
                    .expect("successful OPEN");
                closes.push(CloseOp { fh, stateid });
                slots[index] = Some(Err(error));
            }
        }
        self.close_path_or_defer(closes);
    }
}
