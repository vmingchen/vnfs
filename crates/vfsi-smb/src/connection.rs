use super::*;

impl SmbVecFs {
    /// Whether server-side copy was enabled for this connection.
    pub fn server_copy_enabled(&self) -> bool {
        self.server_copy_enabled
    }

    /// Connect to an SMB share using NTLM credentials (or guest access when
    /// `username` and `password` are empty).
    pub fn connect(
        server: &str,
        share: &str,
        username: &str,
        password: &str,
        domain: &str,
    ) -> VfResult<Self> {
        Self::connect_with_options(
            server,
            share,
            username,
            password,
            domain,
            SmbConnectOptions::default(),
        )
    }

    /// Connect with explicit setup and per-request deadlines.
    ///
    /// DFS shares are rejected with `Unsupported`; referral following is disabled.
    pub fn connect_with_options(
        server: &str,
        share: &str,
        username: &str,
        password: &str,
        domain: &str,
        options: SmbConnectOptions,
    ) -> VfResult<Self> {
        if server.is_empty() || share.is_empty() {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        if options.connect_timeout.is_zero() || options.request_timeout.is_zero() {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        let runtime = smb_runtime()?;
        let config = ClientConfig {
            addr: normalize_server(server),
            timeout: options.connect_timeout,
            response_timeout: Some(options.request_timeout),
            username: username.to_owned(),
            password: password.to_owned(),
            domain: domain.to_owned(),
            auto_reconnect: options.auto_reconnect(),
            compression: options.compression(),
            // Custom compounds are bound to the primary connection. Never let
            // the dependency redirect a tree to an untracked DFS connection.
            dfs_enabled: false,
            dfs_target_policy: smb2::client::DfsTargetPolicy::SameServerOnly,
            dfs_target_overrides: HashMap::new(),
            connect_options: None,
            previous_session_id: None,
        };
        // `smb2` performs TCP connect, negotiate, and session setup in one
        // future. Give TCP its full connect budget plus one request budget for
        // the protocol handshake; ordinary requests use request_timeout alone.
        let setup_timeout = options
            .connect_timeout
            .saturating_add(options.request_timeout);
        // Construct the timeout while the runtime is entered. Creating a
        // Tokio timer before `Runtime::block_on` panics because no reactor is
        // active yet.
        let mut client = runtime
            .block_on(async {
                tokio::time::timeout(setup_timeout, SmbClient::connect(config)).await
            })
            .map_err(|_| {
                VfError::transport_with_kind(
                    None,
                    vfsi_core::TransportKind::Timeout,
                    "SMB session setup timed out",
                )
            })?
            .map_err(|e| smb_error(e, 0))?;
        client
            .connection_mut()
            .set_response_timeout(Some(options.request_timeout));
        client
            .connection_mut()
            .set_send_timeout(Some(options.request_timeout));
        client
            .connection_mut()
            .set_credit_wait_timeout(options.request_timeout);
        let tree = runtime
            .block_on(async {
                tokio::time::timeout(options.request_timeout, client.connect_share(share)).await
            })
            .map_err(|_| {
                VfError::transport_with_kind(
                    None,
                    vfsi_core::TransportKind::Timeout,
                    "SMB share connection timed out",
                )
            })?
            .map_err(|e| smb_error(e, 0))?;
        check_share_routing(tree.is_dfs || tree.dfs_origin.is_some(), share)?;
        Ok(Self {
            runtime,
            client,
            tree,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: HashMap::new(),
            deferred_closes: Vec::new(),
            server_copy_enabled: cfg!(feature = "server-copy"),
            disconnected: false,
            #[cfg(feature = "test-faults")]
            fault_injector: None,
            #[cfg(feature = "test-faults")]
            last_writev_was_concurrent: false,
        })
    }

    /// Close every open handle and disconnect the share, reporting failures.
    ///
    /// `Drop` performs the same cleanup on a best-effort basis when callers do
    /// not need the result.
    pub fn shutdown(mut self) -> VfResult<()> {
        self.shutdown_inner()
    }

    pub(super) fn shutdown_inner(&mut self) -> VfResult<()> {
        if self.disconnected {
            return Ok(());
        }
        let file_ids: Vec<FileId> = self
            .open_files
            .drain()
            .map(|(_, open)| open.file_id)
            .chain(self.deferred_closes.drain(..))
            .collect();
        let close_result = if file_ids.is_empty() {
            Ok(())
        } else {
            let connection = self.client.connection_mut().clone();
            let tree_id = self.tree.tree_id;
            let jobs = file_ids.into_iter().map(|file_id| {
                let connection = connection.clone();
                async move { close_on_connection_result(&connection, tree_id, file_id).await }
            });
            self.runtime
                .block_on(join_all(jobs))
                .into_iter()
                .enumerate()
                .find_map(|(index, result)| result.err().map(|error| error.with_index(index)))
                .map_or(Ok(()), Err)
        };
        let disconnect_result = self
            .runtime
            .block_on(self.client.disconnect_share(&self.tree))
            .map_err(|error| smb_error(error, 0));
        self.disconnected = true;
        close_result.and(disconnect_result)
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn set_fault_injector(&mut self, injector: Arc<dyn FaultInjector>) {
        self.fault_injector = Some(injector);
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_open_handle_count(&self) -> usize {
        self.open_files.len()
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_deferred_close_count(&self) -> usize {
        self.deferred_closes.len()
    }

    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_last_writev_was_concurrent(&self) -> bool {
        self.last_writev_was_concurrent
    }

    #[cfg(feature = "test-faults")]
    pub(super) fn inject_open_fault(&self, point: OpenFaultPoint) -> VfResult<()> {
        self.fault_injector
            .as_ref()
            .map_or(Ok(()), |injector| injector.check(&point))
    }

    /// The dialect negotiated with the server.
    pub fn dialect(&self) -> Dialect {
        self.client
            .params()
            .expect("SMB negotiation completed before SmbVecFs construction")
            .dialect
    }

    pub(super) fn path_string(&self, path: &Path) -> VfResult<String> {
        std::str::from_utf8(path_bytes(path))
            .map(str::to_owned)
            .map_err(|_| VfError::errno(0, libc::EILSEQ as u32))
    }

    pub(super) fn file_path(&self, file: &FileOperand) -> VfResult<PathBuf> {
        match file {
            FileOperand::Fd(fd) => self
                .open_files
                .get(fd)
                .map(|open| open.path.clone())
                .ok_or_else(|| VfError::errno(0, ERR_EBADF)),
            FileOperand::Saved => Err(VfError::unsupported(0)),
            _ => self.vf_path(file),
        }
    }

    pub(super) fn local_path_string(&self, path: &Path) -> VfResult<String> {
        self.path_string(&self.abs_path(path))
    }

    pub(super) fn insert_open_file(&mut self, open: SmbOpen) -> VfResult<Fd> {
        crate::vecfs::insert_fd(&mut self.next_fd, &mut self.open_files, open)
    }

    pub(super) fn drain_deferred_closes(&mut self) -> VfResult<()> {
        let pending = std::mem::take(&mut self.deferred_closes);
        for (index, file_id) in pending.iter().copied().enumerate() {
            if let Err(error) = self.raw_close(file_id) {
                self.deferred_closes.extend(pending.into_iter().skip(index));
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn wire_path(&self, path: &str) -> String {
        let encoded = smb2::name::encode_path(path);
        if self.tree.is_dfs {
            format_dfs_wire_path(&self.tree.server, &self.tree.share_name, &encoded)
        } else {
            encoded
        }
    }

    pub(super) fn open_request(&self, path: &str, flags: i32) -> VfResult<CreateRequest> {
        let access_mode = flags & libc::O_ACCMODE;
        let readable = access_mode != libc::O_WRONLY;
        let writable = access_mode != libc::O_RDONLY;
        if flags & libc::O_TRUNC != 0 && !writable {
            return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
        }
        let mut access = FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE;
        if readable {
            access |= FileAccessMask::FILE_READ_DATA;
        }
        if writable {
            access |= FileAccessMask::FILE_WRITE_DATA | FileAccessMask::FILE_WRITE_ATTRIBUTES;
        }
        let create = flags & libc::O_CREAT != 0;
        let exclusive = flags & libc::O_EXCL != 0;
        let truncate = flags & libc::O_TRUNC != 0;
        let create_disposition = match (create, exclusive, truncate) {
            (true, true, _) => CreateDisposition::FileCreate,
            (true, false, true) => CreateDisposition::FileOverwriteIf,
            (true, false, false) => CreateDisposition::FileOpenIf,
            (false, _, true) => CreateDisposition::FileOverwrite,
            (false, _, false) => CreateDisposition::FileOpen,
        };
        let is_dir = flags & libc::O_DIRECTORY != 0;
        Ok(CreateRequest {
            requested_oplock_level: OplockLevel::None,
            impersonation_level: ImpersonationLevel::Impersonation,
            desired_access: FileAccessMask::new(access),
            file_attributes: if is_dir { 0x10 } else { FILE_ATTRIBUTE_NORMAL },
            share_access: ShareAccess(
                ShareAccess::FILE_SHARE_READ
                    | ShareAccess::FILE_SHARE_WRITE
                    | ShareAccess::FILE_SHARE_DELETE,
            ),
            create_disposition,
            create_options: if is_dir {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            },
            name: self.wire_path(path),
            create_contexts: Vec::new(),
        })
    }

    pub(super) fn raw_open(&mut self, path: &str, flags: i32) -> VfResult<(FileId, u64)> {
        let request = self.open_request(path, flags)?;
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::Create,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::Create, 0)?;
        let response = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(|e| smb_error(e, 0))?;
        Ok((response.file_id, response.end_of_file))
    }

    pub(super) fn raw_close(&mut self, file_id: FileId) -> VfResult<()> {
        let request = CloseRequest { flags: 0, file_id };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(
                self.client
                    .connection_mut()
                    .execute(Command::Close, &request, Some(tree_id)),
            )
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::Close, 0)
    }

    pub(super) fn close_temporary_handle(&mut self, file_id: FileId) -> VfResult<()> {
        let result = self.raw_close(file_id);
        retain_failed_close(&mut self.deferred_closes, file_id, result)
    }

    pub(super) fn close_compound_temporary_handle(
        &mut self,
        file_id: FileId,
        compound_close_succeeded: bool,
    ) -> VfResult<()> {
        if compound_close_succeeded {
            Ok(())
        } else {
            self.close_temporary_handle(file_id)
        }
    }

    pub(super) fn raw_flush(&mut self, file_id: FileId) -> VfResult<()> {
        let request = FlushRequest { file_id };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(
                self.client
                    .connection_mut()
                    .execute(Command::Flush, &request, Some(tree_id)),
            )
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::Flush, 0)
    }
}
