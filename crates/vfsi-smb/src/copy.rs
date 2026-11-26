use super::*;
use smb2::client::Frame;
use smb2::client::copy::{CopyChunk, CopyChunkOutcome, ServerSideCopyLimits};
use smb2::msg::copychunk::{SrvCopychunk, SrvCopychunkCopy, SrvCopychunkResponse};
use smb2::msg::create::CreateAction;
use smb2::msg::ioctl::{FSCTL_SRV_COPYCHUNK, IoctlRequest, IoctlResponse, SMB2_0_IOCTL_IS_FSCTL};
use smb2::pack::{Pack, WriteCursor};
use vfsi_core::{FailureState, FsObjId, OpError, OpResult};

const STATUS_INFO_LENGTH_MISMATCH: NtStatus = NtStatus(0xc000_0004);

fn copy_chunks(
    pair: &ExtentPair,
    copied: u64,
    length: u64,
    limits: ServerSideCopyLimits,
) -> Vec<CopyChunk> {
    let mut bytes = (length - copied).min(u64::from(limits.max_data_size));
    let mut chunks = Vec::with_capacity(
        bytes
            .div_ceil(u64::from(limits.max_chunk_size))
            .min(u64::from(limits.max_chunks)) as usize,
    );
    let mut offset = copied;
    while bytes > 0 && chunks.len() < limits.max_chunks as usize {
        let length = bytes.min(u64::from(limits.max_chunk_size)) as u32;
        chunks.push(CopyChunk::new(
            pair.src_offset + offset,
            pair.dst_offset + offset,
            length,
        ));
        offset += u64::from(length);
        bytes -= u64::from(length);
    }
    chunks
}

fn copy_acknowledged(chunks: &[CopyChunk], result: SrvCopychunkResponse) -> VfResult<u64> {
    let requested: u64 = chunks.iter().map(|chunk| u64::from(chunk.length)).sum();
    // MS-SMB2 3.3.5.15.6: success acknowledges every range, with no
    // partial final write. Never continue from an impossible byte count.
    if result.chunks_written as usize != chunks.len()
        || result.chunk_bytes_written != 0
        || u64::from(result.total_bytes_written) != requested
    {
        return Err(vfsi_core::internal::errors::invalid_reply(
            "invalid SMB COPYCHUNK acknowledgement",
        ));
    }
    Ok(u64::from(result.total_bytes_written))
}

fn smaller_copy_limits(
    current: ServerSideCopyLimits,
    advertised: ServerSideCopyLimits,
) -> VfResult<ServerSideCopyLimits> {
    if advertised.max_chunks == 0 || advertised.max_chunk_size == 0 || advertised.max_data_size == 0
    {
        return Err(vfsi_core::internal::errors::invalid_reply(
            "zero SMB COPYCHUNK limit",
        ));
    }
    let limits = ServerSideCopyLimits {
        max_chunks: current.max_chunks.min(advertised.max_chunks),
        max_chunk_size: current.max_chunk_size.min(advertised.max_chunk_size),
        max_data_size: current.max_data_size.min(advertised.max_data_size),
    };
    if limits == current {
        return Err(vfsi_core::internal::errors::invalid_reply(
            "SMB COPYCHUNK rejection did not reduce limits",
        ));
    }
    Ok(limits)
}

fn copy_rejected_before_progress(error: &SmbError, copied: u64) -> bool {
    copied == 0
        && matches!(error, SmbError::Protocol { .. })
        && error.kind() == SmbErrorKind::Unsupported
}

fn copy_frame_response(
    frame: &smb2::client::Frame,
    dst_id: FileId,
    chunks: &[CopyChunk],
) -> Result<CopyChunkOutcome, (std::io::Error, bool)> {
    require_command(frame, Command::Ioctl, 0).map_err(|cause| (cause, false))?;
    copy_response(frame.header.status, &frame.body, dst_id, chunks)
}

fn copy_response(
    status: NtStatus,
    body: &[u8],
    dst_id: FileId,
    chunks: &[CopyChunk],
) -> Result<CopyChunkOutcome, (std::io::Error, bool)> {
    let wire_error = || SmbError::Protocol {
        status,
        command: Command::Ioctl,
    };
    // A generic SMB error response is a command rejection. A COPYCHUNK
    // failure response instead carries explicit full/partial byte counts.
    if status != NtStatus::SUCCESS && body.len() >= 8 && body.get(..2) == Some(&[9, 0]) {
        let bytes =
            u32::from_le_bytes(body[4..8].try_into().expect("validated SMB error header")) as usize;
        if bytes > body.len() - 8 {
            return Err((
                vfsi_core::internal::errors::invalid_reply("truncated SMB COPYCHUNK error data"),
                false,
            ));
        }
        let error = wire_error();
        let safe = copy_rejected_before_progress(&error, 0);
        return Err((smb_error(error, None), safe));
    }
    let output = IoctlResponse::unpack(&mut ReadCursor::new(body))
        .map_err(|error| (smb_error(error, None), false))?;
    if output.ctl_code != FSCTL_SRV_COPYCHUNK || output.file_id != dst_id {
        return Err((
            vfsi_core::internal::errors::invalid_reply("mismatched SMB COPYCHUNK response"),
            false,
        ));
    }
    if output.output_data.len() != SrvCopychunkResponse::SIZE {
        return Err((
            vfsi_core::internal::errors::invalid_reply("invalid SMB COPYCHUNK response size"),
            false,
        ));
    }
    let response = SrvCopychunkResponse::unpack(&mut ReadCursor::new(&output.output_data))
        .map_err(|error| (smb_error(error, None), false))?;
    if status == NtStatus::INVALID_PARAMETER {
        return Ok(CopyChunkOutcome::Rejected {
            limits: ServerSideCopyLimits {
                max_chunks: response.chunks_written,
                max_chunk_size: response.chunk_bytes_written,
                max_data_size: response.total_bytes_written,
            },
        });
    }
    if status != NtStatus::SUCCESS {
        let error = wire_error();
        let empty = response.chunks_written == 0
            && response.chunk_bytes_written == 0
            && response.total_bytes_written == 0;
        let safe = empty && copy_rejected_before_progress(&error, 0);
        return Err((smb_error(error, None), safe));
    }
    let total = copy_acknowledged(chunks, response).map_err(|error| (error, false))?;
    Ok(CopyChunkOutcome::Copied(
        smb2::client::copy::CopyChunkResult {
            chunks_written: chunks.len() as u32,
            total_bytes_written: total,
        },
    ))
}

fn copy_identity_requests(file_id: FileId) -> [QueryInfoRequest; 2] {
    [
        (InfoType::File, FILE_INTERNAL_INFORMATION, 8),
        (InfoType::Filesystem, 1, 128),
    ]
    .map(|(info_type, class, length)| QueryInfoRequest {
        info_type,
        file_info_class: class,
        output_buffer_length: length,
        additional_information: 0,
        flags: 0,
        file_id,
        input_buffer: Vec::new(),
    })
}

fn identity_query(request: &QueryInfoRequest, tree: TreeId) -> CompoundOp<'_> {
    CompoundOp {
        command: Command::QueryInfo,
        body: request,
        tree_id: Some(tree),
        credit_charge: CreditCharge(1),
    }
}

async fn copy_identity(
    connection: &Connection,
    tree: TreeId,
    file_id: FileId,
) -> VfResult<FsObjId> {
    // Related operations share a single handle on SMB servers.
    let requests = copy_identity_requests(file_id);
    let operations = requests
        .each_ref()
        .map(|request| identity_query(request, tree));
    let responses = connection
        .execute_compound(&operations)
        .await
        .map_err(|error| smb_error(error, 0))?;
    let [file, volume] = <[_; 2]>::try_from(responses)
        .map_err(|responses| compound_cardinality_error(responses.len(), operations.len()))?;
    decode_identity_replies(file, volume)
}

fn decode_identity_replies(
    file: Result<Frame, SmbError>,
    volume: Result<Frame, SmbError>,
) -> VfResult<FsObjId> {
    // Preserve waiter errors before interpreting either server status.
    let file = file.map_err(|error| smb_error(error, 0))?;
    let volume = volume.map_err(|error| smb_error(error, 0))?;
    let decode = |response: Frame| {
        require_status(&response, Command::QueryInfo, 0)?;
        QueryInfoResponse::unpack(&mut ReadCursor::new(&response.body))
            .map(|response| response.output_buffer)
            .map_err(|error| smb_error(error, 0))
    };
    decode_copy_identity(&decode(file)?, &decode(volume)?)
}

pub(super) async fn copy_identity_and_close(
    connection: &Connection,
    tree: &ShareSession,
    file_id: FileId,
) -> (VfResult<FsObjId>, OpResult<()>) {
    let [file, volume] = copy_identity_requests(file_id);
    let close = CloseRequest { flags: 0, file_id };
    let requests = [
        identity_query(&file, tree.tree_id),
        identity_query(&volume, tree.tree_id),
        CompoundOp {
            command: Command::Close,
            body: &close,
            tree_id: Some(tree.tree_id),
            credit_charge: CreditCharge(1),
        },
    ];
    let failed = |mut cause| {
        let shared = vfsi_core::internal::errors::share_in_place(&mut cause);
        let close = vfsi_core::internal::errors::from_shared(&shared);
        (Err(cause), Err(OpError::unknown(close, None)))
    };
    let (identity, close) = match connection.execute_compound(&requests).await {
        Ok(replies) => {
            let [file, volume, close] = match <[_; 3]>::try_from(replies) {
                Ok(replies) => replies,
                Err(replies) => {
                    return failed(compound_cardinality_error(replies.len(), requests.len()));
                }
            };
            decode_copy_source_close(file, volume, close)
        }
        // The SDK returned this error before sending any operation in the
        // chain. Preserve identity support on a server with only two credits.
        Err(SmbError::CreditStarvation { .. }) => {
            let identity = copy_identity(connection, tree.tree_id, file_id).await;
            return (identity, tree.close_file(file_id).await);
        }
        Err(error) => return failed(smb_error(error, 0)),
    };
    let close = if close
        .as_ref()
        .is_err_and(|error| error.state() == FailureState::NotAttempted)
    {
        tree.close_file(file_id).await
    } else {
        close
    };
    (identity, close)
}

fn decode_copy_source_close(
    file: Result<Frame, SmbError>,
    volume: Result<Frame, SmbError>,
    close: Result<Frame, SmbError>,
) -> (VfResult<FsObjId>, OpResult<()>) {
    // A related CLOSE can inherit an explicit rejection from the preceding
    // QUERY_INFO. Only match explicit query rejections or buffer errors.
    let query_rejection = volume
        .as_ref()
        .ok()
        .filter(|frame| frame.header.command == Command::QueryInfo)
        .map(|frame| frame.header.status)
        .filter(|status| {
            matches!(
                *status,
                NtStatus::NOT_SUPPORTED
                    | NtStatus::NOT_IMPLEMENTED
                    | NtStatus::INVALID_INFO_CLASS
                    | NtStatus::INVALID_DEVICE_REQUEST
                    | NtStatus::ACCESS_DENIED
                    | STATUS_INFO_LENGTH_MISMATCH
                    | NtStatus::BUFFER_TOO_SMALL
                    | NtStatus::BUFFER_OVERFLOW
            )
        });
    let close = match close {
        Ok(frame) => {
            let result = require_status(&frame, Command::Close, 0)
                .map_err(|cause| wire_failure(cause, None));
            if close_can_fallback(&result)
                || frame.header.command == Command::Close
                    && query_rejection == Some(frame.header.status)
            {
                result.map_err(|error| OpError::not_attempted(error.into_cause()))
            } else {
                result
            }
        }
        Err(error) => Err(OpError::unknown(smb_error(error, 0), None)),
    };
    (decode_identity_replies(file, volume), close)
}

fn decode_copy_identity(file_index: &[u8], volume: &[u8]) -> VfResult<FsObjId> {
    let index = file_index
        .first_chunk()
        .copied()
        .map(u64::from_le_bytes)
        .ok_or_else(|| {
            vfsi_core::internal::errors::invalid_reply("short SMB copy file-index response")
        })?;
    let serial = volume
        .get(8..)
        .and_then(|serial| serial.first_chunk())
        .copied()
        .map(u32::from_le_bytes)
        .ok_or_else(|| {
            vfsi_core::internal::errors::invalid_reply("short SMB copy volume response")
        })?;
    // MS-FSCC 2.1.9 reserves both 0 and all ones for unavailable file IDs.
    if index == 0 || index == u64::MAX || serial == 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
    }
    // File indices are volume-local; a share can span several volumes.
    Ok(FsObjId::new(u128::from(serial), index))
}

#[cfg(test)]
fn copy_source_test_response(command: Command, status: NtStatus) -> Result<Frame, SmbError> {
    let mut header = smb2::msg::header::Header::new_request(command);
    header.status = status;
    Ok(Frame {
        header,
        body: Vec::new(),
        raw: Vec::new(),
    })
}

#[cfg(test)]
#[test]
fn copy_source_close_distinguishes_query_rejection_from_uncertain_close() {
    let decode = |query_status, close| {
        decode_copy_source_close(
            copy_source_test_response(Command::QueryInfo, query_status),
            copy_source_test_response(Command::QueryInfo, query_status),
            close,
        )
    };

    let (identity, close) = decode(
        NtStatus::NOT_SUPPORTED,
        copy_source_test_response(Command::Close, NtStatus::SUCCESS),
    );
    assert_eq!(
        identity.unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(close.is_ok());

    for status in [
        NtStatus::NOT_SUPPORTED,
        STATUS_INFO_LENGTH_MISMATCH,
        NtStatus::BUFFER_TOO_SMALL,
        NtStatus::BUFFER_OVERFLOW,
    ] {
        let (_, close) = decode(status, copy_source_test_response(Command::Close, status));
        assert_eq!(
            close.unwrap_err().state(),
            FailureState::NotAttempted,
            "{status:?}"
        );
    }

    let (_, close) = decode(
        NtStatus::NOT_SUPPORTED,
        copy_source_test_response(Command::Close, NtStatus::ACCESS_DENIED),
    );
    assert_eq!(close.unwrap_err().state(), FailureState::Failed);

    let (_, close) = decode(NtStatus::NOT_SUPPORTED, Err(SmbError::Disconnected));
    assert_eq!(close.unwrap_err().state(), FailureState::Unknown);

    let (_, close) = decode(
        NtStatus::NOT_SUPPORTED,
        copy_source_test_response(Command::QueryInfo, NtStatus::NOT_SUPPORTED),
    );
    assert_eq!(close.unwrap_err().state(), FailureState::Unknown);
}

#[cfg(test)]
#[test]
fn copy_source_identity_preserves_volume_waiter_failure() {
    let (identity, close) = decode_copy_source_close(
        copy_source_test_response(Command::QueryInfo, NtStatus::NOT_SUPPORTED),
        Err(SmbError::Disconnected),
        copy_source_test_response(Command::Close, NtStatus::SUCCESS),
    );
    assert!(close.is_ok());
    assert_eq!(
        vfsi_core::transport_kind(&identity.unwrap_err()),
        Some(vfsi_core::TransportKind::Connection),
    );
}

#[cfg(test)]
#[test]
fn copy_identity_rejects_unavailable_file_indices() {
    let mut volume = [0; 12];
    volume[8..].copy_from_slice(&7u32.to_le_bytes());
    for index in [0, u64::MAX] {
        let error = decode_copy_identity(&index.to_le_bytes(), &volume).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
    assert_eq!(
        decode_copy_identity(&11u64.to_le_bytes(), &volume).unwrap(),
        FsObjId::new(7, 11)
    );
}

impl SmbVecFs {
    fn copy_file_identity(&mut self, file_id: FileId) -> VfResult<FsObjId> {
        self.runtime.block_on(copy_identity(
            self.client.connection(),
            self.tree.tree_id,
            file_id,
        ))
    }

    /// Compare retained objects, including aliases across different path spellings.
    pub(super) fn copy_handles_alias(&mut self, files: [FileId; 2]) -> VfResult<bool> {
        Ok(self.copy_file_identity(files[0])? == self.copy_file_identity(files[1])?)
    }

    pub(super) fn copy_extent_with_strategy(
        &mut self,
        pair: &ExtentPair,
        server: bool,
    ) -> VfResult<()> {
        self.copy_retained(pair, server, false).map(|_| ())
    }

    /// Whole-file copy for the Python adapter. An identical source and
    /// destination returns `None` without writing to either handle.
    #[doc(hidden)]
    pub fn copy_whole_file(
        &mut self,
        source: &Path,
        destination: &Path,
        buffered: Option<(&[u8], VfResult<FsObjId>)>,
    ) -> VfResult<Option<u64>> {
        if let Some((data, identity)) = buffered {
            let mut collision = false;
            let path = self.local_path_string(destination)?;
            // An exclusive CREATE cannot open a source alias. Keep the small
            // new-file CREATE/WRITE/FLUSH/CLOSE on the existing compound path.
            match self.compound_write_with_flags(
                &path,
                0,
                data,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                Some(&mut collision),
            ) {
                Ok(written) if written == data.len() => return Ok(Some(written as u64)),
                Ok(_) => return Err(std::io::Error::from_raw_os_error(libc::EIO)),
                Err(error) if collision && error.state() != vfsi_core::FailureState::Unknown => {}
                Err(error) => return Err(error.into_cause()),
            }
            return self.copy_buffered_existing(&path, data, identity?);
        }
        let pair = ExtentPair::from_os_paths(source, 0, destination, 0, None);
        self.copy_retained(&pair, self.server_copy_enabled, true)
    }

    fn copy_buffered_existing(
        &mut self,
        path: &str,
        data: &[u8],
        source_identity: FsObjId,
    ) -> VfResult<Option<u64>> {
        let _ = self.drain_deferred_closes();
        let opened = self.raw_open_response(path, libc::O_WRONLY | libc::O_CREAT)?;
        let id = opened.file_id;
        let result: VfResult<Option<u64>> = (|| {
            if opened.create_action != CreateAction::FileCreated
                && self.copy_file_identity(id)? == source_identity
            {
                return Ok(None);
            }
            let written = self
                .raw_write(id, 0, data)
                .map_err(vfsi_core::OpError::into_cause)? as u64;
            self.set_size_handle(id, written)?;
            self.raw_flush(id)?;
            Ok(Some(written))
        })();
        let close = self.copy_close_handle(id, true);
        let copied = result?;
        close?;
        Ok(copied)
    }

    fn copy_retained(
        &mut self,
        pair: &ExtentPair,
        server: bool,
        reject_alias: bool,
    ) -> VfResult<Option<u64>> {
        let source = self.local_path_string(&pair.src_path)?;
        let destination = self.local_path_string(&pair.dst_path)?;
        let _ = self.drain_deferred_closes();
        let (src_id, src_size) = self.raw_open(&source, libc::O_RDONLY)?;
        let available = src_size.saturating_sub(pair.src_offset);
        let length = pair.length.unwrap_or(available).min(available);
        // Validate the effective range before creating the destination.
        if let Err(error) = checked_offset(pair.dst_offset, length).and_then(Self::file_end_of_file)
        {
            let _ = self.copy_close_handle(src_id, false);
            return Err(error);
        }
        // COPYCHUNK requires read and write access on the destination, as did
        // the SDK path helper. Client-only copies retain write-only access.
        let access = if server { libc::O_RDWR } else { libc::O_WRONLY };
        let opened = match self.raw_open_response(&destination, access | libc::O_CREAT) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.copy_close_handle(src_id, false);
                return Err(error);
            }
        };
        let dst_id = opened.file_id;
        let result = (|| {
            let check_overlap =
                pair.dst_offset > pair.src_offset && pair.dst_offset - pair.src_offset < length;
            if (reject_alias || check_overlap)
                && opened.create_action != CreateAction::FileCreated
                && self.copy_handles_alias([src_id, dst_id])?
            {
                if reject_alias {
                    return Ok(None);
                }
                return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
            }
            let server_result = if server {
                self.copy_server_range(src_id, dst_id, pair, length)?
            } else {
                None
            };
            let copied = match server_result {
                Some(copied) => copied,
                None => {
                    if server {
                        self.server_copy_enabled = false;
                    }
                    self.copy_client_range(src_id, dst_id, pair, length)?
                }
            };
            self.set_size_handle(dst_id, checked_offset(pair.dst_offset, copied)?)?;
            self.raw_flush(dst_id)?;
            Ok(Some(copied))
        })();
        let dst_close = self.copy_close_handle(dst_id, true);
        let src_close = self.copy_close_handle(src_id, false);
        let copied = result?;
        dst_close?;
        src_close?;
        Ok(copied)
    }

    fn copy_close_handle(&mut self, id: FileId, _destination: bool) -> VfResult<()> {
        #[cfg(feature = "test-faults")]
        if let Err(error) = self.inject_open_fault(OpenFaultPoint::BeforeCopyCleanup {
            destination: _destination,
        }) {
            self.deferred_closes
                .push(DeferredClose::new(self.tree.clone(), CloseState::Ready(id)));
            return Err(error);
        }
        self.close_temporary_handle(id)
    }

    fn copy_client_range(
        &mut self,
        src_id: FileId,
        dst_id: FileId,
        pair: &ExtentPair,
        length: u64,
    ) -> VfResult<u64> {
        let mut copied = 0u64;
        while copied < length {
            let wanted = (length - copied).min(1 << 20) as usize;
            let data = self
                .raw_read(src_id, checked_offset(pair.src_offset, copied)?, wanted)
                .map_err(vfsi_core::OpError::into_cause)?;
            if data.is_empty() {
                break;
            }
            self.raw_write(dst_id, checked_offset(pair.dst_offset, copied)?, &data)
                .map_err(vfsi_core::OpError::into_cause)?;
            copied = checked_offset(copied, data.len() as u64)?;
        }
        Ok(copied)
    }

    fn copy_chunk_request(
        &mut self,
        dst_id: FileId,
        key: &smb2::client::copy::ResumeKey,
        chunks: &[CopyChunk],
    ) -> Result<CopyChunkOutcome, (std::io::Error, bool)> {
        let mut packed = WriteCursor::new();
        SrvCopychunkCopy {
            source_key: *key.as_bytes(),
            chunks: chunks.iter().copied().map(SrvCopychunk::from).collect(),
        }
        .pack(&mut packed);
        let request = IoctlRequest {
            ctl_code: FSCTL_SRV_COPYCHUNK,
            file_id: dst_id,
            max_input_response: 0,
            max_output_response: SrvCopychunkResponse::SIZE as u32,
            flags: SMB2_0_IOCTL_IS_FSCTL,
            input_data: packed.into_inner(),
        };
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::Ioctl,
                &request,
                Some(self.tree.tree_id),
            ))
            .map_err(|error| (smb_error(error, None), false))?;
        copy_frame_response(&frame, dst_id, chunks)
    }

    // None means a known rejection before any copy bytes, never lost progress.
    // Reuse the admitted handles on fallback; do not resolve either path again.
    fn copy_server_range(
        &mut self,
        src_id: FileId,
        dst_id: FileId,
        pair: &ExtentPair,
        length: u64,
    ) -> VfResult<Option<u64>> {
        if length == 0 {
            return Ok(Some(0));
        }
        let key = match self.runtime.block_on(
            self.tree
                .request_resume_key(self.client.connection_mut(), src_id),
        ) {
            Ok(key) => key,
            Err(error) if copy_rejected_before_progress(&error, 0) => return Ok(None),
            Err(error) => return Err(smb_error(error, None)),
        };
        let mut copied = 0;
        let mut limits = ServerSideCopyLimits::CONSERVATIVE;
        while copied < length {
            let chunks = copy_chunks(pair, copied, length, limits);
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::BeforeSmbCopyRequest { copied })?;
            let result = self.copy_chunk_request(dst_id, &key, &chunks);
            #[cfg(feature = "test-faults")]
            self.inject_open_fault(OpenFaultPoint::AfterSmbCopyReply { copied })?;
            match result {
                Ok(CopyChunkOutcome::Copied(result)) => {
                    copied += result.total_bytes_written;
                }
                Ok(CopyChunkOutcome::Rejected { limits: advertised }) => {
                    limits = smaller_copy_limits(limits, advertised)?;
                }
                Err((_, true)) if copied == 0 => return Ok(None),
                Err((error, _)) => return Err(error),
            }
        }
        Ok(Some(copied))
    }
}

#[cfg(test)]
mod benchmarks {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    #[ignore = "manual managed-SMB new-destination copy experiment"]
    fn compare_bounded_new_file_copy_waves() {
        let server = std::env::var("VFSI_SMB_SERVER").expect("set VFSI_SMB_SERVER");
        let share = std::env::var("VFSI_SMB_SHARE").expect("set VFSI_SMB_SHARE");
        let backing =
            PathBuf::from(std::env::var_os("VFSI_SMB_LOCAL_ROOT").expect("set local backing root"));
        let username = std::env::var("VFSI_SMB_USERNAME").unwrap_or_default();
        let password = std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default();
        let domain = std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default();
        let mut backend =
            SmbVecFs::connect(&server, &share, &username, &password, &domain).unwrap();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("vfsi-copy-waves-{}-{unique}", std::process::id());
        let path = backing.join(&name);
        std::fs::create_dir(&path).unwrap();
        let fixture = Fixture(path);
        // The local creator and Samba's forced user can be different accounts.
        // Set fixture permissions explicitly so a restrictive umask is harmless.
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (case, files, bytes) in [("small", 32, 4096), ("large", 8, 2 * 1024 * 1024)] {
            let source = fixture.0.join(case);
            std::fs::create_dir(&source).unwrap();
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
            for index in 0..files {
                let payload: Vec<_> = (0..bytes)
                    .map(|offset| ((offset + index) % 251) as u8)
                    .collect();
                let path = source.join(format!("source-{index:02}"));
                std::fs::write(&path, payload).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            for sample in 0..3 {
                let order = if sample % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for prototype in order {
                    let destination = format!("{case}/out-{sample}-{prototype}");
                    let local_destination = fixture.0.join(&destination);
                    std::fs::create_dir(&local_destination).unwrap();
                    std::fs::set_permissions(
                        &local_destination,
                        std::fs::Permissions::from_mode(0o777),
                    )
                    .unwrap();
                    let pairs: Vec<_> = (0..files)
                        .map(|index| {
                            ExtentPair::from_os_paths(
                                &PathBuf::from(format!("/{name}/{case}/source-{index:02}")),
                                0,
                                &PathBuf::from(format!("/{name}/{destination}/file-{index:02}")),
                                0,
                                Some(bytes as u64),
                            )
                        })
                        .collect();
                    let before = backend.client.diagnostics().primary.metrics;
                    let start = Instant::now();
                    let operation = if prototype {
                        vfsi_sync::test_support::copy_new_files_batched(
                            &mut backend,
                            &pairs,
                            0o666,
                            Some(vfsi_core::api::SyncMode::Data),
                        )
                    } else {
                        vfsi_core::internal::collect_vec_result(
                            backend.vcopy_data_impl(&pairs),
                            pairs.len(),
                        )
                        .map(|_| ())
                    };
                    let elapsed = start.elapsed().as_micros();
                    let after = backend.client.diagnostics().primary.metrics;
                    operation.unwrap();
                    assert!(
                        backend.open_files.is_empty(),
                        "every owned copy descriptor must close"
                    );
                    for index in 0..files {
                        assert_eq!(
                            std::fs::read(source.join(format!("source-{index:02}"))).unwrap(),
                            std::fs::read(local_destination.join(format!("file-{index:02}")))
                                .unwrap(),
                        );
                    }
                    println!(
                        "SMB_COPY_WAVES {{\"case\":\"{case}\",\"prototype\":{prototype},\"files\":{files},\"bytes_per_file\":{bytes},\"sample\":{sample},\"smb_requests\":{},\"compound_requests\":{},\"wire_bytes_sent\":{},\"elapsed_us\":{elapsed}}}",
                        after.requests_sent - before.requests_sent,
                        after.compound_requests_sent - before.compound_requests_sent,
                        after.wire_bytes_sent - before.wire_bytes_sent,
                    );
                    std::fs::remove_dir_all(local_destination).unwrap();
                }
            }
        }
    }
    #[cfg(feature = "server-copy")]
    #[test]
    #[ignore = "manual paired managed-SMB retained-copy performance control"]
    fn compare_retained_server_copy_to_sdk_control() {
        let backing =
            PathBuf::from(std::env::var_os("VFSI_SMB_LOCAL_ROOT").expect("local backing root"));
        let name = format!("retained-copy-control-{}", std::process::id());
        let local = backing.join(&name);
        std::fs::create_dir(&local).unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o777)).unwrap();
        let fixture = Fixture(local);
        let mut backend = SmbVecFs::connect(
            &std::env::var("VFSI_SMB_SERVER").unwrap(),
            &std::env::var("VFSI_SMB_SHARE").unwrap(),
            &std::env::var("VFSI_SMB_USERNAME").unwrap_or_default(),
            &std::env::var("VFSI_SMB_PASSWORD").unwrap_or_default(),
            &std::env::var("VFSI_SMB_DOMAIN").unwrap_or_default(),
        )
        .unwrap();
        for (case, files, bytes) in [("small", 8, 4096), ("large", 2, 17 << 20)] {
            for index in 0..files {
                let path = fixture.0.join(format!("{case}-source-{index}"));
                std::fs::write(&path, vec![index as u8; bytes]).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            for mixed in [false, true] {
                for sample in 0..3 {
                    for retained in if sample % 2 == 0 {
                        [false, true]
                    } else {
                        [true, false]
                    } {
                        let pairs: Vec<_> = (0..files)
                            .map(|index| {
                                let source = if mixed && index == 1 {
                                    format!("/{name}/missing")
                                } else {
                                    format!("/{name}/{case}-source-{index}")
                                };
                                ExtentPair::new(
                                    &source,
                                    0,
                                    &format!("/{name}/{case}-out-{index}"),
                                    0,
                                    None,
                                )
                            })
                            .collect();
                        let before = backend.client.diagnostics().primary.metrics;
                        let start = Instant::now();
                        let results: Vec<_> = if retained {
                            backend
                                .vcopy_impl(&pairs, CopyOptions::new())
                                .unwrap()
                                .into_iter()
                                .map(|result| result.map_err(vfsi_core::OpError::into_cause))
                                .collect()
                        } else {
                            // Test-only control: reproduce the former path helper,
                            // including its source stat and path-based final size.
                            pairs
                                .iter()
                                .map(|pair| {
                                    let source = backend.local_path_string(&pair.src_path)?;
                                    let destination = backend.local_path_string(&pair.dst_path)?;
                                    let size = backend.client_stat(&source)?.size;
                                    let copied = backend
                                        .runtime
                                        .block_on(backend.client.server_side_copy_file_range(
                                            &backend.tree,
                                            &source,
                                            0,
                                            &destination,
                                            0,
                                            size,
                                        ))
                                        .map_err(|error| smb_error(error, None))?;
                                    backend
                                        .with_temporary_write_handle(&destination, |backend, id| {
                                            backend.set_size_handle(id, copied)
                                        })
                                })
                                .collect()
                        };
                        let elapsed = start.elapsed().as_micros();
                        let after = backend.client.diagnostics().primary.metrics;
                        let requests = after.requests_sent - before.requests_sent;
                        println!(
                            "SMB_RETAINED_COPY {{\"case\":\"{case}\",\"mixed\":{mixed},\"retained\":{retained},\"sample\":{sample},\"requests\":{requests},\"wire_bytes_sent\":{},\"elapsed_us\":{elapsed}}}",
                            after.wire_bytes_sent - before.wire_bytes_sent
                        );
                        for (index, (pair, result)) in pairs.iter().zip(results).enumerate() {
                            let destination = fixture.0.join(pair.dst_path.file_name().unwrap());
                            if mixed && index == 1 {
                                assert_eq!(
                                    result.unwrap_err().kind(),
                                    std::io::ErrorKind::NotFound
                                );
                                assert!(!destination.exists());
                            } else {
                                result.unwrap();
                                assert_eq!(
                                    std::fs::read(&destination).unwrap(),
                                    vec![index as u8; bytes]
                                );
                                std::fs::remove_file(destination).unwrap();
                            }
                        }
                        assert!(
                            backend.open_files.is_empty() && backend.deferred_closes.is_empty()
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod copy_results_tests {
    use super::*;

    fn response(id: FileId, chunks: u32, partial: u32, bytes: u32) -> Vec<u8> {
        let mut output = WriteCursor::new();
        SrvCopychunkResponse {
            chunks_written: chunks,
            chunk_bytes_written: partial,
            total_bytes_written: bytes,
        }
        .pack(&mut output);
        let mut body = WriteCursor::new();
        IoctlResponse {
            ctl_code: FSCTL_SRV_COPYCHUNK,
            file_id: id,
            flags: 0,
            output_data: output.into_inner(),
        }
        .pack(&mut body);
        body.into_inner()
    }

    #[test]
    fn wrong_command_copy_replies_never_authorize_fallback() {
        let id = FileId {
            persistent: 11,
            volatile: 22,
        };
        let chunks = [CopyChunk::new(0, 0, 4)];
        for status in [
            NtStatus::SUCCESS,
            NtStatus::NOT_SUPPORTED,
            NtStatus::INVALID_DEVICE_REQUEST,
        ] {
            let mut header = smb2::msg::header::Header::new_request(Command::Read);
            header.status = status;
            let mut frame = smb2::client::Frame {
                header,
                body: response(id, 1, 0, 4),
                raw: Vec::new(),
            };
            let (cause, fallback) = copy_frame_response(&frame, id, &chunks).unwrap_err();
            assert_eq!(
                vfsi_core::transport_kind(&(cause)),
                Some(vfsi_core::TransportKind::InvalidReply)
            );
            assert!(!fallback);
            frame.header.command = Command::Ioctl;
            if status == NtStatus::SUCCESS {
                assert!(copy_frame_response(&frame, id, &chunks).is_ok());
            }
        }
    }

    #[test]
    fn copy_reply_keeps_exact_success_and_rejects_invalid_acknowledgements() {
        let id = FileId {
            persistent: 11,
            volatile: 22,
        };
        let chunks = [CopyChunk::new(0, 4, 2), CopyChunk::new(2, 6, 3)];
        assert!(
            matches!(copy_response(NtStatus::SUCCESS, &response(id, 2, 0, 5), id, &chunks), Ok(CopyChunkOutcome::Copied(result)) if result.total_bytes_written == 5)
        );
        for (count, partial, bytes) in [
            (0, 0, 0),
            (1, 0, 5),
            (3, 0, 5),
            (2, 1, 5),
            (2, 0, 4),
            (2, 0, 6),
        ] {
            let (cause, fallback) = copy_response(
                NtStatus::SUCCESS,
                &response(id, count, partial, bytes),
                id,
                &chunks,
            )
            .unwrap_err();
            assert_eq!(
                vfsi_core::transport_kind(&(cause)),
                Some(vfsi_core::TransportKind::InvalidReply)
            );
            assert!(!fallback);
        }
        let other = FileId {
            persistent: 11,
            volatile: 23,
        };
        assert!(
            !copy_response(NtStatus::SUCCESS, &response(other, 2, 0, 5), id, &chunks)
                .unwrap_err()
                .1
        );
        assert!(
            !copy_response(NtStatus::SUCCESS, &[49, 0], id, &chunks)
                .unwrap_err()
                .1
        );
    }

    #[test]
    fn copy_fallback_requires_explicit_zero_progress_rejection() {
        let id = FileId {
            persistent: 11,
            volatile: 22,
        };
        let chunks = [CopyChunk::new(0, 0, 4), CopyChunk::new(4, 4, 4)];
        assert!(
            copy_response(NtStatus::NOT_SUPPORTED, &response(id, 0, 0, 0), id, &chunks)
                .unwrap_err()
                .1
        );
        for (count, partial, bytes) in [(1, 0, 4), (0, 1, 1), (1, 1, 5), (0, 0, 1)] {
            assert!(
                !copy_response(
                    NtStatus::NOT_SUPPORTED,
                    &response(id, count, partial, bytes),
                    id,
                    &chunks
                )
                .unwrap_err()
                .1
            );
        }
        assert!(
            !copy_response(NtStatus::ACCESS_DENIED, &response(id, 0, 0, 0), id, &chunks)
                .unwrap_err()
                .1
        );
        assert!(
            copy_response(
                NtStatus::NOT_SUPPORTED,
                &[9, 0, 0, 0, 0, 0, 0, 0],
                id,
                &chunks
            )
            .unwrap_err()
            .1
        );
        assert!(
            !copy_response(NtStatus::NOT_SUPPORTED, &[9, 0], id, &chunks)
                .unwrap_err()
                .1
        );
        assert!(
            !copy_response(
                NtStatus::NOT_SUPPORTED,
                &[9, 0, 0, 0, 1, 0, 0, 0],
                id,
                &chunks
            )
            .unwrap_err()
            .1
        );
        let unsupported = SmbError::Protocol {
            status: NtStatus::NOT_SUPPORTED,
            command: Command::Ioctl,
        };
        assert!(
            !copy_rejected_before_progress(&unsupported, 1),
            "never replay earlier acknowledged chunks"
        );
    }

    #[test]
    fn copy_packing_keeps_native_batch_size_and_monotonic_limits() {
        let pair = ExtentPair::new("/source", 7, "/destination", 13, Some(17 << 20));
        let first = copy_chunks(&pair, 0, 17 << 20, ServerSideCopyLimits::CONSERVATIVE);
        assert_eq!(first.len(), 16);
        assert_eq!(
            first
                .iter()
                .map(|chunk| u64::from(chunk.length))
                .sum::<u64>(),
            16 << 20
        );
        let tail = copy_chunks(
            &pair,
            16 << 20,
            17 << 20,
            ServerSideCopyLimits::CONSERVATIVE,
        );
        assert_eq!(
            tail,
            [CopyChunk::new(7 + (16 << 20), 13 + (16 << 20), 1 << 20)]
        );
        let smaller = ServerSideCopyLimits {
            max_chunks: 4,
            max_chunk_size: 64 << 10,
            max_data_size: 128 << 10,
        };
        let limits = smaller_copy_limits(ServerSideCopyLimits::CONSERVATIVE, smaller).unwrap();
        assert_eq!(copy_chunks(&pair, 0, 17 << 20, limits).len(), 2);
        assert!(smaller_copy_limits(limits, limits).is_err());
        assert!(smaller_copy_limits(limits, ServerSideCopyLimits::CONSERVATIVE).is_err());
        assert!(
            smaller_copy_limits(
                limits,
                ServerSideCopyLimits {
                    max_chunks: 0,
                    ..limits
                }
            )
            .is_err()
        );
    }
}
