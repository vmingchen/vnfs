//! Client-side and server-side extent copying.

use super::*;

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
        let (dfh, dsid) = match self
            .nfs
            .open_path(
                &ddirfh,
                &dname,
                OPEN4_SHARE_ACCESS_WRITE,
                crate::client::OpenCreate::NoCreate,
            )
            .map_err(|e| vfsi_core::error_from_rpc(e, 0))
        {
            Ok(x) => x,
            Err(e) if e.err_no() == ERR_NOENT => self
                .nfs
                .open_path(
                    &ddirfh,
                    &dname,
                    OPEN4_SHARE_ACCESS_WRITE,
                    crate::client::OpenCreate::Guarded,
                )
                .map_err(|e| vfsi_core::error_from_rpc(e, 0))?,
            Err(e) => {
                let _ = self.nfs.close_path(&sfh, &ssid);
                return Err(e);
            }
        };

        let mut so = p.src_offset;
        let mut doff = p.dst_offset;
        let mut copied: u64 = 0;
        let result = loop {
            if let Some(length) = p.length
                && copied >= length
            {
                break Ok(());
            }
            let remaining = p.length.map_or(u64::MAX, |l| l - copied);
            let chunk_len = remaining.min(1 << 20) as u32;
            let chunk = match self.nfs.read(&sfh, &ssid, so, chunk_len) {
                Ok((c, _)) => c,
                Err(e) => break Err(vfsi_core::error_from_rpc(e, 0)),
            };
            if chunk.is_empty() {
                break Ok(()); // EOF
            }
            let n = match self.nfs.write(&dfh, &dsid, doff, &chunk) {
                Ok((n, _)) => n as u64,
                Err(e) => break Err(vfsi_core::error_from_rpc(e, 0)),
            };
            so = so
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
            doff = doff
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
            copied = copied
                .checked_add(n)
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?;
        };
        let result = if result.is_ok() {
            // Truncate any stale tail beyond what was copied (cp semantics).
            self.nfs
                .setattr(&dfh, None, Some(doff))
                .map_err(|e| vfsi_core::error_from_rpc(e, 0))
        } else {
            result
        };
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

    fn copy_extents_server_side(&mut self, pairs: &[ExtentPair]) -> VfRes {
        let src_files: Vec<VfFile> = pairs
            .iter()
            .map(|p| VfFile::from_os_path(&p.src_path))
            .collect();
        let dst_files: Vec<VfFile> = pairs
            .iter()
            .map(|p| VfFile::from_os_path(&p.dst_path))
            .collect();
        let src_refs: Vec<&VfFile> = src_files.iter().collect();
        let dst_refs: Vec<&VfFile> = dst_files.iter().collect();
        let no_create = vec![false; pairs.len()];
        let create = vec![true; pairs.len()];
        let no_truncate = vec![false; pairs.len()];

        let src_tmp = self.open_path_batch(&src_refs, &no_create, false, &no_truncate)?;
        let dst_tmp = match self.open_path_batch(&dst_refs, &create, true, &no_truncate) {
            Ok(tmp) => tmp,
            Err(e) => {
                self.close_tmp(&src_tmp);
                return Err(e);
            }
        };

        let result = (|| {
            let explicit: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter_map(|(i, pair)| pair.length.is_some_and(|length| length > 0).then_some(i))
                .collect();
            let mut source_attrs: Vec<VfAttrs> = explicit
                .iter()
                .map(|&i| VfAttrs {
                    file: VfFile::from_fd(src_tmp[i].expect("path source was opened")),
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
                let counts = self.nfs.copy_many(&active).map_err(|e| {
                    let original = pending.get(e.op_index).copied().unwrap_or(0);
                    vfsi_core::error_from_rpc_indexed(e.with_op_index(original))
                })?;
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
            if let Err(e) = self.copy_extents_server_side(batch) {
                if matches!(
                    e.err_no(),
                    nfsstat4_NFS4ERR_NOTSUPP
                        | nfsstat4_NFS4ERR_OP_ILLEGAL
                        | nfsstat4_NFS4ERR_OFFLOAD_DENIED
                        | nfsstat4_NFS4ERR_OFFLOAD_NO_REQS
                        | nfsstat4_NFS4ERR_STALE_STATEID
                        | nfsstat4_NFS4ERR_OLD_STATEID
                        | nfsstat4_NFS4ERR_BAD_STATEID
                ) {
                    self.server_copy_enabled = false;
                    self.server_copy_stats.fallbacks += 1;
                    let start = base * FILES_PER_COPY_BATCH;
                    return self
                        .vcopy_data_impl(&pairs[start..])
                        .map_err(|fallback| fallback.map_index(|index| start + index));
                }
                return Err(e.map_index(|index| base * FILES_PER_COPY_BATCH + index));
            }
        }
        Ok(())
    }
}
