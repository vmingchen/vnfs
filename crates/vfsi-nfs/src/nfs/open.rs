//! Open state acquisition and deferred close ownership.

use super::*;

pub(super) fn non_destructive_reopen_flags(flags: i32) -> i32 {
    flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC)
}

pub(super) fn open_flags_mutate(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY
        || flags & (libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND) != 0
}

trait PathBatchTruncateClient {
    fn setattr_many(&mut self, ops: &[crate::client::SetattrOp]) -> crate::error::RpcResult<()>;
    fn close_many_path(&mut self, ops: &[crate::client::CloseOp]) -> crate::error::RpcResult<()>;
}

impl PathBatchTruncateClient for crate::client::NfsClient {
    fn setattr_many(&mut self, ops: &[crate::client::SetattrOp]) -> crate::error::RpcResult<()> {
        crate::client::NfsClient::setattr_many(self, ops)
    }

    fn close_many_path(&mut self, ops: &[crate::client::CloseOp]) -> crate::error::RpcResult<()> {
        crate::client::NfsClient::close_many_path(self, ops)
    }
}

fn truncate_path_opens<C: PathBatchTruncateClient>(
    client: &mut C,
    opened: &[(NfsFh, stateid4)],
    setattr_ops: &[crate::client::SetattrOp],
    subset: &[usize],
) -> VfResult<()> {
    crate::client::prepare_path_opens(
        opened,
        client,
        |client| {
            client.setattr_many(setattr_ops).map_err(|error| {
                let error = vfsi_core::error_from_rpc_indexed(error);
                error.map_index(|relative| subset.get(relative).copied().unwrap_or(relative))
            })
        },
        |client, closes| {
            let _ = client.close_many_path(closes);
        },
    )
}

impl NfsVecFs {
    // -- private helpers ----------------------------------------------------

    pub(super) fn insert_open_file(&mut self, open: NfsOpenState) -> VfResult<i32> {
        let fd = crate::vecfs::insert_fd(&mut self.next_fd, &mut self.open_files, open)?;
        self.open_object_ids.remove(&fd);
        Ok(fd)
    }

    pub(super) fn drain_deferred_descriptor_closes(&mut self) -> VfResult<()> {
        if self.deferred_descriptor_closes.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.deferred_descriptor_closes);
        let operations: Vec<crate::client::CloseOp> = pending
            .iter()
            .map(|open| crate::client::CloseOp {
                fh: open.fh.clone(),
                stateid: open.stateid,
            })
            .collect();
        match self.nfs.close_many(&operations) {
            Ok(()) => Ok(()),
            Err(error) => {
                let first_unconfirmed = if error.is_transport() {
                    0
                } else {
                    error.op_index.min(pending.len())
                };
                self.deferred_descriptor_closes
                    .extend(pending.into_iter().skip(first_unconfirmed));
                Err(vfsi_core::error_from_rpc(error, None))
            }
        }
    }

    /// Map fcntl-style flags to an NFSv4 share access mode.
    pub(super) fn flags_to_access(flags: i32) -> u32 {
        use libc::{O_RDWR, O_WRONLY};
        if flags & O_RDWR != 0 {
            OPEN4_SHARE_ACCESS_BOTH
        } else if flags & O_WRONLY != 0 {
            OPEN4_SHARE_ACCESS_WRITE
        } else {
            OPEN4_SHARE_ACCESS_READ
        }
    }

    pub(super) fn open_impl(
        &mut self,
        dir: &Path,
        name: &[u8],
        access: u32,
        create: bool,
        excl: bool,
        mode: u32,
    ) -> VfResult<(NfsFh, stateid4)> {
        let dirfh = self.resolve_path(dir, true).map_err(|e| e.with_index(0))?;
        let create_mode = match (create, excl) {
            (false, _) => OpenCreate::NoCreate,
            (true, true) => OpenCreate::Exclusive,
            (true, false) => OpenCreate::Guarded,
        };
        let result = if create && excl {
            self.nfs
                .open_with_mode(&dirfh, name, access, create_mode, mode)
        } else {
            self.nfs.open(&dirfh, name, access, create_mode)
        };
        result.map_err(|error| {
            // NFS servers may reject OPEN on an existing symlink with
            // NFS4ERR_SYMLINK even for exclusive creation. The parent was
            // already followed above, so this status identifies the existing
            // final entry and has POSIX exclusive-create semantics: EEXIST.
            if create && excl && error.status == nfsstat4_NFS4ERR_SYMLINK {
                VfError::nfs(0, nfsstat4_NFS4ERR_EXIST)
            } else {
                vfsi_core::error_from_rpc(error, 0)
            }
        })
    }

    /// The merged (single-compound) openv: resolve each parent once, then
    /// OPEN + GETFH per file. UNCHECKED creates carry the mode and the
    /// size=0 `O_TRUNC` in their createattrs, so no existence probe or
    /// separate SETATTR is needed (RFC 8881 §18.16.3).
    pub(super) fn openv_merged(
        &mut self,
        paths: &[&Path],
        flags: &[i32],
        modes: &[u32],
    ) -> VfResult<ManyResults<FileOperand>> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        let mut ops = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let create = if flags[i] & O_CREAT != 0 {
                if flags[i] & O_EXCL != 0 {
                    crate::client::OpenCreate::Exclusive
                } else {
                    crate::client::OpenCreate::Unchecked
                }
            } else {
                crate::client::OpenCreate::NoCreate
            };
            ops.push(crate::client::PathOpenOp {
                path: path_bytes(&self.server_path(p)).to_vec(),
                access: Self::flags_to_access(flags[i]),
                create,
                mode: Some(modes[i] & 0o7777),
                truncate: flags[i] & O_TRUNC != 0,
            });
        }
        #[cfg(feature = "test-faults")]
        self.inject_open_fault(OpenFaultPoint::BeforeDispatch { chunk: 0 })?;
        let mut outcome = self
            .nfs
            .openv_path_compound(&ops)
            .map_err(|e| vfsi_core::error_from_rpc(e, None))?;
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterReply { chunk: 0 }) {
            let closes: Vec<crate::client::CloseOp> = outcome
                .opened
                .iter_mut()
                .filter_map(Option::take)
                .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                .collect();
            if !closes.is_empty() {
                let _ = self.nfs.close_unregistered_many(&closes);
            }
            return Err(error);
        }
        let failed = outcome.failed;
        let mut results = Vec::with_capacity(paths.len());
        for index in 0..paths.len() {
            let Some((fh, stateid)) = outcome.opened[index].take() else {
                if let Some((failed_index, status)) = failed {
                    debug_assert_eq!(index, failed_index);
                    results.push(Err(vfsi_core::error_from_rpc(
                        RpcError::op(index, status),
                        Some(index),
                    )));
                }
                break;
            };
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeRegister { index }) {
                let mut closes = vec![crate::client::CloseOp { fh, stateid }];
                closes.extend(outcome.opened[index + 1..].iter_mut().filter_map(|opened| {
                    opened
                        .take()
                        .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                }));
                let _ = self.nfs.close_unregistered_many(&closes);
                results.push(Err(error));
                break;
            }
            let open = NfsOpenState {
                fh: fh.clone(),
                stateid,
                cur_offset: 0,
                append: flags[index] & O_APPEND != 0,
                reopen: Some(ReopenFile {
                    path: self.visible_path(&self.server_path(paths[index])),
                    flags: non_destructive_reopen_flags(flags[index]),
                }),
            };
            match self.insert_open_file(open) {
                Ok(fd) => {
                    let file = FileOperand::from_fd(fd);
                    #[cfg(feature = "test-faults")]
                    if let Err(error) =
                        self.inject_open_fault(OpenFaultPoint::AfterRegister { index })
                    {
                        let _ = self.close_impl(&file);
                        let closes: Vec<crate::client::CloseOp> = outcome.opened[index + 1..]
                            .iter_mut()
                            .filter_map(Option::take)
                            .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                            .collect();
                        if !closes.is_empty() {
                            let _ = self.nfs.close_unregistered_many(&closes);
                        }
                        results.push(Err(error));
                        break;
                    }
                    results.push(Ok(file));
                }
                Err(error) => {
                    let mut closes = vec![crate::client::CloseOp { fh, stateid }];
                    closes.extend(outcome.opened[index + 1..].iter_mut().filter_map(|opened| {
                        opened
                            .take()
                            .map(|(fh, stateid)| crate::client::CloseOp { fh, stateid })
                    }));
                    let _ = self.nfs.close_unregistered_many(&closes);
                    results.push(Err(error));
                    break;
                }
            }
        }
        Ok(ManyResults::new(paths.len(), results))
    }

    /// Open every path-based file in `files` in one batched OPEN compound,
    /// returning, per original index, the temporary descriptor (or `None` for
    /// inputs that already were descriptors). The caller must close them via
    /// [`close_tmp`](Self::close_tmp). Errors are attributed to the original
    /// op index.
    pub(super) fn open_path_batch(
        &mut self,
        files: &[&FileOperand],
        creation: &[bool],
        for_write: bool,
        truncate: &[bool],
    ) -> VfResult<Vec<Option<i32>>> {
        // Resolve each parent directory once per distinct dir, then look up
        // every final component in one tolerant batch (which also reports the
        // type, so symlinks can be followed only when actually present).
        let mut dir_cache: std::collections::HashMap<Box<[u8]>, NfsFh> =
            std::collections::HashMap::new();
        let mut lookups: Vec<(usize, NfsFh, Vec<u8>)> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            if f.is_fd() {
                continue;
            }
            match f {
                FileOperand::Cwd => return Err(VfError::failure(i, ERR_ISDIR)),
                FileOperand::Saved => return Err(VfError::failure(i, ERR_NOENT)),
                FileOperand::Path { .. } | FileOperand::CwdPath(_) => {}
                FileOperand::Fd(_) => unreachable!(),
                _ => return Err(VfError::unsupported(i)),
            }
            let full = self.server_vf_path(f).map_err(|e| e.with_index(i))?;
            let (dir, name) =
                split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(i, ERR_NOENT))?;
            let dirfh = match dir_cache.get(dir.as_slice()) {
                Some(fh) => fh.clone(),
                None => {
                    let fh = self
                        .resolve_path(&path_from_bytes(&dir), true)
                        .map_err(|e| e.with_index(i))?;
                    dir_cache.insert(dir.into_boxed_slice(), fh.clone());
                    fh
                }
            };
            lookups.push((i, dirfh, name));
        }
        if lookups.is_empty() {
            return Ok(vec![None; files.len()]);
        }
        let probe: Vec<(NfsFh, Vec<u8>)> = lookups
            .iter()
            .map(|(_, dir, name)| (dir.clone(), name.clone()))
            .collect();
        let results = self
            .nfs
            .lookup_getattr_many(&probe)
            .map_err(|e| vfsi_core::error_from_rpc(e, None))?;
        let access = if for_write {
            OPEN4_SHARE_ACCESS_BOTH
        } else {
            OPEN4_SHARE_ACCESS_READ
        };
        let mut opens: Vec<(usize, crate::client::OpenOp)> = Vec::new();
        let mut subset: Vec<usize> = Vec::new();
        for ((orig, dirfh, name), r) in lookups.iter().zip(results) {
            match r {
                Ok((_fh, ftype)) if ftype == nfs_ftype4_NF4LNK => {
                    // OPEN cannot target a symlink: follow the chain (creation
                    // follows dangling links and creates the target).
                    let full = &self.server_vf_path(files[*orig])?;
                    let full = self
                        .follow_target_path(full)
                        .map_err(|e| e.with_index(*orig))?;
                    let (dir, name2) = split_path_bytes(path_bytes(&full))
                        .map_err(|_| VfError::failure(*orig, ERR_NOENT))?;
                    let dirfh2 = self
                        .resolve_path(&path_from_bytes(&dir), true)
                        .map_err(|e| e.with_index(*orig))?;
                    let create = match self.nfs.lookup_getattr(&dirfh2, &name2) {
                        Ok(_) => crate::client::OpenCreate::NoCreate,
                        Err(e) if e.status == nfsstat4_NFS4ERR_NOENT => {
                            if !creation[*orig] {
                                return Err(VfError::failure(*orig, ERR_NOENT));
                            }
                            crate::client::OpenCreate::Guarded
                        }
                        Err(e) => return Err(vfsi_core::error_from_rpc(e, *orig)),
                    };
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh2,
                            name: name2,
                            access,
                            create,
                        },
                    ));
                    subset.push(*orig);
                }
                Ok(_) => {
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh.clone(),
                            name: name.clone(),
                            access,
                            create: crate::client::OpenCreate::NoCreate,
                        },
                    ));
                    subset.push(*orig);
                }
                Err(status) if status == nfsstat4_NFS4ERR_NOENT => {
                    if !creation[*orig] {
                        return Err(VfError::failure(*orig, ERR_NOENT));
                    }
                    opens.push((
                        *orig,
                        crate::client::OpenOp {
                            dir: dirfh.clone(),
                            name: name.clone(),
                            access,
                            create: crate::client::OpenCreate::Guarded,
                        },
                    ));
                    subset.push(*orig);
                }
                Err(status) => return Err(VfError::nfs(*orig, status)),
            }
        }
        if opens.is_empty() {
            return Ok(vec![None; files.len()]);
        }
        let open_ops: Vec<crate::client::OpenOp> = opens
            .iter()
            .map(|(_, op)| crate::client::OpenOp {
                dir: op.dir.clone(),
                name: op.name.clone(),
                access: op.access,
                create: op.create,
            })
            .collect();
        let results = self.nfs.open_many_path(&open_ops).map_err(|e| {
            let e = vfsi_core::error_from_rpc_indexed(e);
            // open_many's index is relative to the path-only subset.
            e.map_index(|relative| subset.get(relative).copied().unwrap_or(relative))
        })?;
        // Apply O_TRUNC semantics in this phased fallback.
        let mut setattr_ops = Vec::new();
        for (&orig, (fh, _)) in subset.iter().zip(results.iter()) {
            if truncate.get(orig).copied().unwrap_or(false) {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: None,
                    uid: None,
                    gid: None,
                    size: Some(0),
                    atime: None,
                    mtime: None,
                });
            }
        }
        if !setattr_ops.is_empty() {
            truncate_path_opens(&mut self.nfs, &results, &setattr_ops, &subset)?;
        }
        let mut tmp = vec![None; files.len()];
        for (orig, (fh, stateid)) in subset.iter().zip(results) {
            let fd = self.insert_open_file(NfsOpenState {
                fh,
                stateid,
                cur_offset: 0,
                append: false,
                reopen: None,
            })?;
            tmp[*orig] = Some(fd);
        }
        Ok(tmp)
    }

    /// Close and forget temporary descriptors opened by
    /// [`open_path_batch`](Self::open_path_batch). Best-effort: never fails
    /// the caller (the close failure would mask the real error).
    pub(super) fn close_tmp(&mut self, tmp: &[Option<i32>]) {
        let closes: Vec<crate::client::CloseOp> = tmp
            .iter()
            .filter_map(|fd| {
                let fd = (*fd)?;
                self.open_files.remove(&fd).map(|o| crate::client::CloseOp {
                    fh: o.fh,
                    stateid: o.stateid,
                })
            })
            .collect();
        if !closes.is_empty() {
            let _ = self.nfs.close_many_path(&closes);
        }
    }

    /// Best-effort close of stateids opened by a merged path compound.
    pub(super) fn close_path_opens(&mut self, opens: &[(NfsFh, stateid4)]) {
        if opens.is_empty() {
            return;
        }
        let ops = crate::client::latest_path_open_closes(opens);
        let _ = self.nfs.close_many_path(&ops);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{CloseOp, NfsFh, SetattrOp};
    use nfsv41_sys::{nfsstat4_NFS4ERR_ACCESS, stateid4};
    use std::cell::Cell;

    struct FailingTruncateClient {
        setattr_calls: Cell<usize>,
        closed: Vec<(Vec<u8>, u32, [u8; 12])>,
    }

    impl PathBatchTruncateClient for FailingTruncateClient {
        fn setattr_many(&mut self, ops: &[SetattrOp]) -> crate::error::RpcResult<()> {
            self.setattr_calls.set(self.setattr_calls.get() + 1);
            assert_eq!(ops.len(), 1);
            Err(crate::error::RpcError::op(0, nfsstat4_NFS4ERR_ACCESS))
        }

        fn close_many_path(&mut self, ops: &[CloseOp]) -> crate::error::RpcResult<()> {
            self.closed.extend(ops.iter().map(|op| {
                (
                    op.fh.as_bytes().to_vec(),
                    op.stateid.seqid,
                    op.stateid.other,
                )
            }));
            Ok(())
        }
    }

    #[test]
    fn truncate_setattr_failure_closes_open_path_state_and_keeps_mapped_error() {
        let opened = vec![
            (
                NfsFh::test_from_bytes([1u8, 2]),
                stateid4 {
                    seqid: 3,
                    other: [4; 12],
                },
            ),
            (
                NfsFh::test_from_bytes([5u8]),
                stateid4 {
                    seqid: 6,
                    other: [7; 12],
                },
            ),
        ];
        let setattr_ops = [SetattrOp {
            fh: opened[0].0.clone(),
            mode: None,
            uid: None,
            gid: None,
            size: Some(0),
            atime: None,
            mtime: None,
        }];
        let mut client = FailingTruncateClient {
            setattr_calls: Cell::new(0),
            closed: Vec::new(),
        };

        let error = truncate_path_opens(&mut client, &opened, &setattr_ops, &[8, 13]).unwrap_err();

        assert_eq!(client.setattr_calls.get(), 1);
        assert_eq!(error.index(), Some(8));
        assert_eq!(error.err_no(), nfsstat4_NFS4ERR_ACCESS);
        assert_eq!(
            client.closed,
            vec![(vec![1, 2], 3, [4; 12]), (vec![5], 6, [7; 12])]
        );
    }
}

#[cfg(all(test, feature = "test-faults"))]
#[path = "prr_temporary_open_tests.rs"]
mod prr_temporary_open_tests;
