//! Client-side and server-side extent copying.

use super::*;

fn copy_error(cause: std::io::Error) -> vfsi_core::OpError {
    if crate::client::uncertain_completion(&cause) {
        vfsi_core::OpError::unknown(cause, None)
    } else {
        vfsi_core::OpError::failed(cause, None)
    }
}

// Preparation may create the destination, and a later size/CLOSE failure can
// follow confirmed writes. Such a logical copy is no longer untouched.
fn copy_after_effect(error: vfsi_core::OpError) -> vfsi_core::OpError {
    let (cause, state, _) = error.into_parts();
    if state == vfsi_core::FailureState::Unknown {
        vfsi_core::OpError::unknown(cause, None)
    } else {
        vfsi_core::OpError::failed(cause, None)
    }
}

pub(super) fn copy_slots<T>(
    results: vfsi_core::VecResult<T>,
    count: usize,
) -> Vec<vfsi_core::OpResult<T>> {
    use vfsi_core::OpError;
    let mut results = match results {
        Ok(results) if results.len() <= count => results,
        Ok(_) => {
            return (0..count)
                .map(|_| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply("excess COPY outcomes"),
                        None,
                    ))
                })
                .collect();
        }
        Err(cause) => {
            return vfsi_core::internal::errors::fanout(cause, (0..count).len())
                .map(|cause| Err(OpError::not_attempted(cause)))
                .collect();
        }
    };
    results.resize_with(count, || {
        Err(OpError::unknown(
            vfsi_core::internal::errors::invalid_reply("missing COPY outcomes"),
            None,
        ))
    });
    results
}

fn is_server_copy_fallback_status(status: u32) -> bool {
    matches!(
        status,
        nfsstat4_NFS4ERR_NOTSUPP
            | nfsstat4_NFS4ERR_OP_ILLEGAL
            | nfsstat4_NFS4ERR_OFFLOAD_DENIED
            | nfsstat4_NFS4ERR_OFFLOAD_NO_REQS
            | nfsstat4_NFS4ERR_STALE_STATEID
            | nfsstat4_NFS4ERR_OLD_STATEID
            | nfsstat4_NFS4ERR_BAD_STATEID
    )
}

fn copy_rejection_allows_fallback(result: &vfsi_core::OpResult<u64>, copied: u64) -> bool {
    copied == 0 && result.as_ref().err().is_some_and(|error| {
        error.state() != vfsi_core::FailureState::Unknown
            && error.progress().is_none()
            && matches!(vfsi_core::native_status(error.cause()), Some(vfsi_core::StatusCode::Nfs(status)) if is_server_copy_fallback_status(status))
    })
}

fn effective_copy_length(pair: &ExtentPair, source: &VfAttrs) -> VfResult<Option<u64>> {
    match pair.length {
        Some(requested) if requested > 0 => {
            if !source.returned.contains(AttrMask::SIZE) {
                return Err(vfsi_core::internal::errors::invalid_reply(
                    "COPY source size was not returned",
                ));
            }
            Ok(Some(
                requested.min(source.size.saturating_sub(pair.src_offset)),
            ))
        }
        length => Ok(length),
    }
}

// Compare borrowed object identities, never stateids or allocation addresses.
// A source/destination alias within one pair is handled by that pair's copier.
fn copy_pairs_alias<'a>(pairs: impl ExactSizeIterator<Item = (&'a NfsFh, &'a NfsFh)>) -> bool {
    if pairs.len() < 2 {
        return false;
    }
    let mut seen = std::collections::HashSet::with_capacity(pairs.len().saturating_mul(2));
    for (source, destination) in pairs {
        if !seen.insert(source) || (source != destination && !seen.insert(destination)) {
            return true;
        }
    }
    false
}

fn open_destination_with_create_fallback<C, T>(
    client: &mut C,
    mut open: impl FnMut(&mut C, crate::client::OpenCreate) -> VfResult<T>,
    close_source: impl FnOnce(&mut C),
) -> VfResult<T> {
    let destination = match open(client, crate::client::OpenCreate::NoCreate) {
        Ok(destination) => return Ok(destination),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match open(client, crate::client::OpenCreate::Guarded) {
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && !crate::client::uncertain_completion(&error) =>
                {
                    // Another creator won after NOENT. Open once without
                    // CREATE; never replay a creation with an uncertain reply.
                    open(client, crate::client::OpenCreate::NoCreate)
                }
                result => result,
            }
        }
        Err(error) => Err(error),
    };
    destination.inspect_err(|_error| {
        close_source(client);
    })
}

fn copy_extent_data<C>(
    client: &mut C,
    src_offset: u64,
    dst_offset: u64,
    length: Option<u64>,
    mut read: impl FnMut(&mut C, u64, u32) -> VfResult<(Vec<u8>, bool)>,
    mut write: impl FnMut(&mut C, u64, &[u8]) -> VfResult<usize>,
) -> VfResult<u64> {
    let mut so = src_offset;
    let mut doff = dst_offset;
    let mut copied = 0u64;
    loop {
        if length.is_some_and(|length| copied >= length) {
            return Ok(doff);
        }
        let remaining = length.map_or(u64::MAX, |length| length - copied);
        let chunk_len = remaining.min(1 << 20) as u32;
        let (chunk, eof) = read(client, so, chunk_len)?;
        if chunk.len() > chunk_len as usize {
            return Err(std::io::Error::from_raw_os_error((ERR_IO) as i32));
        }
        if chunk.is_empty() {
            return if eof {
                Ok(doff)
            } else {
                Err(std::io::Error::from_raw_os_error((ERR_IO) as i32))
            };
        }
        let chunk_size = u64::try_from(chunk.len())
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        super::io::write_all_at(client, doff, &chunk, |client, offset, suffix| {
            write(client, offset, suffix)
        })?;
        so = so
            .checked_add(chunk_size)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        doff = doff
            .checked_add(chunk_size)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        copied = copied
            .checked_add(chunk_size)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        if eof {
            return Ok(doff);
        }
    }
}

impl NfsVecFs {
    pub(super) fn copy_extent(
        &mut self,
        src_root_rel: &Path,
        dst_root_rel: &Path,
        p: &ExtentPair,
    ) -> VfResult<()> {
        let (sdir, sname) = split_path_bytes(path_bytes(src_root_rel))
            .map_err(|_| std::io::Error::from_raw_os_error((ERR_NOENT) as i32))?;
        let (ddir, dname) = split_path_bytes(path_bytes(dst_root_rel))
            .map_err(|_| std::io::Error::from_raw_os_error((ERR_NOENT) as i32))?;
        let sdirfh = self.resolve_path(&path_from_bytes(sdir), true)?;
        let ddirfh = self.resolve_path(&path_from_bytes(ddir), true)?;
        let (sfh, ssid) = self
            .nfs
            .open_path(
                &sdirfh,
                sname,
                OPEN4_SHARE_ACCESS_READ,
                crate::client::OpenCreate::NoCreate,
            )
            .map_err(vfsi_core::internal::errors::from_rpc)?;
        // Kernel nfsd rejects CREATE_GUARDED on an existing file, so open the
        // destination without create and fall back to create only on NOENT.
        // Any failed destination open must release the already-open source.
        let (dfh, dsid) = open_destination_with_create_fallback(
            &mut self.nfs,
            |nfs, create| {
                nfs.open_path(&ddirfh, dname, OPEN4_SHARE_ACCESS_WRITE, create)
                    .map_err(vfsi_core::internal::errors::from_rpc)
            },
            |nfs| {
                let _ = nfs.close_temporary_path(&sfh, &ssid);
            },
        )?;

        let same_file = sfh == dfh;
        let read_state = if same_file { &dsid } else { &ssid };
        let result = self.copy_opened_extent(&sfh, read_state, &dfh, &dsid, p);
        let source_close = if same_file {
            Ok(())
        } else {
            self.nfs
                .close_temporary_path(&sfh, &ssid)
                .map_err(vfsi_core::internal::errors::from_rpc)
        };
        let destination_close = self
            .nfs
            .close_temporary_path(&dfh, &dsid)
            .map_err(vfsi_core::internal::errors::from_rpc);
        result.and(source_close).and(destination_close)
    }

    fn copy_opened_extent(
        &mut self,
        sfh: &NfsFh,
        ssid: &stateid4,
        dfh: &NfsFh,
        dsid: &stateid4,
        p: &ExtentPair,
    ) -> VfResult<()> {
        let same_file = sfh == dfh;
        (|| {
            let length = if same_file && p.dst_offset > p.src_offset {
                // Forward writes can extend their own source. Bound both the
                // overlap check and every READ by the original available data.
                let Some(length) = p.length else {
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
                };
                let length = if length == 0 {
                    0
                } else {
                    length.min(self.file_size(sfh)?.saturating_sub(p.src_offset))
                };
                if p.dst_offset - p.src_offset < length {
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
                }
                Some(length)
            } else {
                p.length
            };
            let final_size = copy_extent_data(
                &mut self.nfs,
                p.src_offset,
                p.dst_offset,
                length,
                |nfs, offset, count| {
                    nfs.read(sfh, ssid, offset, count)
                        .map_err(vfsi_core::internal::errors::from_rpc)
                },
                |nfs, offset, data| {
                    nfs.write(dfh, dsid, offset, data)
                        .map(|(written, _)| written as usize)
                        .map_err(vfsi_core::internal::errors::from_rpc)
                },
            )?;
            // Truncate any stale tail beyond what was copied (cp semantics).
            self.nfs
                .setattr(dfh, None, Some(final_size))
                .map_err(vfsi_core::internal::errors::from_rpc)
        })()
    }

    fn copy_extents_server_side(&mut self, pairs: &[ExtentPair]) -> Vec<vfsi_core::OpResult<()>> {
        use vfsi_core::{OpError, OpResult};
        let sources: Vec<_> = pairs.iter().map(|pair| pair.src_path.as_path()).collect();
        let opened = copy_slots(
            self.vopen_raw_impl(
                &sources,
                &vec![libc::O_RDONLY; pairs.len()],
                &vec![0; pairs.len()],
            ),
            pairs.len(),
        );
        let mut output: Vec<Option<OpResult<()>>> = (0..pairs.len()).map(|_| None).collect();
        let mut src = (0..pairs.len()).map(|_| None).collect::<Vec<_>>();
        let mut dst = (0..pairs.len()).map(|_| None).collect::<Vec<_>>();
        let mut active = Vec::new();
        let mut retry_from = pairs.len();
        let source_missing = |error: &OpError| {
            error.state() != vfsi_core::FailureState::Unknown
                && error.cause().kind() == std::io::ErrorKind::NotFound
        };
        for (index, result) in opened.into_iter().enumerate() {
            match result {
                Ok(file) => {
                    src[index] = Some(file);
                    active.push(index);
                }
                Err(error) => {
                    if !active.is_empty() && source_missing(&error) {
                        retry_from = retry_from.min(index);
                    }
                    output[index] = Some(Err(error));
                }
            }
        }
        let prefix = active.partition_point(|&index| index < retry_from);
        self.copy_admitted_extents(pairs, &src, &mut dst, &active[..prefix], &mut output);
        // An absent source can be created by an earlier copy, including
        // through an alias. Recheck only definite missing sources, in order.
        // Keep already-open owners until cleanup and preserve other failures.
        if retry_from < pairs.len() {
            use vfsi_core::FailureState;
            let mut failed = Vec::new();
            let record_failure = |index, slot: &mut Option<OpResult<()>>, failed: &mut Vec<_>| {
                let error = slot.as_mut()?.as_mut().err()?;
                match error.state() {
                    FailureState::Unknown => {
                        Some(vfsi_core::internal::errors::share_op_cause(error))
                    }
                    FailureState::Failed => {
                        failed.push((index, vfsi_core::internal::errors::share_op_cause(error)));
                        None
                    }
                    FailureState::NotAttempted => None,
                }
            };
            let mut uncertain = active[..prefix]
                .iter()
                .find_map(|&index| record_failure(index, &mut output[index], &mut failed));
            for index in retry_from..pairs.len() {
                let retry_source = output[index]
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .is_some_and(source_missing);
                let blocker = if retry_source || src[index].is_some() {
                    uncertain.as_ref()
                } else {
                    None
                };
                if blocker.is_none() && retry_source {
                    let opened = self.vopen_raw_impl(
                        &[pairs[index].src_path.as_path()],
                        &[libc::O_RDONLY],
                        &[0],
                    );
                    match copy_slots(opened, 1).pop().expect("one source slot") {
                        Ok(file) => {
                            src[index] = Some(file);
                            output[index] = None;
                        }
                        Err(error) => output[index] = Some(Err(error)),
                    }
                }
                // Re-admitted and preopened sources use the same pinned
                // identity check. A failure on a different file cannot
                // suppress an otherwise valid dependent copy.
                let blocker = blocker.or_else(|| {
                    let source_fd = src[index].as_ref()?.fd()?;
                    let source = self.open_files[&source_fd].fh.clone();
                    failed.iter().find_map(|&(failed_index, ref cause)| {
                        let affected = match dst[failed_index].as_ref().and_then(FileOperand::fd) {
                            Some(fd) => self.open_files[&fd].fh == source,
                            // An OPEN can create a file before local handle
                            // registration fails. Resolve that destination
                            // only when a previously missing source appeared.
                            None if retry_source => {
                                let path = self.server_path(&pairs[failed_index].dst_path);
                                match self.resolve_path(&path, true) {
                                    Ok(destination) => destination == source,
                                    Err(error) => {
                                        error.raw_os_error() != Some(libc::ELOOP)
                                            && !matches!(
                                                error.kind(),
                                                std::io::ErrorKind::NotFound
                                                    | std::io::ErrorKind::NotADirectory
                                            )
                                    }
                                }
                            }
                            None => false,
                        };
                        affected.then_some(cause)
                    })
                });
                if let Some(cause) = blocker {
                    output[index] = Some(Err(OpError::not_attempted(
                        vfsi_core::internal::errors::from_shared(cause),
                    )));
                } else if src[index].is_some() {
                    self.copy_admitted_extents(pairs, &src, &mut dst, &[index], &mut output);
                    uncertain = record_failure(index, &mut output[index], &mut failed);
                }
            }
        }
        let mut cleanup = Vec::new();
        let mut owners = Vec::new();
        for (index, (source, destination)) in src.into_iter().zip(dst).enumerate() {
            for file in [source, destination].into_iter().flatten() {
                owners.push(index);
                cleanup.push(file);
            }
        }
        let count = cleanup.len();
        for (index, result) in owners
            .into_iter()
            .zip(copy_slots(self.close_temporary_descriptors(cleanup), count))
        {
            if let Err(error) = result
                && output[index].as_ref().is_some_and(Result::is_ok)
            {
                output[index] = Some(Err(copy_after_effect(error)));
            }
        }
        output
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(OpError::unknown(
                        vfsi_core::internal::errors::invalid_reply("missing COPY outcome"),
                        None,
                    ))
                })
            })
            .collect()
    }

    fn copy_admitted_extents(
        &mut self,
        pairs: &[ExtentPair],
        src: &[Option<FileOperand>],
        dst: &mut [Option<FileOperand>],
        active: &[usize],
        output: &mut [Option<vfsi_core::OpResult<()>>],
    ) {
        if active.is_empty() {
            return;
        }
        let paths: Vec<_> = active
            .iter()
            .map(|&index| pairs[index].dst_path.as_path())
            .collect();
        let opened = copy_slots(
            self.vopen_raw_impl(
                &paths,
                &vec![libc::O_WRONLY | libc::O_CREAT; paths.len()],
                &vec![0o644; paths.len()],
            ),
            paths.len(),
        );
        let mut ready = Vec::new();
        for (index, result) in active.iter().copied().zip(opened) {
            match result {
                Ok(file) => {
                    dst[index] = Some(file);
                    ready.push(index);
                }
                Err(error) => output[index] = Some(Err(error)),
            }
        }
        // Identity dependencies retain the existing serial backend semantics;
        // unique OPEN owners let us reuse the admitted handles without reopening.
        let aliases = copy_pairs_alias(ready.iter().map(|&index| {
            let source = &self.open_files[&src[index].as_ref().unwrap().fd().unwrap()];
            let destination = &self.open_files[&dst[index].as_ref().unwrap().fd().unwrap()];
            (&source.fh, &destination.fh)
        }));
        if aliases {
            for (position, &index) in ready.iter().enumerate() {
                self.copy_opened_batch(pairs, src, dst, &ready[position..position + 1], output);
                debug_assert!(output[index].is_some());
            }
        } else {
            self.copy_opened_batch(pairs, src, dst, &ready, output);
        }
    }

    fn copy_opened_batch(
        &mut self,
        pairs: &[ExtentPair],
        sources: &[Option<FileOperand>],
        destinations: &[Option<FileOperand>],
        indices: &[usize],
        output: &mut [Option<vfsi_core::OpResult<()>>],
    ) {
        use vfsi_core::OpError;
        let explicit: Vec<_> = indices
            .iter()
            .copied()
            .filter(|&index| pairs[index].length.is_some_and(|length| length > 0))
            .collect();
        let mut attrs: Vec<_> = explicit
            .iter()
            .map(|&index| VfAttrs {
                file: sources[index].as_ref().unwrap().clone(),
                masks: AttrMask::SIZE,
                ..VfAttrs::default()
            })
            .collect();
        #[cfg(feature = "test-faults")]
        let readiness = if attrs.is_empty() {
            Ok(())
        } else {
            self.inject_open_fault(OpenFaultPoint::BeforeCopySourceAttrs)
        };
        #[cfg(not(feature = "test-faults"))]
        let readiness: VfResult<()> = Ok(());
        let results = if attrs.is_empty() {
            Ok(Vec::new())
        } else {
            readiness.and_then(|()| self.vgetattrs_impl(&mut attrs))
        };
        #[cfg(feature = "test-faults")]
        let results = results.map(|mut outcomes| {
            for (index, outcome) in outcomes.iter_mut().enumerate() {
                if let Err(cause) =
                    self.inject_open_fault(OpenFaultPoint::AfterCopySourceAttrs { index })
                {
                    *outcome = Err(copy_error(cause));
                }
            }
            outcomes
        });
        let mut lengths: Vec<_> = indices.iter().map(|&index| pairs[index].length).collect();
        for ((index, attrs), result) in explicit.into_iter().zip(attrs).zip(copy_slots(
            results,
            lengths
                .iter()
                .filter(|length| length.is_some_and(|length| length > 0))
                .count(),
        )) {
            match result.and_then(|()| {
                effective_copy_length(&pairs[index], &attrs).map_err(OpError::not_attempted)
            }) {
                Ok(length) => {
                    lengths[indices
                        .iter()
                        .position(|&original| original == index)
                        .unwrap()] = length
                }
                Err(error) => output[index] = Some(Err(copy_after_effect(error))),
            }
        }
        // GETATTR can recover the session and replace every retained stateid.
        // Snapshot only after that recovery, including each serial alias batch.
        let copies: Vec<_> = indices
            .iter()
            .map(|&index| {
                let source = &self.open_files[&sources[index].as_ref().unwrap().fd().unwrap()];
                let destination =
                    &self.open_files[&destinations[index].as_ref().unwrap().fd().unwrap()];
                crate::client::CopyOp {
                    src_fh: source.fh.clone(),
                    src_stateid: source.stateid,
                    dst_fh: destination.fh.clone(),
                    dst_stateid: destination.stateid,
                    src_offset: pairs[index].src_offset,
                    dst_offset: pairs[index].dst_offset,
                    count: pairs[index].length.unwrap_or(0),
                }
            })
            .collect();
        let mut totals = vec![0u64; copies.len()];
        let mut pending: Vec<_> = (0..indices.len())
            .filter(|&position| output[indices[position]].is_none() && lengths[position] != Some(0))
            .collect();
        // Same-object server COPY remains rejected before dispatch. The
        // client-only copier retains its separately documented alias rules.
        pending.retain(|&position| {
            let op = &copies[position];
            let index = indices[position];
            if op.src_fh == op.dst_fh {
                output[index] = Some(Err(OpError::not_attempted(
                    std::io::Error::from_raw_os_error((ERR_INVAL) as i32),
                )));
                false
            } else if !self.server_copy_enabled {
                output[index] = Some(
                    self.copy_opened_extent(
                        &op.src_fh,
                        &op.src_stateid,
                        &op.dst_fh,
                        &op.dst_stateid,
                        &pairs[index],
                    )
                    .map_err(copy_error),
                );
                false
            } else {
                true
            }
        });
        while !pending.is_empty() {
            let active: Vec<_> = pending
                .iter()
                .map(|&position| {
                    let op = &copies[position];
                    crate::client::CopyOp {
                        src_fh: op.src_fh.clone(),
                        src_stateid: op.src_stateid,
                        dst_fh: op.dst_fh.clone(),
                        dst_stateid: op.dst_stateid,
                        src_offset: op.src_offset + totals[position],
                        dst_offset: op.dst_offset + totals[position],
                        count: lengths[position]
                            .map(|length| length - totals[position])
                            .unwrap_or(0),
                    }
                })
                .collect();
            self.server_copy_stats.requests += 1;
            let outcomes = copy_slots(self.nfs.copy_many(&active), active.len());
            let mut next = Vec::new();
            for (&position, result) in pending.iter().zip(outcomes) {
                let index = indices[position];
                if copy_rejection_allows_fallback(&result, totals[position]) {
                    self.server_copy_enabled = false;
                    self.server_copy_stats.fallbacks += 1;
                    let op = &copies[position];
                    output[index] = Some(
                        self.copy_opened_extent(
                            &op.src_fh,
                            &op.src_stateid,
                            &op.dst_fh,
                            &op.dst_stateid,
                            &pairs[index],
                        )
                        .map_err(copy_error),
                    );
                    continue;
                }
                match result {
                    Ok(count) => {
                        self.server_copy_stats.operations += 1;
                        let op = &copies[position];
                        let updated = totals[position].checked_add(count).filter(|&total| {
                            op.src_offset.checked_add(total).is_some()
                                && op.dst_offset.checked_add(total).is_some()
                        });
                        let Some(total) = updated else {
                            output[index] = Some(Err(OpError::unknown(
                                std::io::Error::from_raw_os_error(libc::EOVERFLOW),
                                None,
                            )));
                            continue;
                        };
                        totals[position] = total;
                        if count != 0 && lengths[position].is_none_or(|length| total < length) {
                            next.push(position);
                        }
                    }
                    Err(error) => {
                        // Destination OPEN may already have created an entry.
                        // Never advertise a logical copy as untouched afterwards.
                        output[index] = Some(Err(copy_after_effect(error)));
                    }
                }
            }
            pending = next;
        }
        let finished: Vec<_> = (0..indices.len())
            .filter(|&position| output[indices[position]].is_none())
            .collect();
        let attrs: Vec<_> = finished
            .iter()
            .map(|&position| crate::client::SetattrOp {
                fh: copies[position].dst_fh.clone(),
                mode: None,
                uid: None,
                gid: None,
                size: copies[position].dst_offset.checked_add(totals[position]),
                atime: None,
                mtime: None,
            })
            .collect();
        for (&position, result) in finished
            .iter()
            .zip(copy_slots(self.nfs.setattr_many(&attrs), attrs.len()))
        {
            output[indices[position]] = Some(result.map_err(copy_after_effect));
        }
    }

    pub(super) fn vcopy_data_nfs(&mut self, pairs: &[ExtentPair]) -> vfsi_core::VecResult<()> {
        self.ensure_writable(pairs.len())?;
        let mut output = Vec::with_capacity(pairs.len());
        // Admission recovery is scoped to an OPEN batch. Use separate calls
        // for dependencies that require completion of an earlier copy.
        for batch in pairs.chunks(8) {
            if self.server_copy_enabled {
                output.extend(self.copy_extents_server_side(batch));
            } else {
                output.extend(copy_slots(self.vcopy_data_impl(batch), batch.len()));
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_alias_detection_matches_cross_pair_identity_conflicts() {
        let handles: Vec<_> = (0u64..128)
            .map(|value| NfsFh::test_from_bytes(value.to_be_bytes().to_vec()))
            .collect();
        for seed in 0usize..32 {
            for count in 0..64 {
                let pairs: Vec<_> = (0..count)
                    .map(|i| {
                        let source = (i * 17 + seed) % handles.len();
                        let destination = (i * (seed + 1) + 7) % handles.len();
                        (&handles[source], &handles[destination])
                    })
                    .collect();
                let expected = pairs.iter().enumerate().any(|(i, (source, destination))| {
                    pairs[i + 1..]
                        .iter()
                        .any(|(other_source, other_destination)| {
                            source == other_source
                                || source == other_destination
                                || destination == other_source
                                || destination == other_destination
                        })
                });
                assert_eq!(copy_pairs_alias(pairs.into_iter()), expected);
            }
        }
        let same_bytes = handles[0].clone();
        assert!(!copy_pairs_alias([(&handles[0], &same_bytes)].into_iter()));
        assert!(copy_pairs_alias(
            [(&handles[0], &handles[1]), (&handles[2], &same_bytes),].into_iter()
        ));
    }

    #[test]
    fn copy_alias_detection_handles_large_independent_and_fanout_batches() {
        let handles: Vec<_> = (0u64..8192)
            .map(|value| NfsFh::test_from_bytes(value.to_be_bytes().to_vec()))
            .collect();
        assert!(!copy_pairs_alias(
            handles
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (&pair[0], &pair[1]))
        ));
        assert!(copy_pairs_alias(
            (1..4096).map(|index| (&handles[0], &handles[index]))
        ));
    }

    #[test]
    fn destination_create_race_reopens_existing_without_replaying_create() {
        use crate::client::OpenCreate;
        for terminal in [None, Some(nfsstat4_NFS4ERR_ACCESS)] {
            let mut client = (Vec::new(), false);
            let result = open_destination_with_create_fallback(
                &mut client,
                |client, create| {
                    client.0.push(create);
                    let status = match client.0.len() {
                        1 => Some(nfsstat4_NFS4ERR_NOENT),
                        2 => Some(nfsstat4_NFS4ERR_EXIST),
                        3 => terminal,
                        _ => panic!("create race retry must be bounded"),
                    };
                    match status {
                        Some(status) => Err(vfsi_core::internal::errors::from_status(
                            vfsi_core::StatusCode::Nfs(status),
                        )),
                        None => Ok(17),
                    }
                },
                |client| client.1 = true,
            );
            assert_eq!(
                client.0,
                [
                    OpenCreate::NoCreate,
                    OpenCreate::Guarded,
                    OpenCreate::NoCreate
                ]
            );
            assert_eq!(client.1, terminal.is_some());
            match terminal {
                Some(code) => assert_eq!(crate::test_native_code(&result.unwrap_err()), code),
                None => assert_eq!(result.unwrap(), 17),
            }
        }
    }

    #[test]
    fn uncertain_destination_create_is_not_replayed() {
        let mut client = (Vec::new(), false);
        let result = open_destination_with_create_fallback(
            &mut client,
            |client, create| {
                client.0.push(create);
                if client.0.len() == 1 {
                    Err::<(), _>(std::io::Error::from(std::io::ErrorKind::NotFound))
                } else {
                    Err(vfsi_core::internal::errors::invalid_reply(
                        "lost create reply",
                    ))
                }
            },
            |client| client.1 = true,
        );
        assert_eq!(client.0.len(), 2);
        assert!(client.1);
        assert_eq!(
            vfsi_core::transport_kind(&result.unwrap_err()),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
    }

    #[test]
    fn failed_guarded_destination_open_closes_the_source() {
        #[derive(Default)]
        struct Client {
            attempts: Vec<crate::client::OpenCreate>,
            source_closed: bool,
        }

        let mut client = Client::default();
        let error = open_destination_with_create_fallback(
            &mut client,
            |client, create| {
                client.attempts.push(create);
                Err::<(), _>(vfsi_core::internal::errors::from_status(
                    vfsi_core::StatusCode::Nfs(match create {
                        crate::client::OpenCreate::NoCreate => nfsstat4_NFS4ERR_NOENT,
                        crate::client::OpenCreate::Guarded => nfsstat4_NFS4ERR_ACCESS,
                        _ => unreachable!(),
                    }),
                ))
            },
            |client| client.source_closed = true,
        )
        .unwrap_err();

        assert_eq!(crate::test_native_code(&(error)), nfsstat4_NFS4ERR_ACCESS);
        assert_eq!(
            client.attempts,
            [
                crate::client::OpenCreate::NoCreate,
                crate::client::OpenCreate::Guarded,
            ]
        );
        assert!(client.source_closed);
    }

    #[test]
    fn explicit_copy_lengths_require_confirmed_size_and_preserve_zero_and_eof() {
        let mut pair = ExtentPair::new("/source", 3, "/destination", 0, Some(4));
        let mut source = VfAttrs {
            size: 9,
            returned: AttrMask::SIZE,
            ..Default::default()
        };
        assert_eq!(effective_copy_length(&pair, &source).unwrap(), Some(4));
        pair.src_offset = 8;
        assert_eq!(effective_copy_length(&pair, &source).unwrap(), Some(1));
        pair.src_offset = u64::MAX;
        assert_eq!(effective_copy_length(&pair, &source).unwrap(), Some(0));
        source.returned = AttrMask::empty();
        source.size = 999; // A stale payload cannot substitute for a returned field.
        let error = effective_copy_length(&pair, &source).unwrap_err();
        assert_eq!(
            vfsi_core::transport_kind(&(error)),
            Some(vfsi_core::TransportKind::InvalidReply)
        );
        for length in [None, Some(0)] {
            pair.length = length;
            assert_eq!(effective_copy_length(&pair, &source).unwrap(), length);
        }
    }

    #[test]
    fn copy_fallback_requires_an_individual_rejection_without_confirmed_progress() {
        use vfsi_core::{IoProgress, OpError};
        let rejected = |status| {
            Err(OpError::failed(
                vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(status)),
                None,
            ))
        };
        let unsupported = nfsstat4_NFS4ERR_OFFLOAD_DENIED;
        assert!(copy_rejection_allows_fallback(&rejected(unsupported), 0));
        assert!(!copy_rejection_allows_fallback(&rejected(unsupported), 8));
        for count in [0, 8] {
            assert!(
                !copy_rejection_allows_fallback(&Ok(count), 0),
                "successful siblings are never replayed"
            );
        }
        assert!(!copy_rejection_allows_fallback(
            &Err(OpError::unknown(
                vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(unsupported)),
                None
            )),
            0
        ));
        assert!(!copy_rejection_allows_fallback(
            &Err(OpError::failed(
                vfsi_core::internal::errors::from_status(vfsi_core::StatusCode::Nfs(unsupported)),
                Some(IoProgress::Write { bytes_written: 1 })
            )),
            0
        ));
        for status in [nfsstat4_NFS4ERR_ACCESS, nfsstat4_NFS4ERR_MOVED] {
            assert!(!copy_rejection_allows_fallback(&rejected(status), 0));
        }
    }

    #[test]
    fn copy_fallback_accepts_eof_and_returns_final_destination_size() {
        let mut client = ();
        let mut reads = 0;
        let mut writes = Vec::new();
        let final_size = copy_extent_data(
            &mut client,
            40,
            12,
            None,
            |_, offset, _| {
                reads += 1;
                assert_eq!(offset, 40);
                Ok((b"abc".to_vec(), true))
            },
            |_, offset, data| {
                writes.push((offset, data.to_vec()));
                Ok(data.len())
            },
        )
        .unwrap();
        assert_eq!(final_size, 15);
        assert_eq!(reads, 1);
        assert_eq!(writes, [(12, b"abc".to_vec())]);

        let final_size = copy_extent_data(
            &mut client,
            40,
            24,
            None,
            |_, _, _| Ok((Vec::new(), true)),
            |_, _, _| panic!("EOF must not write"),
        )
        .unwrap();
        assert_eq!(final_size, 24);
    }

    #[test]
    fn copy_fallback_rejects_empty_non_eof_read() {
        let mut client = ();
        let mut writes = 0;
        let error = copy_extent_data(
            &mut client,
            0,
            0,
            None,
            |_, _, _| Ok((Vec::new(), false)),
            |_, _, data| {
                writes += 1;
                Ok(data.len())
            },
        )
        .unwrap_err();

        assert_eq!(crate::test_native_code(&(error)), ERR_IO);
        assert_eq!(writes, 0);
    }

    #[test]
    fn copy_fallback_rejects_zero_progress_write() {
        let mut client = ();
        let mut writes = 0;
        let error = copy_extent_data(
            &mut client,
            0,
            0,
            None,
            |_, _, _| Ok((b"data".to_vec(), false)),
            |_, _, _| {
                writes += 1;
                Ok(0)
            },
        )
        .unwrap_err();

        assert_eq!(crate::test_native_code(&(error)), ERR_IO);
        assert_eq!(writes, 1);
    }
}

#[cfg(test)]
mod uncached_reply_tests {
    use super::*;

    #[test]
    fn lost_cached_copy_reply_is_unknown_and_cannot_fall_back() {
        let result = Err(copy_error(vfsi_core::internal::errors::from_status(
            vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_RETRY_UNCACHED_REP),
        )));
        let error = result.as_ref().unwrap_err();
        assert_eq!(error.state(), vfsi_core::FailureState::Unknown);
        assert_eq!(
            vfsi_core::native_status(error.cause()),
            Some(vfsi_core::StatusCode::Nfs(
                nfsstat4_NFS4ERR_RETRY_UNCACHED_REP
            ))
        );
        assert!(!copy_rejection_allows_fallback(&result, 0));
    }
}

#[cfg(all(test, feature = "test-faults", feature = "server-copy"))]
mod copy_metadata_recovery_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vfsi_core::internal::faults::FaultInjector;

    struct RejectMetadataOnce {
        calls: AtomicUsize,
        fail_on: usize,
    }

    impl FaultInjector for RejectMetadataOnce {
        fn check(&self, point: &OpenFaultPoint) -> VfResult<()> {
            if matches!(point, OpenFaultPoint::BeforeGetattrDispatch)
                && self.calls.fetch_add(1, Ordering::Relaxed) == self.fail_on
            {
                return Err(vfsi_core::internal::errors::from_status(
                    vfsi_core::StatusCode::Nfs(nfsstat4_NFS4ERR_BADSESSION),
                ));
            }
            Ok(())
        }
    }

    fn copy_after_metadata_recovery(aliases: bool) {
        let server = std::env::var("VFSI_NFS_SERVER").expect("requires an NFS 4.2 COPY fixture");
        let name = format!("copy-metadata-recovery-{}-{aliases}", std::process::id());
        let mut fs = NfsVecFs::connect_with_options(
            &server,
            NfsConnectOptions {
                minorversion: Some(2),
                client_owner: Some(name.as_bytes().to_vec()),
                ..Default::default()
            },
        )
        .unwrap();
        let root = PathBuf::from(format!("/git/{name}"));
        fs.ensure_dir_impl(&root, 0o755).unwrap();
        let source = root.join("source");
        let file = fs
            .open_raw_impl(&source, libc::O_CREAT | libc::O_TRUNC | libc::O_RDWR, 0o600)
            .unwrap();
        fs.write_raw_impl(&file, 0, b"payload").unwrap();
        fs.close_impl(&file).unwrap();
        let targets: Vec<_> = (0..if aliases { 2 } else { 1 })
            .map(|index| root.join(format!("target-{index}")))
            .collect();
        let pairs: Vec<_> = targets
            .iter()
            .map(|target| ExtentPair::from_os_paths(&source, 0, target, 0, Some(7)))
            .collect();
        let injector = Arc::new(RejectMetadataOnce {
            calls: AtomicUsize::new(0),
            fail_on: usize::from(aliases),
        });
        fs.set_fault_injector(injector.clone());
        let outcomes = fs.vcopy_data_nfs(&pairs).unwrap();
        let enabled = fs.server_copy_enabled();
        let stats = fs.server_copy_stats();
        let handles = fs.open_files.len();
        let contents: Vec<_> = targets
            .iter()
            .map(|target| {
                fs.read_file_impl(&FileOperand::from_os_path(target), 16)
                    .unwrap()
            })
            .collect();
        fs.remove_impl(&root, true).unwrap();

        assert!(
            injector.calls.load(Ordering::Relaxed) > injector.fail_on + 1,
            "the rejected metadata query must recover and retry"
        );
        assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
        assert!(enabled, "metadata recovery must not disable server COPY");
        assert_eq!(stats.fallbacks, 0);
        assert_eq!(stats.operations, pairs.len() as u64);
        assert_eq!(stats.requests, if aliases { 2 } else { 1 });
        assert_eq!(handles, 0, "all admitted owners must close after recovery");
        assert!(contents.iter().all(|data| data == b"payload"));
    }

    #[test]
    #[ignore = "requires live NFSv4.2 server COPY"]
    fn independent_copy_metadata_recovery_refreshes_stateids() {
        copy_after_metadata_recovery(false);
    }

    #[test]
    #[ignore = "requires live NFSv4.2 server COPY"]
    fn aliased_copy_metadata_recovery_refreshes_later_pair_stateids() {
        copy_after_metadata_recovery(true);
    }
}
