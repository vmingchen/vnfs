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

#[cfg(test)]
mod benchmarks {
    use super::*;
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
        for (case, files, bytes) in [("small", 32, 4096), ("large", 8, 2 * 1024 * 1024)] {
            let source = fixture.0.join(case);
            std::fs::create_dir(&source).unwrap();
            for index in 0..files {
                let payload: Vec<_> = (0..bytes)
                    .map(|offset| ((offset + index) % 251) as u8)
                    .collect();
                std::fs::write(source.join(format!("source-{index:02}")), payload).unwrap();
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
                        backend.vcopy_data_impl(&pairs)
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
}
