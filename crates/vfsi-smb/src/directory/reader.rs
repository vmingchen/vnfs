//! The dependency's reader hides its handle, so it cannot anchor fallback
//! reparse probes. Retain one handle for enumeration, identity and name queries;
//! directory entry codecs and session-scoped close remain in smb2.
//! A fresh read-only open can recover a dead session once. Enumeration never
//! replays: reject a cursor whose connection generation or session has changed.
use super::*;
use futures_util::FutureExt;
use smb2::msg::directory_info::parse_entries;
use smb2::msg::query_directory::{
    FileInformationClass, QueryDirectoryFlags, QueryDirectoryRequest, QueryDirectoryResponse,
};
use smb2::types::SessionId;

pub(super) struct DirectoryReader {
    connection: Connection,
    tree: Arc<ShareSession>,
    runtime: tokio::runtime::Handle,
    session: (u64, SessionId),
    file_id: Option<FileId>,
    class: FileInformationClass,
    restart: bool,
    buffer_len: u32,
    names_supported: bool,
}

impl DirectoryReader {
    pub(super) fn open(fs: &mut SmbVecFs, path: &str, index_numbers: bool) -> VfResult<Self> {
        let mut request = fs.open_request(path, libc::O_RDONLY | libc::O_DIRECTORY)?;
        request.create_options = 1; // FILE_DIRECTORY_FILE; preserve ordinary root following.
        let mut retried = false;
        let (connection, session, frame) = loop {
            let connection = fs.client.connection_mut().clone();
            let session = (connection.generation(), connection.session_id());
            match fs.runtime.block_on(connection.execute(
                Command::Create,
                &request,
                Some(fs.tree.tree_id),
            )) {
                Ok(frame) => break (connection, session, frame),
                Err(error)
                    if !retried
                        && fs.client.config().auto_reconnect
                        && connection.can_reconnect()
                        && matches!(
                            error,
                            SmbError::Disconnected
                                | SmbError::ServerUnresponsive { .. }
                                | SmbError::HandleInvalidated { .. }
                        ) =>
                {
                    // Nothing has been enumerated yet, and FILE_OPEN does not
                    // mutate the namespace. Restore the tree as well as the
                    // transport; its old id belongs to the dead session.
                    let tree = fs.runtime.block_on(async {
                        fs.client.reconnect().await?;
                        fs.client.connect_share(&fs.tree.share_name).await
                    });
                    let tree = tree.map_err(|error| smb_error(error, 0))?;
                    check_share_routing(
                        tree.is_dfs || tree.dfs_origin.is_some(),
                        &tree.share_name,
                    )?;
                    fs.tree = Arc::new(ShareSession::new(tree, fs.client.connection().clone()));
                    retried = true;
                }
                Err(error) => return Err(smb_error(error, 0)),
            }
        };
        require_status(&frame, Command::Create, 0)?;
        let opened = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(|error| smb_error(error, 0))?;
        Ok(Self {
            buffer_len: connection
                .params()
                .map_or(65536, |p| p.max_transact_size.min(65536)),
            connection,
            tree: fs.tree.clone(),
            runtime: fs.runtime.handle().clone(),
            session,
            file_id: Some(opened.file_id),
            class: if index_numbers {
                FileInformationClass::FileIdBothDirectoryInformation
            } else {
                FileInformationClass::FileBothDirectoryInformation
            },
            restart: true,
            names_supported: true,
        })
    }

    fn file_id(&self) -> VfResult<FileId> {
        if self.session != (self.connection.generation(), self.connection.session_id()) {
            return Err(VfError::transport(
                None,
                "SMB directory cursor session was invalidated",
            ));
        }
        self.file_id.ok_or_else(|| VfError::errno(0, ERR_EBADF))
    }

    pub(super) async fn current_path(&mut self, fallback: &Path) -> VfResult<(PathBuf, u64)> {
        let file_id = self.file_id()?;
        // Put the mandatory identity query first so an unsupported name class
        // cannot cascade into it. Older servers can use the saved spelling
        // only while the caller verifies that it still names this identity.
        let requests = [
            QueryInfoRequest {
                info_type: smb2::msg::query_info::InfoType::File,
                file_info_class: 6, // FileInternalInformation.
                output_buffer_length: 8,
                additional_information: 0,
                flags: 0,
                file_id,
                input_buffer: Vec::new(),
            },
            QueryInfoRequest {
                info_type: smb2::msg::query_info::InfoType::File,
                file_info_class: 48, // FileNormalizedNameInformation: share-relative.
                output_buffer_length: self.buffer_len,
                additional_information: 0,
                flags: 0,
                file_id,
                input_buffer: Vec::new(),
            },
        ];
        let count = if self.names_supported { 2 } else { 1 };
        let operations: Vec<_> = requests[..count]
            .iter()
            .map(|request| CompoundOp {
                command: Command::QueryInfo,
                body: request,
                tree_id: Some(self.tree.tree_id),
                credit_charge: CreditCharge(1),
            })
            .collect();
        let frames = self
            .connection
            .execute_compound(&operations)
            .await
            .map_err(|error| smb_error(error, 0))?;
        let frames = collect_compound(frames, count)?;
        require_status(&frames[0], Command::QueryInfo, 0)?;
        let identity = QueryInfoResponse::unpack(&mut ReadCursor::new(&frames[0].body))
            .map_err(|error| smb_error(error, 0))?;
        let identity = u64::from_le_bytes(
            identity
                .output_buffer
                .get(..8)
                .ok_or_else(|| VfError::invalid_reply(None, "short SMB directory identity"))?
                .try_into()
                .expect("identity length checked"),
        );
        let path = if self.names_supported {
            let frame = &frames[1];
            if matches!(
                frame.header.status,
                NtStatus::INVALID_INFO_CLASS
                    | NtStatus::NOT_SUPPORTED
                    | NtStatus::INVALID_PARAMETER
            ) {
                self.names_supported = false;
                fallback.to_path_buf()
            } else {
                require_status(frame, Command::QueryInfo, 0)?;
                let response = QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(|error| smb_error(error, 0))?;
                directory_name(&response.output_buffer)?
            }
        } else {
            fallback.to_path_buf()
        };
        Ok((path, identity))
    }

    pub(super) async fn close(&mut self) -> VfRes {
        if let Some(file_id) = self.file_id.take() {
            self.tree
                .close_handle(&mut self.connection, file_id)
                .await
                .map_err(|error| smb_error(error, 0))?;
        }
        Ok(())
    }

    pub(super) async fn next_batch(&mut self) -> VfResult<Option<Vec<DirectoryEntry>>> {
        loop {
            let request = QueryDirectoryRequest {
                file_information_class: self.class,
                flags: QueryDirectoryFlags(if self.restart {
                    QueryDirectoryFlags::RESTART_SCANS
                } else {
                    0
                }),
                file_index: 0,
                file_id: self.file_id()?,
                output_buffer_length: self.buffer_len,
                file_name: "*".to_owned(),
            };
            let frame = self
                .connection
                .execute(Command::QueryDirectory, &request, Some(self.tree.tree_id))
                .await
                .map_err(|error| smb_error(error, 0))?;
            if frame.header.status == NtStatus::NO_MORE_FILES {
                // Keep the handle for final-page metadata probes. The caller
                // closes it after projection (or Drop closes it on failure).
                return Ok(None);
            }
            if self.restart
                && self.class == FileInformationClass::FileIdBothDirectoryInformation
                && matches!(
                    frame.header.status,
                    NtStatus::INVALID_INFO_CLASS
                        | NtStatus::NOT_SUPPORTED
                        | NtStatus::INVALID_PARAMETER
                )
            {
                self.class = FileInformationClass::FileBothDirectoryInformation;
                continue;
            }
            require_status(&frame, Command::QueryDirectory, 0)?;
            let response = QueryDirectoryResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(|error| smb_error(error, 0))?;
            let raw = parse_entries(self.class, &response.output_buffer)
                .map_err(|error| smb_error(error, 0))?;
            if raw.is_empty() {
                return Err(VfError::invalid_reply(
                    None,
                    "SMB directory page made no progress",
                ));
            }
            self.restart = false;
            return Ok(Some(
                raw.into_iter()
                    .map(|raw| {
                        let mut entry = DirectoryEntry::default();
                        entry.name = smb2::name::decode_name(&raw.file_name).into_owned();
                        entry.size = raw.end_of_file;
                        entry.attributes =
                            smb2::types::flags::FileAttributes::new(raw.file_attributes);
                        entry.is_directory = entry.attributes.is_directory();
                        entry.created = raw.creation_time;
                        entry.modified = raw.last_write_time;
                        entry.changed = raw.change_time;
                        entry.index_number = raw.file_id.filter(|&id| id != 0);
                        entry
                    })
                    .collect(),
            ));
        }
    }
}

impl Drop for DirectoryReader {
    fn drop(&mut self) {
        if let Some(file_id) = self.file_id.take() {
            let tree = self.tree.clone();
            let mut connection = self.connection.clone();
            // The retained Tree pins CLOSE to its original session. Never
            // retry a CLOSE already attempted, even after an ambiguous failure.
            let mut close = Box::pin(async move {
                let _ = tree.close_handle(&mut connection, file_id).await;
            });
            // Queue CLOSE before the owner's next request when credits permit,
            // without blocking Drop. Keep the same future if it must wait.
            let _entered = self.runtime.enter();
            if close.as_mut().now_or_never().is_none() {
                self.runtime.spawn(close);
            }
        }
    }
}

fn directory_name(bytes: &[u8]) -> VfResult<PathBuf> {
    let invalid = || VfError::invalid_reply(None, "invalid SMB normalized directory name");
    let length = u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or_else(invalid)?
            .try_into()
            .expect("checked name header"),
    ) as usize;
    let raw = bytes
        .get(4..)
        .and_then(|bytes| bytes.get(..length))
        .ok_or_else(invalid)?;
    let (pairs, trailing) = raw.as_chunks::<2>();
    if !trailing.is_empty() {
        return Err(invalid());
    }
    let units: Vec<_> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    let wire = String::from_utf16(&units).map_err(|_| invalid())?;
    let relative = wire.strip_prefix('\\').unwrap_or(&wire);
    if relative.contains(['\0', '/'])
        || (!relative.is_empty()
            && relative
                .split('\\')
                .any(|p| p.is_empty() || matches!(p, "." | "..")))
    {
        return Err(invalid());
    }
    Ok(PathBuf::from(smb2::name::decode_path(relative)))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Runs in SMB CI with a required live fixture. Tear down only this
    /// client's real transport; no server restart or timing-based proxy needed.
    #[test]
    fn live_directory_open_recovers_without_replaying_retained_cursors() {
        let (server, share) = match (
            std::env::var("VFSI_SMB_SERVER"),
            std::env::var("VFSI_SMB_SHARE"),
        ) {
            (Ok(server), Ok(share)) => (server, share),
            _ => {
                assert_ne!(
                    std::env::var("VFSI_SMB_REQUIRED").as_deref(),
                    Ok("1"),
                    "live SMB directory recovery requires server and share"
                );
                return;
            }
        };
        let username = std::env::var("VFSI_SMB_USERNAME").unwrap_or_default();
        let password = std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default();
        let domain = std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default();
        for auto_reconnect in [true, false] {
            let mut fs = SmbVecFs::connect_with_options(
                &server,
                &share,
                &username,
                &password,
                &domain,
                SmbConnectOptions::default().with_auto_reconnect(auto_reconnect),
            )
            .unwrap();
            let generation = fs.client.connection().generation();
            let missing = format!(
                "vfsi-missing-directory-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let error = DirectoryReader::open(&mut fs, &missing, false)
                .err()
                .expect("a semantic open failure must not be retried");
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
            assert_eq!(fs.client.connection().generation(), generation);

            let mut retained = DirectoryReader::open(&mut fs, "", false).unwrap();
            fs.client.connection().mark_dead();
            assert!(fs.client.is_disconnected());
            let fresh = DirectoryReader::open(&mut fs, "", false);
            if auto_reconnect {
                let mut fresh = fresh.expect("a fresh directory open must reconnect");
                assert_eq!(fs.client.connection().generation(), generation + 1);
                fs.runtime.block_on(fresh.next_batch()).unwrap();
                fs.runtime.block_on(fresh.close()).unwrap();
            } else {
                assert!(fresh.err().expect("reconnect is disabled").is_transport());
                assert_eq!(fs.client.connection().generation(), generation);
                assert!(fs.client.is_disconnected());
            }
            let error = fs.runtime.block_on(retained.next_batch()).unwrap_err();
            assert!(error.is_transport());
            if auto_reconnect {
                assert!(
                    error.to_string().contains("session was invalidated"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn normalized_names_preserve_mapping_and_reject_malformed_or_escaping_replies() {
        let reply = |text: &str| {
            let bytes: Vec<_> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            [(bytes.len() as u32).to_le_bytes().as_slice(), &bytes].concat()
        };
        assert_eq!(
            directory_name(&reply(r"\parent\moved")).unwrap(),
            Path::new("parent/moved")
        );
        assert_eq!(directory_name(&reply("")).unwrap(), Path::new(""));
        for text in [r"\..\outside", r"parent\\child", "a/b", "bad\0name"] {
            assert!(directory_name(&reply(text)).is_err(), "{text:?}");
        }
        for bytes in [vec![], vec![1, 0, 0, 0, 1], vec![8, 0, 0, 0, 1, 0]] {
            assert!(directory_name(&bytes).is_err());
        }
    }
}
