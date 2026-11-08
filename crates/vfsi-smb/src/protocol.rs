use super::*;

pub(super) struct CompoundOutcome<T> {
    pub(super) result: VfResult<T>,
    pub(super) deferred_close: Option<FileId>,
}

pub(super) fn retain_failed_close_slot(
    deferred_close: &mut Option<FileId>,
    file_id: FileId,
    result: VfResult<()>,
) -> VfResult<()> {
    if result.is_err() {
        *deferred_close = Some(file_id);
    }
    result
}

pub(super) fn queue_deferred_closes<T>(
    outcomes: &[CompoundOutcome<T>],
    deferred: &mut Vec<FileId>,
) {
    deferred.extend(outcomes.iter().filter_map(|outcome| outcome.deferred_close));
}

pub(super) fn retain_failed_close(
    deferred: &mut Vec<FileId>,
    file_id: FileId,
    result: VfResult<()>,
) -> VfResult<()> {
    if result.is_err() {
        deferred.push(file_id);
    }
    result
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

pub(super) fn delete_entry_create_request(mut create: CreateRequest) -> CreateRequest {
    create.desired_access = FileAccessMask::new(
        FileAccessMask::DELETE | FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE,
    );
    // Open the final reparse point itself so deleting a symlink cannot delete
    // its target. Leave file/directory type unconstrained to cover dangling and
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
    tree_id: TreeId,
    create: CreateRequest,
) -> CompoundOutcome<()> {
    let mut deferred_close = None;
    let result = async {
        let disposition = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: FILE_DISPOSITION_INFORMATION,
            additional_information: 0,
            file_id: FileId::SENTINEL,
            buffer: vec![1], // DeletePending = true (MS-FSCC 2.4.11).
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
                body: &disposition,
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
            close_succeeded,
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
            |response| response.header.status == NtStatus::SUCCESS,
            |error| smb_error(error, 0),
        )?;
        let close_result = close_compound_on_connection(
            &connection,
            tree_id,
            file_id,
            close_succeeded,
            &mut deferred_close,
        )
        .await;

        if let Some(error) = cardinality_error {
            return Err(error);
        }
        let delete_response = operation_responses
            .into_iter()
            .next()
            .expect("complete compound contains SET_INFO response")
            .map_err(|error| smb_error(error, 0))?;
        require_status(&delete_response, Command::SetInfo, 0)?;
        close_result
    }
    .await;
    CompoundOutcome {
        result,
        deferred_close,
    }
}

pub(super) async fn probe_reparse_point(
    connection: Connection,
    tree_id: TreeId,
    create: CreateRequest,
) -> CompoundOutcome<bool> {
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
            close_succeeded,
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
            |response| response.header.status == NtStatus::SUCCESS,
            |error| smb_error(error, 0),
        )?;
        let is_reparse_point = CreateResponse::unpack(&mut ReadCursor::new(&create_response.body))
            .map(|opened| opened.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
            .map_err(|error| smb_error(error, 0));
        let close_result = close_compound_on_connection(
            &connection,
            tree_id,
            file_id,
            close_succeeded,
            &mut deferred_close,
        )
        .await;
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
    tree_id: TreeId,
    create: CreateRequest,
) -> CompoundOutcome<u64> {
    let mut deferred_close = None;
    let result =
        concurrent_file_identity_inner(connection, tree_id, create, &mut deferred_close).await;
    CompoundOutcome {
        result,
        deferred_close,
    }
}

pub(super) async fn concurrent_file_identity_inner(
    connection: Connection,
    tree_id: TreeId,
    create: CreateRequest,
    deferred_close: &mut Option<FileId>,
) -> VfResult<u64> {
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
        close_succeeded,
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
        |response| response.header.status == NtStatus::SUCCESS,
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
    let close = close_compound_on_connection(
        &connection,
        tree_id,
        file_id,
        close_succeeded,
        deferred_close,
    )
    .await;
    match identity {
        Ok(identity) => close.map(|()| identity),
        Err(error) => {
            let _ = close;
            Err(error)
        }
    }
}

pub(super) async fn close_on_connection_result(
    connection: &Connection,
    tree_id: TreeId,
    file_id: FileId,
) -> VfRes {
    let request = CloseRequest { flags: 0, file_id };
    let frame = connection
        .execute(Command::Close, &request, Some(tree_id))
        .await
        .map_err(|error| smb_error(error, 0))?;
    require_status(&frame, Command::Close, 0)
}

pub(super) async fn close_compound_on_connection(
    connection: &Connection,
    tree_id: TreeId,
    file_id: FileId,
    compound_close_succeeded: bool,
    deferred_close: &mut Option<FileId>,
) -> VfResult<()> {
    let fallback = if compound_close_succeeded {
        Ok(())
    } else {
        close_on_connection_result(connection, tree_id, file_id).await
    };
    finish_compound_close(deferred_close, file_id, compound_close_succeeded, fallback)
}

pub(super) fn finish_compound_close(
    deferred_close: &mut Option<FileId>,
    file_id: FileId,
    compound_close_succeeded: bool,
    fallback: VfResult<()>,
) -> VfResult<()> {
    if compound_close_succeeded {
        Ok(())
    } else {
        retain_failed_close_slot(deferred_close, file_id, fallback)
    }
}

pub(super) async fn concurrent_compound_read(
    connection: Connection,
    tree_id: TreeId,
    create: CreateRequest,
    offset: u64,
    length: usize,
) -> CompoundOutcome<Vec<u8>> {
    let mut deferred_close = None;
    let result = concurrent_compound_read_inner(
        connection,
        tree_id,
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
    tree_id: TreeId,
    create: CreateRequest,
    offset: u64,
    length: usize,
    deferred_close: &mut Option<FileId>,
) -> VfResult<Vec<u8>> {
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
        .map_err(|error| smb_error(error, 0))?;
    let OpenCompoundResponseState {
        file_id,
        operation_responses,
        close_succeeded,
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
        |response| response.header.status == NtStatus::SUCCESS,
        |error| smb_error(error, 0),
    )?;
    let data = if let Some(error) = cardinality_error {
        Err(error)
    } else {
        match operation_responses.into_iter().next() {
            Some(Ok(response)) if response.header.status == NtStatus::END_OF_FILE => Ok(Vec::new()),
            Some(Ok(response)) => require_status(&response, Command::Read, 0).and_then(|()| {
                ReadResponse::unpack(&mut ReadCursor::new(&response.body))
                    .map_err(|error| smb_error(error, 0))
                    .and_then(|response| checked_read_data(response.data, length, 0))
            }),
            Some(Err(error)) => Err(smb_error(error, 0)),
            None => Err(compound_cardinality_error(1, operations.len())),
        }
    };
    let close = close_compound_on_connection(
        &connection,
        tree_id,
        file_id,
        close_succeeded,
        deferred_close,
    )
    .await;
    match data {
        Ok(data) => close.map(|()| data),
        Err(error) => {
            let _ = close;
            Err(error)
        }
    }
}

pub(super) async fn concurrent_compound_write(
    connection: Connection,
    tree_id: TreeId,
    create: CreateRequest,
    offset: u64,
    data: Vec<u8>,
) -> CompoundOutcome<usize> {
    let mut deferred_close = None;
    let result = concurrent_compound_write_inner(
        connection,
        tree_id,
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
    tree_id: TreeId,
    create: CreateRequest,
    offset: u64,
    data: Vec<u8>,
    deferred_close: &mut Option<FileId>,
) -> VfResult<usize> {
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
        .map_err(|error| smb_error(error, 0))?;
    let OpenCompoundResponseState {
        file_id,
        operation_responses,
        close_succeeded,
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
        |response| response.header.status == NtStatus::SUCCESS,
        |error| smb_error(error, 0),
    )?;
    let requested_length = write.data.len();
    let write_result = if let Some(error) = cardinality_error {
        Err(error)
    } else {
        (|| {
            let mut responses = operation_responses.into_iter();
            let write = responses
                .next()
                .expect("complete compound contains WRITE response")
                .map_err(|error| smb_error(error, 0))?;
            let flush = responses
                .next()
                .expect("complete compound contains FLUSH response")
                .map_err(|error| smb_error(error, 0))?;
            let written = require_status(&write, Command::Write, 0).and_then(|()| {
                WriteResponse::unpack(&mut ReadCursor::new(&write.body))
                    .map(|response| response.count as usize)
                    .map_err(|error| smb_error(error, 0))
                    .and_then(|count| {
                        if count > requested_length {
                            Err(VfError::invalid_reply(
                                None,
                                "SMB server reported writing more bytes than requested",
                            ))
                        } else {
                            Ok(count)
                        }
                    })
            })?;
            require_status(&flush, Command::Flush, 0)?;
            Ok(written)
        })()
    };
    let close_result = close_compound_on_connection(
        &connection,
        tree_id,
        file_id,
        close_succeeded,
        deferred_close,
    )
    .await;
    match write_result {
        Ok(written) => close_result.map(|()| written),
        Err(error) => {
            let _ = close_result;
            Err(error)
        }
    }
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

pub(super) fn credit_charge(bytes: usize) -> u16 {
    let charge = bytes.max(1).div_ceil(65_536);
    u16::try_from(charge).unwrap_or(u16::MAX).max(1)
}

pub(super) fn require_status(
    frame: &smb2::client::Frame,
    command: Command,
    index: usize,
) -> VfResult<()> {
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
    pub(super) close_succeeded: bool,
    pub(super) cardinality_error: Option<VfError>,
}

/// Keep a successful CREATE response and its FileId even when a later waiter
/// fails or the compound response vector is truncated. Callers can then issue
/// a fallback CLOSE instead of leaking the temporary handle.
pub(super) fn decode_open_compound_responses<T, E>(
    responses: Vec<Result<T, E>>,
    expected: usize,
    parse_create: impl FnOnce(&T) -> VfResult<FileId>,
    close_succeeded: impl Fn(&T) -> bool,
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
    let close_succeeded = if response_count >= expected {
        response_succeeded(responses.get(operation_count), close_succeeded)
    } else {
        false
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
        close_succeeded,
        cardinality_error,
    })
}

pub(super) fn compound_cardinality_error(actual: usize, expected: usize) -> VfError {
    VfError::invalid_reply(
        None,
        format!("SMB compound returned {actual} responses, expected {expected}"),
    )
}

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

pub(super) fn validate_open_modes(flags: &[i32], modes: &[u32]) -> VfRes {
    for (index, (&flags, &mode)) in flags.iter().zip(modes).enumerate() {
        validate_create_mode(flags, mode, index)?;
    }
    Ok(())
}

pub(super) fn validate_mkdir_modes(dirs: &[VfAttrs]) -> VfRes {
    for (index, dir) in dirs.iter().enumerate() {
        if dir.masks.contains(AttrMask::MODE) && dir.mode & 0o7777 != 0o777 {
            return Err(VfError::unsupported(index));
        }
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
