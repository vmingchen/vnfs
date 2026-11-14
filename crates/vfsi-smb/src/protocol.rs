use super::*;
use vfsi_core::{FailureState, IoProgress, OpError, OpResult};

pub(super) struct CompoundOutcome<T, E = VfError> {
    pub(super) result: Result<T, E>,
    pub(super) deferred_close: Option<CloseState>,
}

pub(super) fn queue_deferred_closes<T, E, O>(
    outcomes: &[CompoundOutcome<T, E>],
    deferred: &mut Vec<O>,
    owner: impl Fn(CloseState) -> O,
) {
    deferred.extend(
        outcomes
            .iter()
            .filter_map(|outcome| outcome.deferred_close)
            .map(owner),
    );
}

pub(super) fn ascii_path_is_prefix(path: &Path, other: &Path) -> bool {
    let mut path = path.components();
    let mut other = other.components();
    loop {
        let Some(component) = path.next() else {
            return true;
        };
        let Some(other_component) = other.next() else {
            return false;
        };
        let Some(component) = component.as_os_str().to_str() else {
            return false;
        };
        let Some(other_component) = other_component.as_os_str().to_str() else {
            return false;
        };
        if !component.eq_ignore_ascii_case(other_component) {
            return false;
        }
    }
}

pub(super) fn paths_are_independent(paths: &[PathBuf]) -> bool {
    // SMB normally resolves names case-insensitively. ASCII aliases can be
    // compared without allocation; for non-ASCII paths, the server's Unicode
    // upcase rules are not available here, so keep the batch ordered.
    if paths
        .iter()
        .any(|path| !path.to_str().is_some_and(str::is_ascii))
    {
        return false;
    }
    for (index, path) in paths.iter().enumerate() {
        if paths[index + 1..]
            .iter()
            .any(|other| ascii_path_is_prefix(path, other) || ascii_path_is_prefix(other, path))
        {
            return false;
        }
    }
    true
}

pub(super) fn handle_info(basic: &[u8], standard: &[u8]) -> VfResult<smb2::client::FileInfo> {
    if basic.len() < 40 || standard.len() < 24 {
        return Err(VfError::invalid_reply(
            None,
            "short SMB handle metadata response",
        ));
    }
    let value = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().expect("length checked"));
    // FileInfo is non-exhaustive; initialize only the fields available in
    // FILE_BASIC_INFORMATION and FILE_STANDARD_INFORMATION.
    let mut info = smb2::client::FileInfo::default();
    info.created = FileTime(value(&basic[0..8]));
    info.accessed = FileTime(value(&basic[8..16]));
    info.modified = FileTime(value(&basic[16..24]));
    info.changed = FileTime(value(&basic[24..32]));
    info.attributes = smb2::types::flags::FileAttributes(u32::from_le_bytes(
        basic[32..36].try_into().expect("length checked"),
    ));
    info.size = value(&standard[8..16]);
    info.is_directory =
        standard[21] != 0 || info.attributes.0 & smb2::types::flags::FileAttributes::DIRECTORY != 0;
    Ok(info)
}

pub(super) fn mutation_create_request(mut create: CreateRequest) -> CreateRequest {
    create.desired_access = FileAccessMask::new(
        FileAccessMask::DELETE | FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
    );
    // Open the final reparse point itself so renaming/deleting a symlink cannot
    // mutate its target. Leave file/directory type unconstrained to cover dangling and
    // directory symlinks as well as ordinary files and directories.
    create.create_options = FILE_OPEN_REPARSE_POINT;
    create
}

pub(super) fn identities_are_independent(identities: &[u64]) -> bool {
    let mut unique = HashSet::with_capacity(identities.len());
    identities.iter().all(|identity| unique.insert(*identity))
}

pub(super) async fn delete_entry_on_connection(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
) -> CompoundOutcome<()> {
    set_info_on_connection(
        connection,
        tree,
        create,
        FILE_DISPOSITION_INFORMATION,
        vec![1],
    )
    .await
}

pub(super) async fn rename_entry_on_connection(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
    destination: &str,
) -> CompoundOutcome<()> {
    // FILE_RENAME_INFORMATION_TYPE_2, MS-FSCC 2.4.42.2. SMB paths are
    // encoded by the SDK's shared name mapping before UTF-16 serialization.
    let name = smb2::name::encode_path(destination);
    let mut buffer = vec![0; 20];
    buffer[0] = 1; // ReplaceIfExists: Vfsi RenameMode::Replace is atomic.
    for unit in name.encode_utf16() {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    let bytes = (buffer.len() - 20) as u32;
    buffer[16..20].copy_from_slice(&bytes.to_le_bytes());
    set_info_on_connection(connection, tree, create, 10, buffer).await
}

async fn set_info_on_connection(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
    information_class: u8,
    buffer: Vec<u8>,
) -> CompoundOutcome<()> {
    let tree_id = tree.tree_id;
    let mut deferred_close = None;
    let result = async {
        let set_info = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: information_class,
            additional_information: 0,
            file_id: FileId::SENTINEL,
            buffer,
        };
        let close = CloseRequest {
            flags: 0,
            file_id: FileId::SENTINEL,
        };
        let operations = [
            CompoundOp {
                command: Command::Create,
                body: &create,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::SetInfo,
                body: &set_info,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::Close,
                body: &close,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
        ];
        let responses = connection
            .execute_compound(&operations)
            .await
            .map_err(|error| smb_error(error, 0))?;
        let OpenCompoundResponseState {
            file_id,
            operation_responses,
            close_result,
            cardinality_error,
            ..
        } = decode_open_compound_responses(
            responses,
            operations.len(),
            |response| {
                require_status(response, Command::Create, 0)?;
                CreateResponse::unpack(&mut ReadCursor::new(&response.body))
                    .map(|opened| opened.file_id)
                    .map_err(|error| smb_error(error, 0))
            },
            |response| {
                require_status(response, Command::Close, 0)
                    .map_err(|cause| wire_failure(cause, None))
            },
            |error| smb_error(error, 0),
        )?;
        let close_result =
            close_compound_on_connection(&tree, file_id, close_result, &mut deferred_close).await;

        if let Some(error) = cardinality_error {
            return Err(error);
        }
        let set_info_response = operation_responses
            .into_iter()
            .next()
            .expect("complete compound contains SET_INFO response")
            .map_err(|error| smb_error(error, 0))?;
        require_status(&set_info_response, Command::SetInfo, 0)?;
        close_result
    }
    .await;
    CompoundOutcome {
        result,
        deferred_close,
    }
}

/// Run a related CREATE/CLOSE pair, report the object's reparse flag, and
/// retain any acquired handle whose cleanup remains unconfirmed.
pub(super) async fn open_close_on_connection(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
) -> CompoundOutcome<bool> {
    let tree_id = tree.tree_id;
    let mut deferred_close = None;
    let close = CloseRequest {
        flags: 0,
        file_id: FileId::SENTINEL,
    };
    let operations = [
        CompoundOp {
            command: Command::Create,
            body: &create,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Close,
            body: &close,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
    ];
    let result = async {
        let responses = connection
            .execute_compound(&operations)
            .await
            .map_err(|error| smb_error(error, 0))?;
        let OpenCompoundResponseState {
            file_id,
            create_response,
            close_result,
            cardinality_error,
            ..
        } = decode_open_compound_responses(
            responses,
            operations.len(),
            |response| {
                require_status(response, Command::Create, 0)?;
                CreateResponse::unpack(&mut ReadCursor::new(&response.body))
                    .map(|opened| opened.file_id)
                    .map_err(|error| smb_error(error, 0))
            },
            |response| {
                require_status(response, Command::Close, 0)
                    .map_err(|cause| wire_failure(cause, None))
            },
            |error| smb_error(error, 0),
        )?;
        let is_reparse_point = CreateResponse::unpack(&mut ReadCursor::new(&create_response.body))
            .map(|opened| opened.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
            .map_err(|error| smb_error(error, 0));
        let close_result =
            close_compound_on_connection(&tree, file_id, close_result, &mut deferred_close).await;
        if let Some(error) = cardinality_error {
            return Err(error);
        }
        close_result?;
        is_reparse_point
    }
    .await;
    CompoundOutcome {
        result,
        deferred_close,
    }
}

/// Return the server's stable file index for an existing path without
/// changing it. Hard links and paths reached through reparse aliases report
/// the same index, allowing `vwrite_impl` to avoid racing writes to one object.
pub(super) async fn concurrent_file_identity(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
) -> CompoundOutcome<u64> {
    let mut deferred_close = None;
    let result =
        concurrent_file_identity_inner(connection, tree, create, &mut deferred_close).await;
    CompoundOutcome {
        result,
        deferred_close,
    }
}

pub(super) async fn concurrent_file_identity_inner(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
    deferred_close: &mut Option<CloseState>,
) -> VfResult<u64> {
    let tree_id = tree.tree_id;
    let query = QueryInfoRequest {
        info_type: InfoType::File,
        file_info_class: FILE_INTERNAL_INFORMATION,
        output_buffer_length: 8,
        additional_information: 0,
        flags: 0,
        file_id: FileId::SENTINEL,
        input_buffer: Vec::new(),
    };
    let close = CloseRequest {
        flags: 0,
        file_id: FileId::SENTINEL,
    };
    let operations = [
        CompoundOp {
            command: Command::Create,
            body: &create,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::QueryInfo,
            body: &query,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Close,
            body: &close,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
    ];
    let responses = connection
        .execute_compound(&operations)
        .await
        .map_err(|error| smb_error(error, 0))?;
    let OpenCompoundResponseState {
        file_id,
        operation_responses,
        close_result,
        cardinality_error,
        ..
    } = decode_open_compound_responses(
        responses,
        operations.len(),
        |response| {
            require_status(response, Command::Create, 0)?;
            CreateResponse::unpack(&mut ReadCursor::new(&response.body))
                .map(|opened| opened.file_id)
                .map_err(|error| smb_error(error, 0))
        },
        |response| {
            require_status(response, Command::Close, 0).map_err(|cause| wire_failure(cause, None))
        },
        |error| smb_error(error, 0),
    )?;
    let identity = if let Some(error) = cardinality_error {
        Err(error)
    } else {
        (|| {
            let response = operation_responses
                .into_iter()
                .next()
                .expect("complete compound contains QUERY_INFO response")
                .map_err(|error| smb_error(error, 0))?;
            require_status(&response, Command::QueryInfo, 0)?;
            let response = QueryInfoResponse::unpack(&mut ReadCursor::new(&response.body))
                .map_err(|error| smb_error(error, 0))?;
            let bytes: [u8; 8] = response
                .output_buffer
                .get(..8)
                .ok_or_else(|| {
                    VfError::invalid_reply(None, "short SMB FileInternalInformation response")
                })?
                .try_into()
                .expect("eight-byte slice");
            Ok(u64::from_le_bytes(bytes))
        })()
    };
    let close = close_compound_on_connection(&tree, file_id, close_result, deferred_close).await;
    match identity {
        Ok(identity) => close.map(|()| identity),
        Err(error) => {
            let _ = close;
            Err(error)
        }
    }
}

/// Only an explicit related-chain inheritance rejection proves that CLOSE
/// did not resolve a handle. Missing, malformed, or lost replies never do.
pub(super) fn close_can_fallback(result: &OpResult<()>) -> bool {
    result.as_ref().is_err_and(|error| {
        error.state() == FailureState::Failed
            && matches!(error.cause().raw_code(), 0xc0000008 | 0xc000000d)
    })
}

pub(super) async fn close_compound_on_connection(
    tree: &Arc<ShareSession>,
    file_id: FileId,
    close_result: OpResult<()>,
    deferred_close: &mut Option<CloseState>,
) -> VfResult<()> {
    let result = if close_can_fallback(&close_result) {
        tree.close_file(file_id).await
    } else {
        close_result
    };
    finish_compound_close(deferred_close, file_id, result)
}

pub(super) fn finish_compound_close(
    deferred_close: &mut Option<CloseState>,
    file_id: FileId,
    result: OpResult<()>,
) -> VfResult<()> {
    result.map_err(|error| {
        *deferred_close = Some(CloseState::NoReplay(file_id));
        error.into_cause()
    })
}

/// Classify at a READ/cleanup protocol boundary, where a missing reply can
/// conceal completion. Local validation uses NotAttempted separately.
pub(super) async fn flush_file(
    connection: Connection,
    tree_id: TreeId,
    file_id: FileId,
) -> OpResult<()> {
    let frame = connection
        .execute(Command::Flush, &FlushRequest { file_id }, Some(tree_id))
        .await
        .map_err(|error| wire_failure(smb_error(error, 0), None))?;
    require_status(&frame, Command::Flush, 0).map_err(|error| wire_failure(error, None))
}

pub(super) fn wire_failure(cause: VfError, progress: Option<IoProgress>) -> OpError {
    if cause.is_transport() {
        OpError::unknown(cause, progress)
    } else {
        OpError::failed(cause, progress)
    }
}

pub(super) fn write_wire_error(cause: VfError, written: usize) -> OpError {
    wire_failure(
        cause,
        (written != 0).then_some(IoProgress::Write {
            bytes_written: written,
        }),
    )
}

/// CREATE/WRITE and temporary cleanup can have effects despite a final failure.
/// Cleanup must retain the acknowledged WRITE count rather than discard it.
pub(super) fn finish_write_result(
    result: OpResult<usize>,
    close: VfResult<()>,
    structural_error: Option<VfError>,
) -> OpResult<usize> {
    let close = close.and(structural_error.map_or(Ok(()), Err));
    match (result, close) {
        (Ok(written), Ok(())) => Ok(written),
        (Ok(written), Err(cause)) => Err(write_wire_error(cause, written)),
        (Err(error), close) => {
            let uncertain_close = close.as_ref().err().is_some_and(VfError::is_transport);
            let (cause, state, progress) = error.into_parts();
            Err(if state == FailureState::Unknown || uncertain_close {
                OpError::unknown(cause, progress)
            } else {
                OpError::failed(cause, progress)
            })
        }
    }
}

pub(super) fn read_wire_error(cause: VfError, offset: u64, data: Option<Vec<u8>>) -> OpError {
    wire_failure(
        cause,
        data.map(|data| IoProgress::Read(vfsi_core::api::ReadResult::owned(offset, data, false))),
    )
}

/// Temporary handle cleanup must never erase confirmed read data.
pub(super) fn finish_read_result(
    result: OpResult<Vec<u8>>,
    close: VfResult<()>,
    structural_error: Option<VfError>,
    offset: u64,
    requested: usize,
) -> OpResult<Vec<u8>> {
    let close = close.and(structural_error.map_or(Ok(()), Err));
    match (result, close) {
        (Ok(data), Ok(())) => Ok(data),
        (Ok(data), Err(cause)) => {
            let eof = requested > 0 && data.len() < requested;
            let progress = Some(IoProgress::Read(vfsi_core::api::ReadResult::owned(
                offset, data, eof,
            )));
            Err(if cause.is_transport() {
                OpError::unknown(cause, progress)
            } else {
                OpError::failed(cause, progress)
            })
        }
        (Err(error), close) => {
            // OPEN and cleanup have already executed around this read. A local
            // READ rejection is no longer a wholly untouched logical input.
            let uncertain_close = close.as_ref().err().is_some_and(VfError::is_transport);
            let (cause, state, progress) = error.into_parts();
            Err(if state == FailureState::Unknown || uncertain_close {
                OpError::unknown(cause, progress)
            } else {
                OpError::failed(cause, progress)
            })
        }
    }
}

pub(super) async fn concurrent_compound_read(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
    offset: u64,
    length: usize,
) -> CompoundOutcome<Vec<u8>, OpError> {
    let mut deferred_close = None;
    let result = concurrent_compound_read_inner(
        connection,
        tree,
        create,
        offset,
        length,
        &mut deferred_close,
    )
    .await;
    CompoundOutcome {
        result,
        deferred_close,
    }
}

pub(super) async fn concurrent_compound_read_inner(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
    offset: u64,
    length: usize,
    deferred_close: &mut Option<CloseState>,
) -> OpResult<Vec<u8>> {
    let tree_id = tree.tree_id;
    let read = ReadRequest {
        padding: 0x50,
        flags: 0,
        length: length as u32,
        offset,
        file_id: FileId::SENTINEL,
        minimum_count: 0,
        channel: SMB2_CHANNEL_NONE,
        remaining_bytes: 0,
        read_channel_info: Vec::new(),
    };
    let close = CloseRequest {
        flags: 0,
        file_id: FileId::SENTINEL,
    };
    let operations = [
        CompoundOp {
            command: Command::Create,
            body: &create,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Read,
            body: &read,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(credit_charge(length)),
        },
        CompoundOp {
            command: Command::Close,
            body: &close,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
    ];
    let responses = connection
        .execute_compound(&operations)
        .await
        .map_err(|error| read_wire_error(smb_error(error, 0), offset, None))?;
    let OpenCompoundResponseState {
        file_id,
        operation_responses,
        close_result,
        cardinality_error,
        ..
    } = decode_open_compound_responses(
        responses,
        operations.len(),
        |response| {
            require_status(response, Command::Create, 0)?;
            CreateResponse::unpack(&mut ReadCursor::new(&response.body))
                .map(|opened| opened.file_id)
                .map_err(|error| smb_error(error, 0))
        },
        |response| {
            require_status(response, Command::Close, 0).map_err(|cause| wire_failure(cause, None))
        },
        |error| smb_error(error, 0),
    )
    .map_err(|error| read_wire_error(error, offset, None))?;
    // A missing CLOSE does not invalidate the already-validated READ slot.
    // Still return a structural error, carrying those confirmed bytes.
    let data = match operation_responses.into_iter().next() {
        Some(Ok(response)) => decode_read_response(&response, length, 0),
        Some(Err(error)) => Err(smb_error(error, 0)),
        None => Err(cardinality_error
            .clone()
            .unwrap_or_else(|| compound_cardinality_error(1, operations.len()))),
    };
    let close = close_compound_on_connection(&tree, file_id, close_result, deferred_close).await;
    finish_read_result(
        data.map_err(|error| read_wire_error(error, offset, None)),
        close,
        cardinality_error,
        offset,
        length,
    )
}

pub(super) fn decode_write_flush(
    operation_responses: Vec<Result<smb2::client::Frame, SmbError>>,
    requested_length: usize,
) -> OpResult<usize> {
    let mut responses = operation_responses.into_iter();
    let written = (|| {
        let response = responses
            .next()
            .ok_or_else(|| compound_cardinality_error(1, 4))?
            .map_err(|error| smb_error(error, 0))?;
        require_status(&response, Command::Write, 0)?;
        let count = WriteResponse::unpack(&mut ReadCursor::new(&response.body))
            .map_err(|error| smb_error(error, 0))?
            .count as usize;
        if count > requested_length {
            return Err(VfError::invalid_reply(
                None,
                "SMB server reported writing more bytes than requested",
            ));
        }
        Ok(count)
    })()
    .map_err(|cause| write_wire_error(cause, 0));
    // Decode WRITE before looking at FLUSH, so missing/failed later replies
    // cannot erase an already acknowledged byte count.
    written.and_then(|written| {
        let flush = (|| {
            let response = responses
                .next()
                .ok_or_else(|| compound_cardinality_error(2, 4))?
                .map_err(|error| smb_error(error, 0))?;
            require_status(&response, Command::Flush, 0)
        })();
        flush
            .map(|()| written)
            .map_err(|cause| write_wire_error(cause, written))
    })
}

pub(super) async fn concurrent_compound_write(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
    offset: u64,
    data: Vec<u8>,
) -> CompoundOutcome<usize, OpError> {
    let mut deferred_close = None;
    let result = concurrent_compound_write_inner(
        connection,
        tree,
        create,
        offset,
        data,
        &mut deferred_close,
    )
    .await;
    CompoundOutcome {
        result,
        deferred_close,
    }
}

pub(super) async fn concurrent_compound_write_inner(
    connection: Connection,
    tree: Arc<ShareSession>,
    create: CreateRequest,
    offset: u64,
    data: Vec<u8>,
    deferred_close: &mut Option<CloseState>,
) -> OpResult<usize> {
    let tree_id = tree.tree_id;
    checked_offset(offset, data.len() as u64, 0).map_err(OpError::not_attempted)?;
    let write = WriteRequest {
        data_offset: 0x70,
        offset,
        file_id: FileId::SENTINEL,
        channel: 0,
        remaining_bytes: 0,
        write_channel_info_offset: 0,
        write_channel_info_length: 0,
        flags: 0,
        data,
    };
    let flush = FlushRequest {
        file_id: FileId::SENTINEL,
    };
    let close = CloseRequest {
        flags: 0,
        file_id: FileId::SENTINEL,
    };
    let operations = [
        CompoundOp {
            command: Command::Create,
            body: &create,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Write,
            body: &write,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(credit_charge(write.data.len())),
        },
        CompoundOp {
            command: Command::Flush,
            body: &flush,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Close,
            body: &close,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
    ];
    let responses = connection
        .execute_compound(&operations)
        .await
        .map_err(|error| write_wire_error(smb_error(error, 0), 0))?;
    let OpenCompoundResponseState {
        file_id,
        operation_responses,
        close_result,
        cardinality_error,
        ..
    } = decode_open_compound_responses(
        responses,
        operations.len(),
        |response| {
            require_status(response, Command::Create, 0)?;
            CreateResponse::unpack(&mut ReadCursor::new(&response.body))
                .map(|opened| opened.file_id)
                .map_err(|error| smb_error(error, 0))
        },
        |response| {
            require_status(response, Command::Close, 0).map_err(|cause| wire_failure(cause, None))
        },
        |error| smb_error(error, 0),
    )
    .map_err(|cause| write_wire_error(cause, 0))?;
    let write_result = decode_write_flush(operation_responses, write.data.len());
    let close_result =
        close_compound_on_connection(&tree, file_id, close_result, deferred_close).await;
    finish_write_result(write_result, close_result, cardinality_error)
}

pub(super) fn format_dfs_wire_path(server: &str, share: &str, encoded_path: &str) -> String {
    // Tree.server is already a hostname/IP without a port. Keep IPv6 colons
    // intact: the DFS server strips exactly the server and share components.
    if encoded_path.is_empty() {
        format!("{server}\\{share}")
    } else {
        format!("{server}\\{share}\\{encoded_path}")
    }
}

pub(super) fn normalize_server(server: &str) -> String {
    if server.starts_with('[') {
        if server.ends_with(']') {
            format!("{server}:445")
        } else {
            server.to_owned()
        }
    } else if server.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{server}]:445")
    } else if server
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        server.to_owned()
    } else {
        format!("{server}:445")
    }
}

pub(super) fn checked_read_data(
    data: Vec<u8>,
    requested: usize,
    index: usize,
) -> VfResult<Vec<u8>> {
    if data.len() > requested {
        return Err(VfError::transport_with_kind(
            Some(index),
            vfsi_core::TransportKind::InvalidReply,
            format!(
                "SMB server returned {} READ bytes for a {requested}-byte request",
                data.len()
            ),
        ));
    }
    Ok(data)
}

pub(super) fn decode_read_response(
    frame: &smb2::client::Frame,
    requested: usize,
    index: usize,
) -> VfResult<Vec<u8>> {
    require_command(frame, Command::Read, index)?;
    if frame.header.status == NtStatus::END_OF_FILE {
        return Ok(Vec::new());
    }
    require_status(frame, Command::Read, index)?;
    let response = ReadResponse::unpack(&mut ReadCursor::new(&frame.body))
        .map_err(|error| smb_error(error, index))?;
    checked_read_data(response.data, requested, index)
}

pub(super) fn credit_charge(bytes: usize) -> u16 {
    let charge = bytes.max(1).div_ceil(65_536);
    u16::try_from(charge).unwrap_or(u16::MAX).max(1)
}

pub(super) fn require_command(
    frame: &smb2::client::Frame,
    command: Command,
    index: usize,
) -> VfResult<()> {
    if frame.header.command != command {
        return Err(VfError::invalid_reply(
            index,
            format!(
                "SMB response command {:?}, expected {command:?}",
                frame.header.command
            ),
        ));
    }
    Ok(())
}

pub(super) fn require_status(
    frame: &smb2::client::Frame,
    command: Command,
    index: usize,
) -> VfResult<()> {
    require_command(frame, command, index)?;
    if frame.header.status == NtStatus::SUCCESS {
        Ok(())
    } else {
        Err(smb_error(
            SmbError::Protocol {
                status: frame.header.status,
                command,
            },
            index,
        ))
    }
}

pub(super) struct OpenCompoundResponseState<T, E> {
    pub(super) file_id: FileId,
    pub(super) create_response: T,
    pub(super) operation_responses: Vec<Result<T, E>>,
    pub(super) close_result: OpResult<()>,
    pub(super) cardinality_error: Option<VfError>,
}

/// Keep a successful CREATE response and its FileId even when a later waiter
/// fails or the compound response vector is truncated. Callers can then issue
/// retain cleanup ownership without replaying an uncertain CLOSE.
pub(super) fn decode_open_compound_responses<T, E>(
    responses: Vec<Result<T, E>>,
    expected: usize,
    parse_create: impl FnOnce(&T) -> VfResult<FileId>,
    close_result: impl Fn(&T) -> OpResult<()>,
    mut map_error: impl FnMut(E) -> VfError,
) -> VfResult<OpenCompoundResponseState<T, E>> {
    let response_count = responses.len();
    let mut responses = responses;
    if expected < 2 {
        return Err(VfError::transport(
            None,
            "SMB temporary-handle compound must include CREATE and CLOSE",
        ));
    }
    if responses.is_empty() {
        return Err(compound_cardinality_error(response_count, expected));
    }
    let create_response = responses.remove(0).map_err(&mut map_error)?;
    let file_id = parse_create(&create_response)?;

    let operation_count = expected - 2;
    // A short response vector is a prefix, so its final response cannot be
    // assumed to be CLOSE. Only trust the expected CLOSE slot when present.
    let close_result = if responses.len() > operation_count {
        match responses.remove(operation_count) {
            Ok(response) => close_result(&response),
            Err(error) => Err(wire_failure(map_error(error), None)),
        }
    } else {
        Err(OpError::unknown(
            compound_cardinality_error(response_count, expected),
            None,
        ))
    };
    // Reuse the response vector's allocation rather than allocating another
    // Vec for the middle operations. Extra responses are rejected below.
    responses.truncate(operation_count);
    let cardinality_error =
        (response_count != expected).then(|| compound_cardinality_error(response_count, expected));

    Ok(OpenCompoundResponseState {
        file_id,
        create_response,
        operation_responses: responses,
        close_result,
        cardinality_error,
    })
}

pub(super) fn compound_cardinality_error(actual: usize, expected: usize) -> VfError {
    VfError::invalid_reply(
        None,
        format!("SMB compound returned {actual} responses, expected {expected}"),
    )
}

#[cfg(test)]
pub(super) fn response_succeeded<T, E>(
    response: Option<&Result<T, E>>,
    is_success: impl FnOnce(&T) -> bool,
) -> bool {
    response.is_some_and(|response| response.as_ref().is_ok_and(is_success))
}

pub(super) fn collect_compound(
    responses: Vec<Result<smb2::client::Frame, SmbError>>,
    expected: usize,
) -> VfResult<Vec<smb2::client::Frame>> {
    if responses.len() != expected {
        return Err(compound_cardinality_error(responses.len(), expected));
    }
    responses
        .into_iter()
        .map(|result| result.map_err(|error| smb_error(error, 0)))
        .collect()
}

pub(super) fn smb_error(error: SmbError, index: impl Into<Option<usize>>) -> VfError {
    let index = index.into();
    if let SmbError::DfsReferralRequired { path }
    | SmbError::DfsTargetNotAllowed { path, .. }
    | SmbError::DfsTargetUnreachable { path, .. } = &error
    {
        return VfError::smb(index, NtStatus::PATH_NOT_COVERED.0)
            .with_context("SMB DFS referral", path);
    }
    if let Some(status) = error.status() {
        return VfError::smb(index, status.0).with_message(error.to_string());
    }
    use vfsi_core::TransportKind as T;
    let kind = match error.kind() {
        SmbErrorKind::TimedOut => T::Timeout,
        SmbErrorKind::ConnectionLost | SmbErrorKind::SessionExpired => T::Connection,
        SmbErrorKind::AuthRequired
        | SmbErrorKind::SigningRequired
        | SmbErrorKind::SignatureInvalid
        | SmbErrorKind::NegotiateValidationFailed => T::Auth,
        SmbErrorKind::InvalidData => T::InvalidReply,
        SmbErrorKind::Unsupported => {
            return VfError::unsupported(index).with_message(error.to_string());
        }
        _ => T::Other,
    };
    VfError::transport_with_kind(index, kind, error.to_string())
}

pub(super) fn validate_create_mode(flags: i32, mode: u32, index: usize) -> VfResult<()> {
    if flags & libc::O_CREAT != 0 && mode & 0o7777 != 0o666 {
        return Err(VfError::unsupported(index));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn validate_open_modes(flags: &[i32], modes: &[u32]) -> VfRes {
    for (index, (&flags, &mode)) in flags.iter().zip(modes).enumerate() {
        validate_create_mode(flags, mode, index)?;
    }
    Ok(())
}

pub(super) fn validate_mkdir_mode(attrs: &VfAttrs) -> VfRes {
    if attrs.masks.contains(AttrMask::MODE) && attrs.mode & 0o7777 != 0o777 {
        Err(VfError::unsupported(None))
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn validate_mkdir_modes(dirs: &[VfAttrs]) -> VfRes {
    for (index, dir) in dirs.iter().enumerate() {
        validate_mkdir_mode(dir).map_err(|error| error.with_index(index))?;
    }
    Ok(())
}

pub(super) fn filetime_parts(time: FileTime) -> (i64, u32) {
    const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;
    const TICKS_PER_SECOND: u64 = 10_000_000;
    if time.0 >= UNIX_EPOCH_FILETIME {
        let ticks = time.0 - UNIX_EPOCH_FILETIME;
        (
            (ticks / TICKS_PER_SECOND) as i64,
            ((ticks % TICKS_PER_SECOND) * 100) as u32,
        )
    } else {
        let ticks_before_epoch = UNIX_EPOCH_FILETIME - time.0;
        let whole_seconds = (ticks_before_epoch / TICKS_PER_SECOND) as i64;
        let fractional_ticks = ticks_before_epoch % TICKS_PER_SECOND;
        if fractional_ticks == 0 {
            (whole_seconds.saturating_neg(), 0)
        } else {
            (
                whole_seconds.saturating_neg().saturating_sub(1),
                1_000_000_000 - (fractional_ticks * 100) as u32,
            )
        }
    }
}
