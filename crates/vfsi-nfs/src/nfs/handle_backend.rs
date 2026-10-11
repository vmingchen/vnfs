//! HandleBackend implementation for retained NFS resources.

use super::*;

impl HandleBackend for NfsVecFs {
    fn vstatfs_impl(&mut self, files: &[VfFile]) -> VfResult<Vec<FilesystemStats>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| HandleBackend::vstatfs_impl(client, files));
        }
        let refs: Vec<_> = files.iter().collect();
        let resolved = self.resolve_files_nfs(&refs, true)?;
        let mut ops = Vec::with_capacity(files.len());
        for (index, result) in resolved.into_iter().enumerate() {
            let (fh, _) = result.map_err(|status| VfError::nfs(index, status))?;
            ops.push(crate::client::GetattrOp {
                fh,
                attrs: filesystem_attributes(),
            });
        }
        self.nfs
            .getattr_many_with_bitmap(&ops)
            .map_err(vfsi_core::error_from_rpc_indexed)?
            .into_iter()
            .enumerate()
            .map(|(index, (bitmap, bytes))| {
                let mut stats =
                    decode_filesystem_stats(bitmap, &bytes).map_err(|e| e.with_index(index))?;
                // Client mount policy is known; the protocol has no filesystem read-only attribute.
                if self.read_only {
                    stats.read_only = Some(true);
                }
                Ok(stats)
            })
            .collect()
    }

    fn close_deferred(&mut self, file: &VfFile) -> VfResult<()> {
        let fd = file.fd().ok_or_else(|| VfError::client(0, ERR_INVAL))?;
        if self.open_files.contains_key(&fd) {
            self.close_impl(file)
        } else {
            // A failed scalar CLOSE already transferred the remote state into
            // the backend queue and disarmed this descriptor for ordinary I/O.
            self.drain_deferred_descriptor_closes()
        }
    }

    fn take_notifications(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        let Some(observer) = &self.observer else {
            return Vec::new();
        };
        let observer = observer.clone();
        let events = std::mem::take(&mut self.pending_events);
        if events.is_empty() {
            return Vec::new();
        }
        events
            .into_iter()
            .map(|event| {
                let observer = observer.clone();
                Box::new(move || observer.on_event(&event)) as Box<dyn FnOnce() + Send>
            })
            .collect()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::UNIX_SEMANTICS
            | if self.server_copy_enabled() {
                Capabilities::SERVER_COPY
            } else {
                Capabilities::empty()
            }
    }

    /// Namespace-relative path (no leading `/`), per the [`HandleBackend::abs_path`]
    /// contract. The export root (`connection.root`) is not included; use
    /// `NfsVecFs::server_path` for the path sent to the NFS server.
    fn abs_path(&self, path: &Path) -> PathBuf {
        namespace_path(&self.cwd, path)
    }

    fn open_path_impl(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        mode: u32,
    ) -> VfResult<VfFile> {
        use libc::{O_APPEND, O_CREAT, O_EXCL, O_TRUNC};
        if open_flags_mutate(flags) {
            self.ensure_writable(1)?;
        }
        let full = match base {
            VfPathBase::Abs => self.server_path(&Path::new("/").join(pathname)),
            VfPathBase::Cwd => self.server_path(pathname),
        };
        let full = path_from_bytes(&normalize_bytes(path_bytes(&full)));
        // Follow a final symlink chain so O_CREAT creates the target
        // (POSIX semantics); OPEN cannot target a symlink directly.
        let full = self.follow_target_path(&full)?;
        let (dir, name) =
            split_path_bytes(path_bytes(&full)).map_err(|_| VfError::failure(0, ERR_NOENT))?;
        let access = Self::flags_to_access(flags);
        let create = flags & O_CREAT != 0;
        let excl = flags & O_EXCL != 0;
        // The mode only applies when O_CREAT actually creates the file
        // (POSIX ignores it for existing files).
        let created = if create {
            if excl {
                true
            } else {
                match self.nfs.resolve(path_bytes(&full)) {
                    Ok(_) => false,
                    Err(e) if e.status == nfsstat4_NFS4ERR_NOENT => true,
                    Err(e) => return Err(vfsi_core::error_from_rpc(e, 0)),
                }
            }
        } else {
            false
        };
        // Open with NoCreate when the file already exists (kernel nfsd
        // rejects CREATE_GUARDED on existing files with NFS4ERR_EXIST).
        let (fh, stateid) = self.open_impl(&path_from_bytes(&dir), &name, access, created, excl)?;
        let setup = (|| {
            if created {
                self.nfs
                    .setattr(&fh, Some(mode & 0o7777), None)
                    .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
            }
            if flags & O_TRUNC != 0 {
                self.nfs
                    .setattr(&fh, None, Some(0))
                    .map_err(|e| vfsi_core::error_from_rpc(e, 0))?;
            }
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeRegister { index: 0 })?;
            Ok(())
        })();
        if let Err(error) = setup {
            let _ = self.nfs.close_unregistered(&fh, &stateid);
            return Err(error);
        }
        let open = OpenFile {
            fh: fh.clone(),
            stateid,
            cur_offset: 0,
            append: flags & O_APPEND != 0,
            reopen: Some(ReopenFile {
                path: self.visible_path(&full),
                flags: non_destructive_reopen_flags(flags),
                mode,
            }),
        };
        match self.insert_open_file(open) {
            Ok(fd) => Ok(VfFile::from_fd(fd)),
            Err(error) => {
                let _ = self.nfs.close_unregistered(&fh, &stateid);
                Err(error)
            }
        }
    }

    fn close_impl(&mut self, tcf: &VfFile) -> VfResult<()> {
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let fd = tcf.fd().unwrap();
        let open = self
            .open_files
            .remove(&fd)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeCloseDispatch { index: 0 })
        {
            self.deferred_descriptor_closes.push(open);
            return Err(error);
        }
        match self.nfs.close(&open.fh, &open.stateid) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.deferred_descriptor_closes.push(open);
                Err(vfsi_core::error_from_rpc(error, 0))
            }
        }
    }

    fn vfsync_impl(&mut self, files: &[VfFile], _mode: vfsi_core::api::SyncMode) -> VfRes {
        for (index, file) in files.iter().enumerate() {
            self.sync_data(file)
                .map_err(|error| error.map_index(|_| index))?;
        }
        Ok(())
    }

    fn sync_data(&mut self, tcf: &VfFile) -> VfResult<()> {
        let fd = tcf.fd().ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        if self.open_files.contains_key(&fd) {
            // All writes request NFS FILE_SYNC4 stability. Validate the
            // descriptor so flush cannot incorrectly succeed after close.
            Ok(())
        } else {
            Err(VfError::failure(0, ERR_EBADF))
        }
    }

    fn chdir(&mut self, path: &Path) -> VfResult<()> {
        let st = self.stat_impl(path)?;
        if st.ftype != VfType::Directory {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        self.cwd = self.abs_path(path);
        Ok(())
    }

    fn getcwd(&self) -> PathBuf {
        Path::new("/").join(&self.cwd)
    }

    fn seek_raw_impl(&mut self, tcf: &VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64> {
        if !self.recovery_in_progress {
            return self.read_with_recovery(|client| client.seek_raw_impl(tcf, offset, whence));
        }
        if !tcf.is_descriptor() {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let cur = self
            .open_files
            .get(&tcf.fd().unwrap())
            .map(|o| o.cur_offset)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        let new = match whence {
            SeekFrom::Set => offset,
            SeekFrom::Cur => i64::try_from(cur)
                .ok()
                .and_then(|cur| cur.checked_add(offset))
                .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?,
            SeekFrom::End => {
                let fh = self
                    .open_files
                    .get(&tcf.fd().unwrap())
                    .map(|o| o.fh.clone())
                    .unwrap();
                let size = self.file_size(&fh)?;
                i64::try_from(size)
                    .ok()
                    .and_then(|size| size.checked_add(offset))
                    .ok_or_else(|| VfError::failure(0, libc::EOVERFLOW as u32))?
            }
            _ => return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        };
        if new < 0 {
            return Err(VfError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let new = new as u64;
        self.advance_offset(tcf, new);
        Ok(new as i64)
    }
    fn read_file_impl(&mut self, file: &VfFile, max_bytes: usize) -> VfResult<Vec<u8>> {
        vfsi_sync::backend::helpers::native_read_file_impl_default(self, file, max_bytes)
    }
    fn open_impl(&mut self, request: &OpenOp) -> VfResult<VfFile> {
        vfsi_sync::backend::helpers::native_open_impl_default(self, request)
    }
    fn read_impl(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        vfsi_sync::backend::helpers::native_read_impl_default(self, request)
    }
    fn read_into_impl(&mut self, request: &ReadOp, buffer: &mut [u8]) -> VfResult<ReadIntoResult> {
        vfsi_sync::backend::helpers::native_read_into_impl_default(self, request, buffer)
    }
    fn write_impl(&mut self, request: WriteOp<&VfFile, &[u8]>) -> VfResult<WriteResult> {
        vfsi_sync::backend::helpers::native_write_impl_default(self, request)
    }
    fn seek_impl(&mut self, file: &VfFile, position: std::io::SeekFrom) -> VfResult<u64> {
        vfsi_sync::backend::helpers::native_seek_impl_default(self, file, position)
    }
    fn metadata_impl(
        &mut self,
        target: Target<'_, VfFile>,
        options: vfsi_core::api::AttrsOptions,
    ) -> VfResult<VfAttrs> {
        vfsi_sync::backend::helpers::native_metadata_impl_default(self, target, options)
    }
    fn set_attributes_impl(&mut self, update: &SetAttrsOp<Target<'_, VfFile>>) -> VfResult<()> {
        vfsi_sync::backend::helpers::native_set_attributes_impl_default(self, update)
    }
    fn vsetattrs_impl(&mut self, updates: &[SetAttrsOp<Target<'_, VfFile>>]) -> VfResult<()> {
        vfsi_sync::backend::helpers::vsetattrs_typed_default(self, updates)
    }
}
