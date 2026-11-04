use super::*;

impl SmbVecFs {
    /// Compare retained objects, including aliases across different path spellings.
    /// Only potentially overlapping forward copies need these identity queries.
    pub(super) fn copy_handles_alias(&mut self, files: [FileId; 2]) -> VfResult<bool> {
        let mut identities = [(0, 0); 2];
        for (identity, file_id) in identities.iter_mut().zip(files) {
            // Related compound operations share one handle on SMB servers.
            // Query each retained handle in its own compound, never mix them.
            let requests = [
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
            });
            let operations = requests.each_ref().map(|request| CompoundOp {
                command: Command::QueryInfo,
                body: request,
                tree_id: Some(self.tree.tree_id),
                credit_charge: CreditCharge(1),
            });
            let responses = self
                .runtime
                .block_on(self.client.connection_mut().execute_compound(&operations))
                .map_err(|error| smb_error(error, 0))?;
            let responses = collect_compound(responses, operations.len())?;
            let buffers: Vec<_> = responses
                .into_iter()
                .map(|response| {
                    require_status(&response, Command::QueryInfo, 0)?;
                    QueryInfoResponse::unpack(&mut ReadCursor::new(&response.body))
                        .map(|response| response.output_buffer)
                        .map_err(|error| smb_error(error, 0))
                })
                .collect::<VfResult<_>>()?;
            let index = buffers[0].get(..8).ok_or_else(|| {
                VfError::invalid_reply(None, "short SMB copy file-index response")
            })?;
            let volume = buffers[1]
                .get(8..12)
                .ok_or_else(|| VfError::invalid_reply(None, "short SMB copy volume response"))?;
            if index == [0; 8] || volume == [0; 4] {
                return Err(VfError::unsupported(0));
            }
            // File indices are volume-local; a share can span several volumes.
            *identity = (
                u64::from_le_bytes(index.try_into().expect("eight-byte slice")),
                u32::from_le_bytes(volume.try_into().expect("four-byte slice")),
            );
        }
        Ok(identities[0] == identities[1])
    }

    pub(super) fn copy_client_side(&mut self, pair: &ExtentPair) -> VfResult<()> {
        let source = self.local_path_string(&pair.src_path)?;
        let destination = self.local_path_string(&pair.dst_path)?;
        let (src_id, src_size) = self.raw_open(&source, libc::O_RDONLY)?;
        let (dst_id, _) = match self.raw_open(&destination, libc::O_WRONLY | libc::O_CREAT) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.close_temporary_handle(src_id);
                return Err(error);
            }
        };
        let available = src_size.saturating_sub(pair.src_offset);
        let length = pair.length.unwrap_or(available).min(available);
        let mut copied = 0u64;
        let result = (|| {
            if pair.dst_offset > pair.src_offset
                && pair.dst_offset - pair.src_offset < length
                && self.copy_handles_alias([src_id, dst_id])?
            {
                // Forward chunking would overwrite bytes a later read still
                // needs. Reject before writing, flushing or truncating either alias.
                return Err(VfError::from_kind(0, std::io::ErrorKind::InvalidInput));
            }
            while copied < length {
                let wanted = (length - copied).min(1 << 20) as usize;
                let src_offset = checked_offset(pair.src_offset, copied, 0)?;
                let data = self.raw_read(src_id, src_offset, wanted)?;
                if data.is_empty() {
                    break;
                }
                let dst_offset = checked_offset(pair.dst_offset, copied, 0)?;
                self.raw_write(dst_id, dst_offset, &data)?;
                copied = checked_offset(copied, data.len() as u64, 0)?;
            }
            self.set_size_handle(dst_id, checked_offset(pair.dst_offset, copied, 0)?)?;
            self.raw_flush(dst_id)
        })();
        let dst_close = self.close_temporary_handle(dst_id);
        let src_close = self.close_temporary_handle(src_id);
        result.and(dst_close).and(src_close)
    }
}
