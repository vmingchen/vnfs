//! Controlled new-destination copy experiment; production copy hooks are unchanged.
use super::*;
use crate::contracts::{validate_read_results, validate_write_results};
use vfsi_core::IndexedError;

const FILES: usize = 8;
const BYTES: usize = 1024 * 1024;

/// Copy into exclusively created destinations using bounded native I/O waves.
/// Available only through test-support for protocol/performance experiments.
/// Sources must be regular files and exist before dispatch. Destination paths
/// must not exist; CREATE_NEW proves they cannot alias any retained source.
/// There is no fallback, rollback, or retry after a dispatched I/O failure.
/// Final sizes and optional durability follow the supplied backend policy.
pub fn copy_new_files_batched<F: VectorBackend + ?Sized>(
    backend: &mut F,
    pairs: &[ExtentPair],
    creation_mode: u32,
    durability: Option<vfsi_core::api::SyncMode>,
) -> Result<(), IndexedError> {
    for (chunk, pairs) in pairs.chunks(FILES).enumerate() {
        let base = chunk * FILES;
        let sources: Vec<_> = pairs.iter().map(|p| p.src_path.as_path()).collect();
        let destinations: Vec<_> = pairs.iter().map(|p| p.dst_path.as_path()).collect();
        let sources = crate::backend::helpers::open_raw_batch(
            backend,
            &sources,
            &[libc::O_RDONLY; FILES][..sources.len()],
            &[0; FILES][..sources.len()],
        )
        .map_err(|e| e.map_index(|i| base + i))?;
        let destinations = match crate::backend::helpers::open_raw_batch(
            backend,
            &destinations,
            &[libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL; FILES][..destinations.len()],
            &[creation_mode; FILES][..destinations.len()],
        ) {
            Ok(files) => files,
            Err(error) => {
                for file in &sources {
                    let _ = backend.close_impl(file);
                }
                return Err(error.map_index(|i| base + i));
            }
        };
        let operation = (|| {
            let sizes = copy_handles(backend, pairs, &sources, &destinations)?;
            let updates: Vec<_> = destinations
                .iter()
                .zip(sizes)
                .map(|(file, size)| SetAttrsOp::new(Target::File(file)).len(size))
                .collect();
            vfsi_core::internal::collect_vec_result(
                backend.vsetattrs_impl(&updates),
                updates.len(),
            )?;
            if let Some(durability) = durability {
                crate::internal::collect_vec_result(
                    backend.vfsync_impl(&destinations, durability),
                    destinations.len(),
                )?;
            }
            Ok(())
        })();
        let mut result = operation.map_err(|e: IndexedError| e.map_index(|i| base + i));
        // One close attempt per owned descriptor, including after data errors.
        // Do not blindly replay a failed vector CLOSE with ambiguous progress.
        for (i, file) in sources.iter().chain(&destinations).enumerate() {
            let close = backend
                .close_impl(file)
                .map_err(|e| IndexedError::at(base + i % pairs.len(), e));
            if result.is_ok() {
                result = close;
            }
        }
        result?;
    }
    Ok(())
}

fn remap(error: IndexedError, active: &[usize]) -> IndexedError {
    match error.index() {
        Some(index) => match active.get(index) {
            Some(&owner) => error.map_index(|_| owner),
            None => IndexedError::unattributed(vfsi_core::internal::errors::invalid_reply(
                "invalid copy experiment result index",
            )),
        },
        None => error,
    }
}

fn copy_handles<F: VectorBackend + ?Sized>(
    backend: &mut F,
    pairs: &[ExtentPair],
    sources: &[FileOperand],
    destinations: &[FileOperand],
) -> Result<Vec<u64>, IndexedError> {
    if pairs.len() > FILES || pairs.len() != sources.len() || pairs.len() != destinations.len() {
        return Err(IndexedError::unattributed(std::io::Error::from(
            std::io::ErrorKind::InvalidInput,
        )));
    }
    let mut totals = vec![0u64; pairs.len()];
    let mut active: Vec<_> = (0..pairs.len())
        .filter(|&i| pairs[i].length != Some(0))
        .collect();
    while !active.is_empty() {
        let chunk_size = BYTES / active.len();
        let reads: Vec<_> = active
            .iter()
            .map(|&i| {
                let offset = pairs[i].src_offset.checked_add(totals[i]).ok_or_else(|| {
                    IndexedError::at(i, std::io::Error::from_raw_os_error(libc::EOVERFLOW))
                })?;
                let count = pairs[i]
                    .length
                    .map_or(chunk_size as u64, |n| n - totals[i])
                    .min(chunk_size as u64) as usize;
                Ok(ReadOp::at(sources[i].clone(), offset, count))
            })
            .collect::<Result<_, IndexedError>>()?;
        let data = crate::internal::collect_vec_result(backend.vread_impl(&reads), reads.len())
            .map_err(|e| remap(e, &active))?;
        validate_read_results("copy experiment", &reads, &data)
            .map_err(|error| remap(error, &active))?;
        let mut pending: Vec<_> = data
            .iter()
            .enumerate()
            .filter_map(|(slot, r)| (!r.data.is_empty()).then_some((slot, 0usize)))
            .collect();
        while !pending.is_empty() {
            let owners: Vec<_> = pending.iter().map(|&(slot, _)| active[slot]).collect();
            let writes: Vec<_> = pending
                .iter()
                .map(|&(slot, sent)| {
                    let i = active[slot];
                    let offset = pairs[i]
                        .dst_offset
                        .checked_add(totals[i])
                        .and_then(|n| n.checked_add(sent as u64))
                        .ok_or_else(|| {
                            IndexedError::at(i, std::io::Error::from_raw_os_error(libc::EOVERFLOW))
                        })?;
                    offset
                        .checked_add((data[slot].data.len() - sent) as u64)
                        .ok_or_else(|| {
                            IndexedError::at(i, std::io::Error::from_raw_os_error(libc::EOVERFLOW))
                        })?;
                    Ok(WriteOp::new(
                        &destinations[i],
                        VfOffset::At(offset),
                        &data[slot].data[sent..],
                    ))
                })
                .collect::<Result<_, IndexedError>>()?;
            let written =
                crate::internal::collect_vec_result(backend.vwrite_impl(&writes), writes.len())
                    .map_err(|e| remap(e, &owners))?;
            validate_write_results("copy experiment", &writes, &written)
                .map_err(|error| remap(error, &owners))?;
            let mut next = Vec::new();
            for ((slot, sent), result) in pending.into_iter().zip(written) {
                if result.bytes_written == 0 {
                    return Err(IndexedError::at(
                        active[slot],
                        std::io::Error::from_raw_os_error(libc::EIO),
                    ));
                }
                let sent = sent + result.bytes_written;
                if sent < data[slot].data.len() {
                    next.push((slot, sent));
                }
            }
            pending = next;
        }
        let mut next = Vec::new();
        for (&i, result) in active.iter().zip(data) {
            totals[i] = totals[i]
                .checked_add(result.data.len() as u64)
                .ok_or_else(|| {
                    IndexedError::at(i, std::io::Error::from_raw_os_error(libc::EOVERFLOW))
                })?;
            if !result.eof && pairs[i].length.is_none_or(|n| totals[i] < n) {
                next.push(i);
            }
        }
        active = next;
    }
    pairs
        .iter()
        .zip(totals)
        .enumerate()
        .map(|(i, (pair, total))| {
            pair.dst_offset.checked_add(total).ok_or_else(|| {
                IndexedError::at(i, std::io::Error::from_raw_os_error(libc::EOVERFLOW))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::HandleBackend;
    use vfsi_core::IndexedError;

    struct Memory {
        sources: Vec<Vec<u8>>,
        destinations: Vec<Vec<u8>>,
        read_limit: usize,
        write_limit: usize,
        write_failure: bool,
        malformed_read: bool,
        write_calls: usize,
        read_widths: Vec<usize>,
    }
    impl Memory {
        fn new(sources: Vec<Vec<u8>>) -> Self {
            Self {
                destinations: vec![Vec::new(); sources.len()],
                sources,
                read_limit: usize::MAX,
                write_limit: usize::MAX,
                write_failure: false,
                malformed_read: false,
                write_calls: 0,
                read_widths: Vec::new(),
            }
        }
        fn run(&mut self, pairs: &[ExtentPair]) -> Result<Vec<u64>, IndexedError> {
            let sources: Vec<_> = (0..pairs.len())
                .map(|i| FileOperand::from_fd(i as i32))
                .collect();
            let destinations: Vec<_> = (0..pairs.len())
                .map(|i| FileOperand::from_fd(10 + i as i32))
                .collect();
            copy_handles(self, pairs, &sources, &destinations)
        }
    }
    impl HandleBackend for Memory {
        fn open_impl(&mut self, _: &OpenOp) -> VfResult<FileOperand> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn vclose_impl(&mut self, files: &[vfsi_core::FileOperand]) -> vfsi_core::VecResult<()> {
            Ok(files
                .iter()
                .map(|file| {
                    self.close_impl(file)
                        .map_err(|error| vfsi_core::OpError::unknown(error, None))
                })
                .collect())
        }

        fn close_impl(&mut self, _: &FileOperand) -> VfResult<()> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn read_impl(&mut self, _: &ReadOp) -> VfResult<ReadOutput> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn write_impl(&mut self, _: WriteOp<&FileOperand, &[u8]>) -> VfResult<WriteOutput> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn seek_impl(&mut self, _: &FileOperand, _: std::io::SeekFrom) -> VfResult<u64> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn metadata_impl(
            &mut self,
            _: Target<'_, FileOperand>,
            _: vfsi_core::api::AttrsOptions,
        ) -> VfResult<VfAttrs> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn set_attributes_impl(&mut self, _: &SetAttrsOp<Target<'_, FileOperand>>) -> VfRes {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
        fn vfsync_impl(
            &mut self,
            files: &[FileOperand],
            _mode: crate::api::SyncMode,
        ) -> crate::VecResult<()> {
            Ok(files
                .iter()
                .map(|file| {
                    self.sync_data(file)
                        .map_err(|cause| crate::OpError::unknown(cause, None))
                })
                .collect())
        }
        fn sync_data(&mut self, _: &FileOperand) -> VfResult<()> {
            Ok(())
        }
    }
    impl VectorBackend for Memory {
        fn vopen_raw_impl(
            &mut self,
            paths: &[&std::path::Path],
            flags: &[i32],
            modes: &[u32],
        ) -> crate::VecResult<crate::FileOperand> {
            if paths.len() != flags.len() || paths.len() != modes.len() {
                return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
            }
            Ok(paths
                .iter()
                .zip(flags)
                .zip(modes)
                .map(|((path, flags), mode)| {
                    self.open_raw_impl(path, *flags, *mode).map_err(|cause| {
                        if vfsi_core::transport_kind(&cause).is_some() {
                            crate::OpError::unknown(cause, None)
                        } else {
                            crate::OpError::failed(cause, None)
                        }
                    })
                })
                .collect())
        }

        fn vread_impl(&mut self, reads: &[ReadOp]) -> crate::VecResult<ReadOutput> {
            let result: VfResult<Vec<ReadOutput>> = {
                assert!(reads.iter().map(|r| r.length).sum::<usize>() <= BYTES);
                self.read_widths.push(reads.len());
                Ok(reads
                    .iter()
                    .map(|r| {
                        let VfOffset::At(offset) = r.offset else {
                            panic!("positional read")
                        };
                        let source = &self.sources[r.file.fd().unwrap() as usize];
                        let start = (offset as usize).min(source.len());
                        let end = source.len().min(start + r.length.min(self.read_limit));
                        ReadOutput {
                            file: if self.malformed_read {
                                FileOperand::from_fd(999)
                            } else {
                                r.file.clone()
                            },
                            offset,
                            data: source[start..end].to_vec(),
                            eof: end == source.len(),
                        }
                    })
                    .collect())
            };
            result.map(|results| results.into_iter().map(Ok).collect())
        }
        fn vwrite_impl(
            &mut self,
            writes: &[WriteOp<&FileOperand, &[u8]>],
        ) -> crate::VecResult<WriteOutput> {
            self.write_calls += 1;
            if self.write_failure {
                // Simulate an acknowledged-unknown mutation before a lost reply.
                self.destinations[(writes[0].file().fd().unwrap() - 10) as usize]
                    .push(writes[0].data()[0]);
                return Ok(writes
                    .iter()
                    .enumerate()
                    .map(|(index, write)| {
                        if index == 0 {
                            let VfOffset::At(offset) = write.offset() else {
                                panic!("positional write")
                            };
                            Ok(WriteOutput {
                                file: (*write.file()).clone(),
                                offset,
                                bytes_written: 1,
                                stable: true,
                            })
                        } else {
                            Err(crate::OpError::unknown(
                                vfsi_core::internal::errors::transport(
                                    vfsi_core::TransportKind::Other,
                                    "lost copy write reply",
                                ),
                                None,
                            ))
                        }
                    })
                    .collect());
            }
            Ok(writes
                .iter()
                .map(|w| {
                    let VfOffset::At(offset) = w.offset() else {
                        panic!("positional write")
                    };
                    let n = w.data().len().min(self.write_limit);
                    let destination =
                        &mut self.destinations[(w.file().fd().unwrap() - 10) as usize];
                    destination.resize(destination.len().max(offset as usize + n), 0);
                    destination[offset as usize..offset as usize + n]
                        .copy_from_slice(&w.data()[..n]);
                    WriteOutput {
                        file: (*w.file()).clone(),
                        offset,
                        bytes_written: n,
                        stable: true,
                    }
                })
                .map(Ok)
                .collect())
        }
    }
    fn pairs(count: usize, length: Option<u64>) -> Vec<ExtentPair> {
        (0..count)
            .map(|i| {
                ExtentPair::from_os_paths(
                    Path::new(&format!("/s{i}")),
                    0,
                    Path::new(&format!("/d{i}")),
                    0,
                    length,
                )
            })
            .collect()
    }
    #[test]
    fn short_reads_and_confirmed_short_writes_keep_offsets_and_finite_ranges() {
        let mut memory = Memory::new(vec![
            b"0123456789abcdefgh".to_vec(),
            vec![],
            b"abcde".to_vec(),
        ]);
        memory.read_limit = 3;
        memory.write_limit = 2;
        let mut pairs = pairs(3, None);
        pairs[0].src_offset = 2;
        pairs[0].dst_offset = 4;
        pairs[0].length = Some(7);
        pairs[2].length = Some(0);
        assert_eq!(memory.run(&pairs).unwrap(), [11, 0, 0]);
        assert_eq!(memory.destinations[0], b"\0\0\0\x002345678");
        assert!(memory.destinations[1].is_empty() && memory.destinations[2].is_empty());
    }
    #[test]
    fn active_copy_waves_are_bounded_and_preserve_all_payloads() {
        let sources: Vec<_> = (0..8).map(|i| vec![i; 262_144 + 17]).collect();
        let mut memory = Memory::new(sources.clone());
        let sizes = memory.run(&pairs(8, None)).unwrap();
        assert_eq!(sizes, vec![262_144 + 17; 8]);
        assert_eq!(memory.destinations, sources);
        assert!(memory.read_widths.contains(&8));
    }
    #[test]
    fn lost_write_reply_keeps_original_index_and_is_never_replayed() {
        let mut memory = Memory::new(vec![b"abc".to_vec(); 4]);
        memory.write_failure = true;
        let mut pairs = pairs(4, None);
        pairs[0].length = Some(0);
        pairs[2].length = Some(0);
        let error = memory.run(&pairs).unwrap_err();
        assert!(vfsi_core::transport_kind(error.cause()).is_some());
        assert_eq!(error.index(), Some(3));
        assert_eq!(memory.write_calls, 1);
        assert_eq!(memory.destinations[1], b"a");
    }
    #[test]
    fn malformed_reads_and_zero_writes_fail_without_replaying_mutations() {
        let mut memory = Memory::new(vec![b"abc".to_vec()]);
        memory.malformed_read = true;
        assert!(
            vfsi_core::transport_kind(memory.run(&pairs(1, None)).unwrap_err().cause()).is_some()
        );
        assert_eq!(memory.write_calls, 0);
        memory.malformed_read = false;
        memory.write_limit = 0;
        assert_eq!(
            vfsi_core::internal::errors::to_errno(memory.run(&pairs(1, None)).unwrap_err().cause())
                as u32,
            ERR_IO
        );
        assert_eq!(memory.write_calls, 1);
    }
}
