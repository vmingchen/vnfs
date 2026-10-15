//! Client-side and server-side extent copying.

use super::*;

struct CopyBatchFailure {
    error: VfError,
    fallback_safe: bool,
}

impl CopyBatchFailure {
    fn without_fallback(error: VfError) -> Self {
        Self {
            error,
            fallback_safe: false,
        }
    }
}

impl From<VfError> for CopyBatchFailure {
    fn from(error: VfError) -> Self {
        let fallback_safe = is_server_copy_fallback_status(error.err_no());
        Self {
            error,
            fallback_safe,
        }
    }
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

fn server_copy_failure_allows_fallback(status: u32, active_index: usize, totals: &[u64]) -> bool {
    is_server_copy_fallback_status(status)
        && active_index == 0
        && totals.iter().all(|&total| total == 0)
}

fn open_destination_with_create_fallback<C, T>(
    client: &mut C,
    mut open: impl FnMut(&mut C, crate::client::OpenCreate) -> VfResult<T>,
    close_source: impl FnOnce(&mut C),
) -> VfResult<T> {
    let destination = match open(client, crate::client::OpenCreate::NoCreate) {
        Ok(destination) => return Ok(destination),
        Err(error) if error.err_no() == ERR_NOENT => {
            open(client, crate::client::OpenCreate::Guarded)
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
            return Err(VfError::client(0, ERR_IO));
        }
        if chunk.is_empty() {
            return if eof {
                Ok(doff)
            } else {
                Err(VfError::client(0, ERR_IO))
            };
        }
        let chunk_size =
            u64::try_from(chunk.len()).map_err(|_| VfError::failure(0, libc::EOVERFLOW as u32))?;
        super::io::write_all_at(client, 0, doff, &chunk, |client, offset, suffix| {
            write(client, offset, suffix)
        })?;
        so = so
            .checked_add(chunk_size)
            .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
        doff = doff
            .checked_add(chunk_size)
            .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
        copied = copied
            .checked_add(chunk_size)
            .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
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
            .map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let (ddir, dname) = split_path_bytes(path_bytes(dst_root_rel))
            .map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let sdirfh = self
            .resolve_path(&path_from_bytes(&sdir), true)
            .map_err(|e| e.with_index(0))?;
        let ddirfh = self
            .resolve_path(&path_from_bytes(&ddir), true)
            .map_err(|e| e.with_index(0))?;
        let (sfh, ssid) = self
            .nfs
            .open_path(
                &sdirfh,
                &sname,
                OPEN4_SHARE_ACCESS_READ,
                crate::client::OpenCreate::NoCreate,
            )
            .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
        // Kernel nfsd rejects CREATE_GUARDED on an existing file, so open the
        // destination without create and fall back to create only on NOENT.
        // Any failed destination open must release the already-open source.
        let (dfh, dsid) = open_destination_with_create_fallback(
            &mut self.nfs,
            |nfs, create| {
                nfs.open_path(&ddirfh, &dname, OPEN4_SHARE_ACCESS_WRITE, create)
                    .map_err(|error| vfsi_core::error_from_rpc(error, 0))
            },
            |nfs| {
                let _ = nfs.close_path(&sfh, &ssid);
            },
        )?;

        let result = copy_extent_data(
            &mut self.nfs,
            p.src_offset,
            p.dst_offset,
            p.length,
            |nfs, offset, count| {
                nfs.read(&sfh, &ssid, offset, count)
                    .map_err(|error| vfsi_core::error_from_rpc(error, 0))
            },
            |nfs, offset, data| {
                nfs.write(&dfh, &dsid, offset, data)
                    .map(|(written, _)| written as usize)
                    .map_err(|error| vfsi_core::error_from_rpc(error, 0))
            },
        )
        .and_then(|final_size| {
            // Truncate any stale tail beyond what was copied (cp semantics).
            self.nfs
                .setattr(&dfh, None, Some(final_size))
                .map_err(|error| vfsi_core::error_from_rpc(error, 0))
        });
        let source_close = self
            .nfs
            .close_path(&sfh, &ssid)
            .map_err(|e| vfsi_core::error_from_rpc(e, 0));
        let destination_close = self
            .nfs
            .close_path(&dfh, &dsid)
            .map_err(|e| vfsi_core::error_from_rpc(e, 0));
        result.and(source_close).and(destination_close)
    }

    fn copy_extents_server_side(&mut self, pairs: &[ExtentPair]) -> Result<(), CopyBatchFailure> {
        let src_files: Vec<FileOperand> = pairs
            .iter()
            .map(|p| FileOperand::from_os_path(&p.src_path))
            .collect();
        let dst_files: Vec<FileOperand> = pairs
            .iter()
            .map(|p| FileOperand::from_os_path(&p.dst_path))
            .collect();
        let src_refs: Vec<&FileOperand> = src_files.iter().collect();
        let dst_refs: Vec<&FileOperand> = dst_files.iter().collect();
        let no_create = vec![false; pairs.len()];
        let create = vec![true; pairs.len()];
        let no_truncate = vec![false; pairs.len()];

        let src_tmp = self
            .open_path_batch(&src_refs, &no_create, false, &no_truncate)
            .map_err(CopyBatchFailure::from)?;
        let dst_tmp = match self.open_path_batch(&dst_refs, &create, true, &no_truncate) {
            Ok(tmp) => tmp,
            Err(e) => {
                self.close_tmp(&src_tmp);
                return Err(CopyBatchFailure::from(e));
            }
        };

        // OPENs through the shared path owner can advance the stateid of any
        // earlier OPEN of the same object, including read-only fanout. Filehandle
        // identity also catches hardlink and symlink aliases without another RPC.
        // Keep independent pairs batched; aliases must complete in caller order.
        let aliases = (0..pairs.len()).any(|i| {
            let source = &self.open_files[&src_tmp[i].expect("path source was opened")].fh;
            let destination =
                &self.open_files[&dst_tmp[i].expect("path destination was opened")].fh;
            (i + 1..pairs.len()).any(|j| {
                let other_source =
                    &self.open_files[&src_tmp[j].expect("path source was opened")].fh;
                let other_destination =
                    &self.open_files[&dst_tmp[j].expect("path destination was opened")].fh;
                source == other_source
                    || source == other_destination
                    || destination == other_source
                    || destination == other_destination
            })
        });
        if aliases {
            // Obsolete stateids can fail CLOSE. Attempt every descriptor so
            // one such error cannot skip cleanup of later independent objects.
            for descriptor in src_tmp.iter().chain(&dst_tmp) {
                self.close_tmp(std::slice::from_ref(descriptor));
            }
            for (index, pair) in pairs.iter().enumerate() {
                self.copy_extents_server_side(std::slice::from_ref(pair))
                    .map_err(|mut failure| {
                        failure.error = failure.error.map_index(|_| index);
                        // A previous pair may already have changed its destination.
                        // Never ask the caller to replay that successful prefix.
                        failure.fallback_safe &= index == 0;
                        failure
                    })?;
            }
            return Ok(());
        }

        let result = (|| -> Result<(), CopyBatchFailure> {
            let explicit: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter_map(|(i, pair)| pair.length.is_some_and(|length| length > 0).then_some(i))
                .collect();
            let mut source_attrs: Vec<VfAttrs> = explicit
                .iter()
                .map(|&i| VfAttrs {
                    file: FileOperand::from_fd(src_tmp[i].expect("path source was opened")),
                    masks: AttrMask::SIZE,
                    ..VfAttrs::default()
                })
                .collect();
            if !source_attrs.is_empty() {
                self.vgetattrs_impl(&mut source_attrs)?;
            }
            let mut effective_lengths: Vec<Option<u64>> =
                pairs.iter().map(|pair| pair.length).collect();
            for (&i, attrs) in explicit.iter().zip(&source_attrs) {
                effective_lengths[i] = pairs[i]
                    .length
                    .map(|length| length.min(attrs.size.saturating_sub(pairs[i].src_offset)));
            }
            let mut copies = Vec::with_capacity(pairs.len());
            for (i, p) in pairs.iter().enumerate() {
                let src = self
                    .open_files
                    .get(&src_tmp[i].expect("path source was opened"))
                    .expect("temporary source descriptor");
                let dst = self
                    .open_files
                    .get(&dst_tmp[i].expect("path destination was opened"))
                    .expect("temporary destination descriptor");
                copies.push(crate::client::CopyOp {
                    src_fh: src.fh.clone(),
                    src_stateid: src.stateid,
                    dst_fh: dst.fh.clone(),
                    dst_stateid: dst.stateid,
                    src_offset: p.src_offset,
                    dst_offset: p.dst_offset,
                    count: effective_lengths[i].unwrap_or(0),
                });
            }
            let mut totals = vec![0u64; copies.len()];
            // NFSv4.2 uses count=0 to mean "through EOF", while the VFSI API
            // uses Some(0) for an explicit zero-byte copy. Do not put those
            // operations on the wire; the SETATTR phase below still applies
            // the destination-size semantics.
            let mut pending: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter_map(|(i, _)| (effective_lengths[i] != Some(0)).then_some(i))
                .collect();
            while !pending.is_empty() {
                let active: Vec<crate::client::CopyOp> = pending
                    .iter()
                    .map(|&i| crate::client::CopyOp {
                        src_fh: copies[i].src_fh.clone(),
                        src_stateid: copies[i].src_stateid,
                        dst_fh: copies[i].dst_fh.clone(),
                        dst_stateid: copies[i].dst_stateid,
                        src_offset: copies[i].src_offset,
                        dst_offset: copies[i].dst_offset,
                        count: effective_lengths[i]
                            .map(|length| length.saturating_sub(totals[i]))
                            .unwrap_or(0),
                    })
                    .collect();
                self.server_copy_stats.requests += 1;
                let counts = match self.nfs.copy_many(&active) {
                    Ok(counts) => counts,
                    Err(error) => {
                        let active_index = error.op_index;
                        let original = pending.get(active_index).copied().unwrap_or(0);
                        let fallback_safe = server_copy_failure_allows_fallback(
                            error.status,
                            active_index,
                            &totals,
                        );
                        let error =
                            vfsi_core::error_from_rpc_indexed(error.with_op_index(original));
                        return Err(CopyBatchFailure {
                            error,
                            fallback_safe,
                        });
                    }
                };
                self.server_copy_stats.operations += counts.len() as u64;
                let mut next = Vec::new();
                for (&i, n) in pending.iter().zip(counts) {
                    totals[i] = totals[i]
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    copies[i].src_offset = copies[i]
                        .src_offset
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    copies[i].dst_offset = copies[i]
                        .dst_offset
                        .checked_add(n)
                        .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                    // A zero-byte continuation means EOF. For an explicit
                    // count this matches vcopy_data_impl's short-at-EOF behavior; for a
                    // count of zero it confirms that a possibly partial COPY
                    // has reached EOF.
                    if n != 0
                        && effective_lengths[i]
                            .map(|length| totals[i] < length)
                            .unwrap_or(true)
                    {
                        next.push(i);
                    }
                }
                pending = next;
            }
            let mut attrs = Vec::with_capacity(pairs.len());
            for (i, (&n, p)) in totals.iter().zip(pairs).enumerate() {
                let size = p
                    .dst_offset
                    .checked_add(n)
                    .ok_or_else(|| VfError::failure(i, libc::EOVERFLOW as u32))?;
                attrs.push(crate::client::SetattrOp {
                    fh: copies[i].dst_fh.clone(),
                    mode: None,
                    uid: None,
                    gid: None,
                    size: Some(size),
                    atime: None,
                    mtime: None,
                });
            }
            self.nfs
                .setattr_many(&attrs)
                .map_err(vfsi_core::error_from_rpc_indexed)
                .map_err(CopyBatchFailure::without_fallback)
        })();
        self.close_tmp(&src_tmp);
        self.close_tmp(&dst_tmp);
        result
    }

    pub(super) fn vcopy_data_nfs(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.ensure_writable(pairs.len())?;
        if !self.server_copy_enabled {
            return self.vcopy_data_impl(pairs);
        }
        // Keep the number of simultaneously open source/destination states
        // bounded while still amortizing OPEN, COPY, SETATTR, and CLOSE.
        const FILES_PER_COPY_BATCH: usize = 8;
        for (base, batch) in pairs.chunks(FILES_PER_COPY_BATCH).enumerate() {
            if let Err(failure) = self.copy_extents_server_side(batch) {
                if failure.fallback_safe {
                    self.server_copy_enabled = false;
                    self.server_copy_stats.fallbacks += 1;
                    let start = base * FILES_PER_COPY_BATCH;
                    return self
                        .vcopy_data_impl(&pairs[start..])
                        .map_err(|fallback| fallback.map_index(|index| start + index));
                }
                return Err(failure
                    .error
                    .map_index(|index| base * FILES_PER_COPY_BATCH + index));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                Err::<(), _>(VfError::nfs(
                    0,
                    match create {
                        crate::client::OpenCreate::NoCreate => nfsstat4_NFS4ERR_NOENT,
                        crate::client::OpenCreate::Guarded => nfsstat4_NFS4ERR_EXIST,
                        _ => unreachable!(),
                    },
                ))
            },
            |client| client.source_closed = true,
        )
        .unwrap_err();

        assert_eq!(error.err_no(), nfsstat4_NFS4ERR_EXIST);
        assert_eq!(
            client.attempts,
            [
                crate::client::OpenCreate::NoCreate,
                crate::client::OpenCreate::Guarded
            ]
        );
        assert!(client.source_closed);
    }

    #[test]
    fn server_copy_fallback_requires_no_completed_prefix() {
        assert!(server_copy_failure_allows_fallback(
            nfsstat4_NFS4ERR_OFFLOAD_DENIED,
            0,
            &[0, 0, 0],
        ));
        assert!(!server_copy_failure_allows_fallback(
            nfsstat4_NFS4ERR_OFFLOAD_DENIED,
            2,
            &[0, 0, 0],
        ));
        assert!(!server_copy_failure_allows_fallback(
            nfsstat4_NFS4ERR_OFFLOAD_DENIED,
            0,
            &[8, 0, 0],
        ));
        assert!(!server_copy_failure_allows_fallback(
            nfsstat4_NFS4ERR_ACCESS,
            0,
            &[0, 0, 0],
        ));
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

        assert_eq!(error.err_no(), ERR_IO);
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

        assert_eq!(error.err_no(), ERR_IO);
        assert_eq!(writes, 1);
    }
}
