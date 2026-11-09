//! Shared packing and completion for bounded path mutations.
use super::*;

impl NfsClient {
    #[allow(clippy::too_many_arguments)] // Shared wire builder, decoder, repair and dispatch boundary.
    pub(super) fn path_mutation_results<T>(
        &mut self,
        tag: &'static [u8],
        per_file: usize,
        count: usize,
        mut build: impl FnMut(&mut Compound, &mut CfhCursor, usize) -> vfsi_core::VfResult<usize>,
        mut decode: impl FnMut(&CompoundRes, usize, usize) -> vfsi_core::OpResult<T>,
        mut repairable: impl FnMut(u32, u32) -> bool,
        mut readiness: impl FnMut(usize) -> vfsi_core::VfResult<()>,
    ) -> vfsi_core::VfResult<PathMutationRun<T>> {
        use vfsi_core::{OpError, VfError, error_from_rpc};
        if count != 0 && self.max_ops < 3 {
            return Err(VfError::nfs(None, nfsstat4_NFS4ERR_TOO_MANY_OPS));
        }
        let mut slots = (0..count).map(|_| None).collect::<Vec<_>>();
        let mut submitted = Vec::new();
        let mut dispatched = false;
        let mut repair = None;
        let operation = (|| -> vfsi_core::VfResult<()> {
            let reserve = if tag == b"removev" { 0 } else { 16 };
            let mut global = 0;
            let mut max_items = usize::MAX;
            while global < count {
                submitted.clear();
                let budget = self
                    .op_budget_for(tag, 1 + per_file)
                    .merged_limit(reserve, per_file);
                let chunk_start = global;
                let mut cursor = CfhCursor::default();
                let mut map = ExecutionMap::new();
                let mut compound = Compound::new();
                compound.tag(tag);
                let mut payload = 0;
                while global < count
                    && map.ranges.len() < max_items
                    && map.next + per_file <= budget
                {
                    let estimate = if tag == b"removev1" { 128 } else { 256 };
                    if tag != b"removev"
                        && payload > 0
                        && self.max_request_arg_bytes() > 0
                        && payload + estimate > self.max_request_arg_bytes()
                    {
                        break;
                    }
                    // Builders must validate a rejected input before appending wire operations.
                    let operation_count = match build(&mut compound, &mut cursor, global) {
                        Ok(count) => count,
                        Err(cause) => {
                            slots[global] = Some(Err(OpError::not_attempted(cause)));
                            global += 1;
                            continue;
                        }
                    };
                    map.begin(global);
                    map.note_ops(operation_count);
                    map.end();
                    if !matches!(tag, b"removev" | b"removev1") {
                        cursor.descend();
                    }
                    payload += estimate;
                    global += 1;
                }
                if map.ranges.is_empty() {
                    if global == count {
                        break;
                    }
                    slots[global] = Some(Err(OpError::not_attempted(VfError::nfs(
                        None,
                        nfsstat4_NFS4ERR_TOO_MANY_OPS,
                    ))));
                    global += 1;
                    continue;
                }
                if self
                    .merged_paths_need_repack(tag, 1 + per_file, &compound, map.ranges.len())
                    .map_err(|error| error_from_rpc(error, None))?
                {
                    max_items = if tag == b"removev" {
                        (map.ranges.len() / 2).max(1)
                    } else {
                        map.ranges.len() - 1
                    };
                    global = chunk_start;
                    continue;
                }
                if let Err(error) = CompoundBudget::new(self.max_ops).ensure(&compound) {
                    if map.ranges.len() == 1 {
                        slots[map.ranges[0].0] =
                            Some(Err(OpError::not_attempted(error_from_rpc(error, None))));
                        max_items = usize::MAX;
                        continue;
                    }
                    return Err(error_from_rpc(error, None));
                }
                if !self
                    .compound_request_fits(&compound)
                    .map_err(|error| error_from_rpc(error, None))?
                {
                    if map.ranges.len() == 1 {
                        let index = map.ranges[0].0;
                        slots[index] = Some(Err(OpError::not_attempted(VfError::nfs(
                            None,
                            nfsstat4_NFS4ERR_REQ_TOO_BIG,
                        ))));
                        max_items = usize::MAX;
                        continue;
                    }
                    return Err(VfError::nfs(None, nfsstat4_NFS4ERR_REQ_TOO_BIG));
                }
                self.ensure_usable()
                    .map_err(|error| error_from_rpc(error, None))?;
                readiness(chunk_start)?;
                #[cfg(feature = "test-faults")]
                if let Some(injector) = &self.fault_injector {
                    let point = match tag {
                        b"setattrv1" => {
                            OpenFaultPoint::BeforeSetattrCompound { first: chunk_start }
                        }
                        b"renamev1" => OpenFaultPoint::BeforeRenameCompound { first: chunk_start },
                        b"removev" | b"removev1" => {
                            OpenFaultPoint::BeforeRemoveCompound { first: chunk_start }
                        }
                        _ => unreachable!("path mutation tag"),
                    };
                    injector.check(&point)?;
                }
                submitted.clone_from(&map.ranges);
                dispatched = true;
                let reply = self
                    .call_compound_with_safety(&mut compound, RequestSafety::NonIdempotentMutation)
                    .map_err(|error| error_from_rpc(error, None))?;
                #[cfg(feature = "test-faults")]
                if let Some(injector) = &self.fault_injector {
                    let point = match tag {
                        b"setattrv1" => {
                            OpenFaultPoint::AfterSetattrCompoundReply { first: chunk_start }
                        }
                        b"renamev1" => {
                            OpenFaultPoint::AfterRenameCompoundReply { first: chunk_start }
                        }
                        b"removev" | b"removev1" => {
                            OpenFaultPoint::AfterRemoveCompoundReply { first: chunk_start }
                        }
                        _ => unreachable!("path mutation tag"),
                    };
                    injector.check(&point)?;
                }
                if self.can_retry_merged_resource(tag, &reply, budget, reserve, per_file) {
                    global = chunk_start;
                    max_items = usize::MAX;
                    continue;
                }
                let report = map.analyze(&reply).map_err(|cause| {
                    VfError::invalid_reply(None, format!("malformed path mutation reply: {cause}"))
                })?;
                for &(input, start, end) in map.completed_ranges(&report) {
                    slots[input] = Some(decode(&reply, start, end));
                }
                max_items = usize::MAX;
                if let Some(failure) = report.failure {
                    slots[failure.caller] = Some(Err(OpError::failed(
                        VfError::nfs(None, failure.status),
                        None,
                    )));
                    let failed_op = reply.op(failure.op_index).resop;
                    if repairable(failure.status, failed_op) {
                        repair = Some(PathRepair {
                            input: failure.caller,
                            failed_op,
                        });
                        break;
                    }
                    global = failure.caller + 1;
                } else if let Some(status) = report.compound_failure {
                    submitted.clear();
                    return Err(VfError::nfs(None, status));
                }
            }
            Ok(())
        })();
        if let Err(cause) = operation {
            if !dispatched {
                return Err(cause);
            }
            return Ok((completion::finish_slots(slots, &submitted, cause), None));
        }
        if let Some(repair_point) = repair {
            let cause = slots[repair_point.input]
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .expect("repair has a failed lookup or guard")
                .cause()
                .clone();
            Ok((completion::finish_slots(slots, &[], cause), repair))
        } else {
            Ok((
                slots
                    .into_iter()
                    .map(|slot| {
                        slot.unwrap_or_else(|| {
                            Err(OpError::unknown(
                                VfError::invalid_reply(None, "missing path mutation result"),
                                None,
                            ))
                        })
                    })
                    .collect(),
                None,
            ))
        }
    }
}
