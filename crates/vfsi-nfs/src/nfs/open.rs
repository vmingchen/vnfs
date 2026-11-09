//! Open state acquisition and deferred close ownership.

use super::*;

pub(super) fn non_destructive_reopen_flags(flags: i32) -> i32 {
    flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_TRUNC)
}

pub(super) fn open_flags_mutate(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY
        || flags & (libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND) != 0
}

#[cfg(test)]
trait PathBatchTruncateClient {
    fn setattr_many(&mut self, ops: &[crate::client::SetattrOp]) -> VfResult<()>;
    fn close_many_path(&mut self, ops: &[crate::client::CloseOp]) -> vfsi_core::RpcResult<()>;
}

#[cfg(test)]
impl PathBatchTruncateClient for crate::client::NfsClient {
    fn setattr_many(&mut self, ops: &[crate::client::SetattrOp]) -> VfResult<()> {
        vfsi_core::internal::collect_vec_result(
            crate::client::NfsClient::setattr_many(self, ops),
            ops.len(),
        )
        .map(|_| ())
    }

    fn close_many_path(&mut self, ops: &[crate::client::CloseOp]) -> vfsi_core::RpcResult<()> {
        crate::client::NfsClient::close_many_path(self, ops)
    }
}

#[cfg(test)]
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
        let fd = vfsi_core::insert_fd(&mut self.next_fd, &mut self.open_files, open)?;
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
    ) -> vfsi_core::VecResult<FileOperand> {
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
        let outcomes = self.nfs.openv_path_compound(&ops)?;
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::AfterReply { chunk: 0 }) {
            let mut closes = Vec::new();
            let results = outcomes
                .into_iter()
                .map(|outcome| {
                    match outcome {
                        Ok((fh, stateid)) => closes.push(crate::client::CloseOp { fh, stateid }),
                        Err(error) if error.state() == vfsi_core::FailureState::NotAttempted => {
                            return Err(error);
                        }
                        Err(_) => {}
                    }
                    Err(vfsi_core::OpError::unknown(error.clone(), None))
                })
                .collect();
            self.nfs.close_path_or_defer(closes);
            return Ok(results);
        }
        let mut results = Vec::with_capacity(paths.len());
        for (index, outcome) in outcomes.into_iter().enumerate() {
            let (fh, stateid) = match outcome {
                Ok(open) => open,
                Err(error) => {
                    results.push(Err(error));
                    continue;
                }
            };
            #[cfg(feature = "test-faults")]
            if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeRegister { index }) {
                self.nfs
                    .close_path_or_defer(vec![crate::client::CloseOp { fh, stateid }]);
                results.push(Err(vfsi_core::OpError::failed(error, None)));
                continue;
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
                        results.push(Err(vfsi_core::OpError::failed(error, None)));
                        continue;
                    }
                    results.push(Ok(file));
                }
                Err(error) => {
                    self.nfs
                        .close_path_or_defer(vec![crate::client::CloseOp { fh, stateid }]);
                    results.push(Err(vfsi_core::OpError::failed(error, None)));
                }
            }
        }
        Ok(results)
    }

    /// Retire temporary public descriptors; retain every unconfirmed owner.
    pub(super) fn close_temporary_descriptors(
        &mut self,
        files: Vec<FileOperand>,
    ) -> vfsi_core::VecResult<()> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let result = self.vclose_impl(&files);
        for file in files {
            if let Some(fd) = file.fd()
                && let Some(open) = self.open_files.remove(&fd)
            {
                self.deferred_descriptor_closes.push(open);
            }
        }
        result
    }

    /// Best-effort close of stateids opened by a merged path compound.
    pub(super) fn close_path_opens(&mut self, opens: &[(NfsFh, stateid4)]) {
        if opens.is_empty() {
            return;
        }
        let ops = crate::client::latest_path_open_closes(opens);
        self.nfs.close_path_or_defer(ops);
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
        closed: Vec<(Vec<u8>, u32, [std::ffi::c_char; 12])>,
    }

    impl PathBatchTruncateClient for FailingTruncateClient {
        fn setattr_many(&mut self, ops: &[SetattrOp]) -> VfResult<()> {
            self.setattr_calls.set(self.setattr_calls.get() + 1);
            assert_eq!(ops.len(), 1);
            Err(VfError::nfs(0, nfsstat4_NFS4ERR_ACCESS))
        }

        fn close_many_path(&mut self, ops: &[CloseOp]) -> vfsi_core::RpcResult<()> {
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
        assert_eq!(error.raw_code(), nfsstat4_NFS4ERR_ACCESS);
        assert_eq!(
            client.closed,
            vec![(vec![1, 2], 3, [4; 12]), (vec![5], 6, [7; 12])]
        );
    }
}

#[cfg(all(test, feature = "test-faults"))]
#[path = "prr_temporary_open_tests.rs"]
mod prr_temporary_open_tests;
