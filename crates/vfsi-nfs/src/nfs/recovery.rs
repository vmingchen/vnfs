//! Connection construction, reconnect, recovery, and teardown.

use super::*;

impl Drop for NfsVecFs {
    /// `tc_deinit()`: close every open file so the client has no state left,
    /// allowing the session teardown to destroy the clientid on the server.
    fn drop(&mut self) {
        let closes: Vec<crate::client::CloseOp> = self
            .open_files
            .drain()
            .map(|(_, open)| open)
            .chain(self.deferred_descriptor_closes.drain(..))
            .map(|open| crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            })
            .collect();
        let _ = self.nfs.close_many(&closes);
    }
}

impl NfsVecFs {
    /// Close all descriptors and explicitly tear down NFS session state.
    /// `Drop` remains a best-effort fallback when this result is not needed.
    pub fn shutdown(mut self) -> VfResult<()> {
        for callback in HandleBackend::take_notifications(&mut self) {
            callback();
        }
        let observer = self.observer.clone();
        let closes: Vec<crate::client::CloseOp> = self
            .open_files
            .drain()
            .map(|(_, open)| open)
            .chain(self.deferred_descriptor_closes.drain(..))
            .map(|open| crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            })
            .collect();
        let close_result = self
            .nfs
            .close_many(&closes)
            .map_err(vfsi_core::error_from_rpc_indexed);
        let shutdown_result = self
            .nfs
            .shutdown()
            .map_err(|error| vfsi_core::error_from_rpc(error, None));
        let result = close_result.and(shutdown_result);
        if let Some(observer) = observer {
            observer.on_event(&NfsEvent::Shutdown {
                result: result.clone(),
            });
        }
        result
    }

    pub(super) fn notify(&mut self, event: NfsEvent) {
        if self.observer.is_some() {
            self.pending_events.push(event);
        }
    }

    /// Configure bounded automatic recovery for side-effect-free operations.
    pub fn set_recovery_policy(&mut self, policy: NfsRecoveryPolicy) {
        self.recovery_policy = policy;
    }

    /// Enable or disable automatic recovery of side-effect-free operations.
    pub fn set_auto_reconnect(&mut self, enabled: bool) {
        self.auto_reconnect = enabled;
    }

    /// Connect to the NFS server at `host` and resolve the export root.
    pub fn connect(host: &str) -> VfResult<NfsVecFs> {
        Self::connect_with_timeouts(host, None, Duration::from_secs(10), Duration::from_secs(5))
    }

    /// Connect using an explicit NFS minor version (2 enables server COPY).
    pub fn connect_minor(host: &str, minorversion: u32) -> VfResult<NfsVecFs> {
        Self::connect_with_timeouts(
            host,
            Some(minorversion),
            Duration::from_secs(10),
            Duration::from_secs(5),
        )
    }

    /// Connect with explicit setup and per-RPC timeouts.
    pub fn connect_with_timeouts(
        host: &str,
        minorversion: Option<u32>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> VfResult<NfsVecFs> {
        Self::connect_with_options(
            host,
            NfsConnectOptions {
                minorversion,
                connect_timeout,
                request_timeout,
                ..NfsConnectOptions::default()
            },
        )
    }

    /// Connect using explicit protocol, timeout, and authentication options.
    pub fn connect_with_options(host: &str, options: NfsConnectOptions) -> VfResult<NfsVecFs> {
        let root = path_from_bytes(&normalize_bytes(path_bytes(&options.root)));
        let connection = ConnectionConfig {
            host: host.to_owned(),
            root,
            minorversion: options.minorversion,
            connect_timeout: options.connect_timeout,
            request_timeout: options.request_timeout,
            authentication: options.authentication,
            client_owner: options.client_owner,
            client_verifier: make_verifier(),
        };
        let nfs = Self::connect_client(&connection)?;
        let mut filesystem = Self::from_client(nfs, connection);
        filesystem.recovery_policy = options.recovery_policy;
        filesystem.auto_reconnect = options.flags.auto_reconnect();
        if let Some(bytes) = options.max_compound_bytes {
            filesystem.set_max_compound_bytes(bytes.get());
        }
        // The protocol handshake already resolved the pseudo-root. Only a
        // configured sub-root needs an eager lookup and type check.
        if !filesystem.connection.root.as_os_str().is_empty() {
            let root_attrs = filesystem.stat_impl(Path::new("/"))?;
            if root_attrs.ftype != VfType::Directory {
                return Err(VfError::failure(0, ERR_NOTDIR));
            }
        }
        Ok(filesystem)
    }

    fn connect_client(connection: &ConnectionConfig) -> VfResult<NfsClient> {
        match connection.minorversion {
            Some(version) => NfsClient::connect_minor_with_identity(
                &connection.host,
                version,
                connection.connect_timeout,
                connection.request_timeout,
                &connection.authentication,
                connection.client_owner.as_deref(),
                Some(connection.client_verifier),
            ),
            None => NfsClient::connect_with_identity(
                &connection.host,
                connection.connect_timeout,
                connection.request_timeout,
                &connection.authentication,
                connection.client_owner.as_deref(),
                Some(connection.client_verifier),
            ),
        }
        .map_err(|e| vfsi_core::error_from_rpc(e, 0))
    }

    fn from_client(nfs: NfsClient, connection: ConnectionConfig) -> NfsVecFs {
        let server_copy_enabled = cfg!(feature = "server-copy") && nfs.minorversion() >= 2;
        // `cwd` is namespace-relative (the `/`-rooted application namespace),
        // not the server path; `server_path` adds `connection.root`.
        let cwd = PathBuf::new();
        NfsVecFs {
            nfs,
            connection,
            recovery_policy: NfsRecoveryPolicy::default(),
            auto_reconnect: true,
            recovery_in_progress: false,
            cwd,
            next_fd: 0,
            open_files: std::collections::HashMap::new(),
            open_dirs: std::collections::HashMap::new(),
            dir_owner: NEXT_DIR_OWNER.fetch_add(1, Ordering::Relaxed),
            deferred_descriptor_closes: Vec::new(),
            server_copy_enabled,
            server_copy_stats: NfsServerCopyStats::default(),
            merged_mode: MergedIoMode::Full,
            configured_max_compound_bytes: 0,
            read_only: false,
            #[cfg(target_os = "linux")]
            mount_source: None,
            observer: None,
            pending_events: Vec::new(),
            #[cfg(feature = "test-faults")]
            fault_injector: None,
            #[cfg(feature = "test-faults")]
            short_read_once: None,
            #[cfg(feature = "test-faults")]
            short_write_once: None,
        }
    }

    pub(super) fn needs_recovery(error: &VfError) -> bool {
        error.is_transport()
            || matches!(
                error.err_no(),
                nfsstat4_NFS4ERR_EXPIRED
                    | nfsstat4_NFS4ERR_GRACE
                    | nfsstat4_NFS4ERR_STALE_CLIENTID
                    | nfsstat4_NFS4ERR_STALE_STATEID
                    | nfsstat4_NFS4ERR_BAD_STATEID
                    | nfsstat4_NFS4ERR_BADSESSION
                    | nfsstat4_NFS4ERR_DEADSESSION
            )
    }

    fn reconnect_once(&mut self) -> VfResult<()> {
        #[cfg(target_os = "linux")]
        if let Some(mount) = &self.mount_source {
            mount.check_local()?;
        }
        let snapshots: Vec<(i32, ReopenFile, u64, WireFileHandle)> = self
            .open_files
            .iter()
            .map(|(&fd, open)| {
                open.reopen
                    .clone()
                    .map(|reopen| (fd, reopen, open.cur_offset, open.fh.clone()))
                    .ok_or_else(|| {
                        VfError::transport(
                            None,
                            "cannot recover while an internal temporary descriptor is live",
                        )
                    })
            })
            .collect::<VfResult<_>>()?;
        let nfs = Self::connect_client(&self.connection)?;
        let mut replacement = Self::from_client(nfs, self.connection.clone());
        replacement.read_only = self.read_only;
        #[cfg(target_os = "linux")]
        if let Some(mount) = &self.mount_source {
            replacement.recovery_in_progress = true;
            mount.verify(&mut replacement)?;
            replacement.recovery_in_progress = false;
            replacement.mount_source = Some(mount.clone());
        }
        replacement.recovery_policy = self.recovery_policy;
        replacement.auto_reconnect = self.auto_reconnect;
        replacement.cwd = self.cwd.clone();
        replacement.next_fd = self.next_fd;
        replacement.open_dirs = self.open_dirs.clone();
        replacement.dir_owner = self.dir_owner;
        replacement.merged_mode = self.merged_mode;
        replacement.server_copy_stats = self.server_copy_stats;
        replacement.observer = self.observer.clone();
        #[cfg(feature = "test-faults")]
        {
            replacement.fault_injector = self.fault_injector.clone();
            if let Some(injector) = &replacement.fault_injector {
                replacement.nfs.set_fault_injector(injector.clone());
            }
        }
        replacement.configured_max_compound_bytes = self.configured_max_compound_bytes;
        if self.configured_max_compound_bytes != 0 {
            replacement
                .nfs
                .set_max_compound_bytes(self.configured_max_compound_bytes);
        }

        if !snapshots.is_empty() {
            let paths: Vec<&Path> = snapshots
                .iter()
                .map(|(_, open, _, _)| open.path.as_path())
                .collect();
            let flags: Vec<i32> = snapshots.iter().map(|(_, open, _, _)| open.flags).collect();
            let modes: Vec<u32> = snapshots.iter().map(|(_, open, _, _)| open.mode).collect();
            let reopened = VectorBackend::vopen_raw_impl(&mut replacement, &paths, &flags, &modes)?;
            // Validate every identity before removing any handle from the
            // replacement's cleanup map or publishing the new session.
            for ((_, recipe, _, original), file) in snapshots.iter().zip(&reopened) {
                let open = &replacement.open_files[&file.fd().expect("openv returns descriptors")];
                if &open.fh != original {
                    return Err(VfError::client(0, libc::ESTALE as u32)
                        .with_context("reconnect changed file identity", &recipe.path));
                }
            }
            let mut restored = std::collections::HashMap::with_capacity(reopened.len());
            for ((old_fd, _, offset, _), file) in snapshots.iter().zip(reopened) {
                let new_fd = file.fd().expect("openv returns descriptors");
                let mut open = replacement
                    .open_files
                    .remove(&new_fd)
                    .expect("openv registered descriptor");
                open.cur_offset = *offset;
                restored.insert(*old_fd, open);
            }
            replacement.open_files = restored;
        }

        // Publish pending lifecycle events with the new session; a failed
        // reopen must leave them in the old backend for delivery on unlock.
        replacement.pending_events = std::mem::take(&mut self.pending_events);
        std::mem::swap(self, &mut replacement);
        // `replacement` now owns the dead session and its obsolete open
        // state. Do not turn successful recovery into several close/destroy
        // RPC timeouts while it is dropped.
        replacement.open_files.clear();
        replacement.nfs.abandon();
        Ok(())
    }

    /// Establish a fresh session and reopen all live path-backed descriptors.
    /// Descriptor numbers and current offsets are preserved. Reopen never
    /// repeats create, exclusive-create, or truncate side effects.
    pub fn reconnect(&mut self) -> VfResult<()> {
        #[cfg(target_os = "linux")]
        if let Some(mount) = &self.mount_source {
            mount.check_local()?;
        }
        self.notify(NfsEvent::ReconnectStarted);
        let attempts = self.recovery_policy.attempt_limit();
        let mut backoff = self.recovery_policy.initial_backoff;
        let started = std::time::Instant::now();
        let mut last = None;
        for attempt in 0..attempts {
            match self.reconnect_once() {
                Ok(()) => {
                    self.notify(NfsEvent::ReconnectSucceeded);
                    return Ok(());
                }
                Err(error) => {
                    let identity_changed =
                        !error.is_transport() && error.err_no() == libc::ESTALE as u32;
                    last = Some(error);
                    // A different object at the old pathname is permanent,
                    // not a server restart/grace condition to back off over.
                    if identity_changed {
                        break;
                    }
                }
            }
            let elapsed = started.elapsed();
            if attempt + 1 >= attempts || elapsed >= self.recovery_policy.max_elapsed {
                break;
            }
            if !backoff.is_zero() {
                let remaining = self.recovery_policy.max_elapsed.saturating_sub(elapsed);
                std::thread::sleep(backoff.min(self.recovery_policy.max_backoff).min(remaining));
                backoff = backoff
                    .checked_mul(2)
                    .unwrap_or(self.recovery_policy.max_backoff)
                    .min(self.recovery_policy.max_backoff);
            }
        }
        let error = last.unwrap_or_else(|| VfError::transport(None, "NFS reconnect failed"));
        self.notify(NfsEvent::ReconnectFailed {
            error: error.clone(),
        });
        Err(error)
    }

    pub(super) fn read_with_recovery<T>(
        &mut self,
        mut operation: impl FnMut(&mut Self) -> VfResult<T>,
    ) -> VfResult<T> {
        debug_assert!(!self.recovery_in_progress);
        self.recovery_in_progress = true;
        let first = operation(self);
        self.recovery_in_progress = false;
        match first {
            Err(error) if self.auto_reconnect && Self::needs_recovery(&error) => {
                self.reconnect()?;
                self.recovery_in_progress = true;
                let retry = operation(self);
                self.recovery_in_progress = false;
                retry
            }
            result => result,
        }
    }
}
