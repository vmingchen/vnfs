//! Read dispatch, completion validation, and bounded streaming.

use super::*;

impl<F: HandleBackend> FsClient<F> {
    /// Stream one file using an explicit maximum chunk size.
    ///
    /// The callback runs without holding the backend lock, so it may use this
    /// client or drop other files owned by it. Return `Ok(std::ops::ControlFlow::Break(()))` to stop
    /// successfully. Callback errors propagate. At most one requested chunk is
    /// buffered at once, and the file closes on success, cancellation, or error.
    pub(crate) fn read_stream_with_options(
        &self,
        path: impl AsRef<Path>,
        options: StreamOptions,
        mut callback: impl FnMut(u64, &[u8]) -> VfResult<std::ops::ControlFlow<()>>,
    ) -> VfResult<StreamCompletion> {
        let chunk_size = options.chunk_size_bytes();
        let file = self.open(path)?;
        let raw_file = file.raw()?.clone();
        let operation = (|| -> VfResult<StreamCompletion> {
            let mut offset = 0u64;
            loop {
                let request = ReadOp::at(raw_file.clone(), offset, chunk_size);
                let result = {
                    let mut backend = self.lock()?;
                    let result = backend.read_impl(&request)?;
                    validate_read_results(
                        "read_stream",
                        std::slice::from_ref(&request),
                        std::slice::from_ref(&result),
                    )?;
                    result
                };
                let length = result.data.len();
                if !result.data.is_empty() && callback(offset, &result.data)?.is_break() {
                    return Ok(StreamCompletion::Stopped {
                        next_offset: offset
                            .checked_add(length as u64)
                            .ok_or_else(|| VfError::client(0, crate::ERR_INVAL))?,
                    });
                }
                if result.eof {
                    return Ok(StreamCompletion::Complete);
                }
                offset = offset
                    .checked_add(length as u64)
                    .ok_or_else(|| VfError::client(0, libc::EOVERFLOW as u32))?;
            }
        })();
        let close = file.close();
        match operation {
            Err(error) => Err(error),
            Ok(completion) => close.map(|()| completion),
        }
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Read an ordered vector with a 16 MiB aggregate request limit.
    /// Use [`vread_with_limit_native`](Self::vread_with_limit_native) to tune the limit or
    /// [`vread_into_native`](Self::vread_into_native) to provide bounded caller-owned buffers.
    #[doc(hidden)]
    pub fn vread_native(&self, requests: &[FsRead<'_, F>]) -> VfResult<Vec<FsReadResult>> {
        self.vread_with_limit_native(requests, self.limits.read_byte_limit())
    }

    /// Read an ordered vector with an explicit aggregate request limit.
    #[doc(hidden)]
    pub fn vread_with_limit_native(
        &self,
        requests: &[FsRead<'_, F>],
        max_total_bytes: usize,
    ) -> VfResult<Vec<FsReadResult>> {
        self.vread_with_limit_projected_native(requests, max_total_bytes, |request| request)
    }

    /// Backend adapter for opaque application requests; projection does not allocate.
    /// The projection must return the same embedded request on every invocation.
    #[doc(hidden)]
    pub fn vread_with_limit_projected_native<'b, T>(
        &self,
        requests: &[T],
        max_total_bytes: usize,
        project: impl for<'r> Fn(&'r T) -> &'r FsRead<'b, F>,
    ) -> VfResult<Vec<FsReadResult>>
    where
        F: 'b,
    {
        let mut requested = 0usize;
        for (index, request) in requests.iter().map(&project).enumerate() {
            requested = requested
                .checked_add(request.length)
                .ok_or_else(|| VfError::client(index, libc::EFBIG as u32))?;
            if requested > max_total_bytes {
                return Err(VfError::client(index, libc::EFBIG as u32)
                    .with_context("vread_native", request.file.path()));
            }
        }
        let reads = self.read_ops(requests, &project)?;
        let results = self.lock()?.vread_impl(&reads).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_native", request.file.path())
                })
        })?;
        if results.len() != requests.len() {
            return Err(wrong_result_count(
                "vread_native",
                requests.len(),
                results.len(),
            ));
        }
        validate_read_results("vread_native", &reads, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index).map(&project))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_native", request.file.path())
                })
        })?;
        Ok(results.into_iter().map(read_result).collect())
    }

    fn read_ops<'b, T>(
        &self,
        requests: &[T],
        project: impl for<'r> Fn(&'r T) -> &'r FsRead<'b, F>,
    ) -> VfResult<Vec<ReadOp>>
    where
        F: 'b,
    {
        let mut reads = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().map(&project).enumerate() {
            self.validate_owner(request.file, index)?;
            reads.push(ReadOp::new(
                request.file.raw()?.clone(),
                request.offset,
                request.length,
            ));
        }
        Ok(reads)
    }

    #[doc(hidden)]
    pub fn vread_into_native(
        &self,
        requests: &mut [FsReadInto<'_, F>],
    ) -> VfResult<Vec<FsReadIntoResult>> {
        self.vread_into_with_limit_native(requests, self.limits.read_byte_limit())
    }

    /// Read into caller storage with an explicit aggregate buffer budget.
    /// This also bounds allocation in copying fallback implementations.
    #[doc(hidden)]
    pub fn vread_into_with_limit_native(
        &self,
        requests: &mut [FsReadInto<'_, F>],
        max_bytes: usize,
    ) -> VfResult<Vec<FsReadIntoResult>> {
        let mut requested = 0usize;
        let mut reads = Vec::with_capacity(requests.len());
        for (index, request) in requests.iter().enumerate() {
            self.validate_owner(request.file, index)?;
            requested = requested
                .checked_add(request.buffer.len())
                .ok_or_else(|| VfError::client(index, libc::EFBIG as u32))?;
            if requested > max_bytes {
                return Err(VfError::client(index, libc::EFBIG as u32));
            }
            reads.push(ReadOp::new(
                request.file.raw()?.clone(),
                request.offset,
                request.buffer.len(),
            ));
        }
        let results = {
            let mut buffers: Vec<&mut [u8]> = requests
                .iter_mut()
                .map(|request| &mut *request.buffer)
                .collect();
            self.lock()?.vread_into_impl(&reads, &mut buffers)
        }
        .map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_into_native", request.file.path())
                })
        })?;
        validate_read_into_results("vread_into_native", &reads, &results).map_err(|error| {
            error
                .index()
                .and_then(|index| requests.get(index))
                .map_or(error.clone(), |request| {
                    error.with_context("vread_into_native", request.file.path())
                })
        })?;
        Ok(results
            .into_iter()
            .map(|result| FsReadIntoResult {
                offset: result.offset,
                read: result.read,
                eof: result.eof,
            })
            .collect())
    }
}

impl<F: VectorBackend> FsClient<F> {
    /// Native bounded whole-file reads for backend adapters.
    ///
    /// Applications should use [`VfsiExt::read_files_with_options`].
    #[doc(hidden)]
    pub fn read_files_native<P: AsRef<Path>>(
        &self,
        paths: &[P],
        options: ReadAllOptions,
    ) -> VfResult<Vec<Vec<u8>>> {
        let files: Vec<_> = paths
            .iter()
            .map(|path| VfFile::from_os_path(path.as_ref()))
            .collect();
        let result = self
            .lock()?
            .vread_all_with_options_impl(&files, options)
            .map_err(|error| {
                error
                    .index()
                    .and_then(|index| paths.get(index))
                    .map_or(error.clone(), |path| {
                        error.with_context("read_files", path.as_ref())
                    })
            });
        result.and_then(|buffers| {
            if buffers.len() != paths.len() {
                Err(wrong_result_count("read_files", paths.len(), buffers.len()))
            } else {
                Ok(buffers)
            }
        })
    }
}
