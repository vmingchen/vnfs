//! Complete paths and positional ranges for owned-result reads.
use crate::{Error, OwnedReadResult, Result};

pub(crate) fn read_backend_owned<
    F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static,
>(
    client: &vfsi_sync::FsClient<F>,
    requests: &[ReadRequest<'_, vfsi_sync::FsRead<'_, F>>],
    options: ReadOptions,
) -> Result<Vec<OwnedReadResult>> {
    let budget = options.limit_or(client.limits().max_read_bytes);
    if requests.iter().all(|request| request.range_ref().is_some()) {
        return client.readv_with_limit_projected(requests, budget, |request| {
            request.range_ref().expect("checked ranges")
        });
    }
    read_batch(
        requests,
        budget,
        |ranges, bytes| client.readv_with_limit_projected(ranges, bytes, |request| request),
        |paths, bytes| {
            client
                .read_files_with_options(paths, crate::ReadAllOptions::new().max_total_bytes(bytes))
        },
    )
}
pub(crate) fn read_backend<
    'a,
    F: vfsi_sync::NativeFileSystem + vfsi_sync::VectorFileSystem + vfsi_sync::VecFs + 'static,
>(
    client: &vfsi_sync::FsClient<F>,
    ops: impl IntoIterator<Item = ReadOp<'a, vfsi_sync::FsFile<F>>>,
    options: ReadOptions,
) -> Result<Vec<ReadResult>> {
    consume_ops(
        ops,
        options.limit_or(client.limits().max_read_bytes),
        |requests, options| read_backend_owned(client, requests, options),
        |requests, bytes| client.readv_into_with_limit(requests, bytes),
    )
}
use std::path::Path;

/// One consuming read operation. Construction performs no I/O or allocation.
/// Caller buffers are borrowed only until `readv` returns, including on errors.
pub struct ReadOp<'a, H: crate::FileHandle + 'a> {
    source: OpSource<'a, H>,
}
enum OpSource<'a, H: crate::FileHandle + 'a> {
    Whole(&'a Path),
    Range(H::ReadRequest<'a>, usize),
    Into(H::ReadIntoRequest<'a>, usize),
}
impl<H: crate::FileHandle> std::fmt::Debug for ReadOp<'_, H> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            OpSource::Whole(path) => formatter.debug_tuple("ReadOp::whole").field(path).finish(),
            OpSource::Range(_, length) => formatter
                .debug_struct("ReadOp::range")
                .field("length", length)
                .finish_non_exhaustive(),
            OpSource::Into(_, length) => formatter
                .debug_struct("ReadOp::into")
                .field("length", length)
                .finish_non_exhaustive(),
        }
    }
}
impl<'a, H: crate::FileHandle + 'a> From<&'a str> for ReadOp<'a, H> {
    fn from(path: &'a str) -> Self {
        Self::whole(path)
    }
}
impl<'a, H: crate::FileHandle + 'a> From<&'a Path> for ReadOp<'a, H> {
    fn from(path: &'a Path) -> Self {
        Self::whole(path)
    }
}
impl<'a, H: crate::FileHandle + 'a> ReadOp<'a, H> {
    /// Collect a complete file; fail instead of truncating at the byte budget.
    pub fn whole<P: AsRef<Path> + ?Sized>(path: &'a P) -> Self {
        Self {
            source: OpSource::Whole(path.as_ref()),
        }
    }
    /// Allocate a positional range. Short progress and EOF are reported explicitly.
    pub fn range(file: &'a H, offset: u64, length: usize) -> Self {
        Self {
            source: OpSource::Range(file.read_request_at(offset, length), length),
        }
    }
    /// Fill caller storage at an absolute offset, without changing the cursor.
    /// The request length is exactly `buffer.len()`.
    pub fn into(file: &'a H, offset: u64, buffer: &'a mut [u8]) -> Self {
        let length = buffer.len();
        Self {
            source: OpSource::Into(file.read_request_at_into(offset, buffer), length),
        }
    }
    /// Inspect a complete-file path from an external `Fs` implementation.
    pub fn whole_file_path(&self) -> Option<&'a Path> {
        match &self.source {
            OpSource::Whole(path) => Some(*path),
            _ => None,
        }
    }
    /// Borrow an allocating range request from an external implementation.
    pub fn range_ref(&self) -> Option<&H::ReadRequest<'a>> {
        match &self.source {
            OpSource::Range(request, _) => Some(request),
            _ => None,
        }
    }
    /// Borrow a caller-buffer request exclusively from an external implementation.
    pub fn buffer_request_mut(&mut self) -> Option<&mut H::ReadIntoRequest<'a>> {
        match &mut self.source {
            OpSource::Into(request, _) => Some(request),
            _ => None,
        }
    }
}

/// Progress for one operation, with no borrowed references to caller storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    /// Resolved absolute offset; zero for a complete-file operation.
    pub offset: u64,
    /// Number of valid bytes in owned data or the caller's buffer.
    pub read: usize,
    /// True when the backend observed end of file.
    pub eof: bool,
    /// Owned contents for `whole`/`range`; `None` for `into`, including empty reads.
    pub data: Option<Vec<u8>>,
}

pub(crate) fn consume_ops<'a, H: crate::FileHandle + 'a>(
    ops: impl IntoIterator<Item = ReadOp<'a, H>>,
    budget: usize,
    owned: impl FnOnce(
        &[ReadRequest<'a, H::ReadRequest<'a>>],
        ReadOptions,
    ) -> Result<Vec<OwnedReadResult>>,
    buffers: impl FnOnce(&mut [H::ReadIntoRequest<'a>], usize) -> Result<Vec<crate::ReadIntoResult>>,
) -> Result<Vec<ReadResult>> {
    let mut owned_ops = Vec::new();
    let mut owned_indices = Vec::new();
    let mut buffer_ops = Vec::new();
    let mut buffer_indices = Vec::new();
    let mut requested = 0usize;
    let mut count = 0usize;
    for (index, op) in ops.into_iter().enumerate() {
        count += 1;
        let length = match &op.source {
            OpSource::Whole(_) => 0,
            OpSource::Range(_, length) | OpSource::Into(_, length) => *length,
        };
        requested = requested
            .checked_add(length)
            .filter(|bytes| *bytes <= budget)
            .ok_or_else(|| Error::client(index, libc::EFBIG as u32))?;
        match op.source {
            OpSource::Whole(path) => {
                owned_ops.push(ReadRequest::whole_file(path));
                owned_indices.push(index);
            }
            OpSource::Range(request, _) => {
                owned_ops.push(ReadRequest::range(request));
                owned_indices.push(index);
            }
            OpSource::Into(request, _) => {
                buffer_ops.push(request);
                buffer_indices.push(index);
            }
        }
    }
    let remap = |error: Error, indices: &[usize]| match error.index() {
        Some(index) if index < indices.len() => error.with_index(indices[index]),
        Some(_) => Error::transport(None, "invalid readv backend error index"),
        None => error,
    };
    let buffered = if buffer_ops.is_empty() {
        Vec::new()
    } else {
        buffers(&mut buffer_ops, budget).map_err(|e| remap(e, &buffer_indices))?
    };
    drop(buffer_ops);
    if buffered.len() != buffer_indices.len() {
        return Err(Error::transport(None, "invalid readv buffer result count"));
    }
    let mut remaining = budget;
    for result in &buffered {
        remaining = remaining
            .checked_sub(result.read)
            .ok_or_else(|| Error::transport(None, "readv backend exceeded byte budget"))?;
    }
    let allocated = if owned_ops.is_empty() {
        Vec::new()
    } else {
        owned(&owned_ops, ReadOptions::new().max_total_bytes(remaining))
            .map_err(|e| remap(e, &owned_indices))?
    };
    if allocated.len() != owned_indices.len() {
        return Err(Error::transport(None, "invalid readv owned result count"));
    }
    let mut output: Vec<Option<ReadResult>> = (0..count).map(|_| None).collect();
    for (index, result) in buffer_indices.into_iter().zip(buffered) {
        output[index] = Some(ReadResult {
            offset: result.offset,
            read: result.read,
            eof: result.eof,
            data: None,
        });
    }
    for (index, result) in owned_indices.into_iter().zip(allocated) {
        remaining = remaining
            .checked_sub(result.data.len())
            .ok_or_else(|| Error::transport(None, "readv backend exceeded byte budget"))?;
        output[index] = Some(ReadResult {
            offset: result.offset,
            read: result.data.len(),
            eof: result.eof,
            data: Some(result.data),
        });
    }
    output
        .into_iter()
        .map(|result| result.ok_or_else(|| Error::transport(None, "readv omitted a result")))
        .collect()
}
/// Internal projection for the native owned-result batch path.
pub(crate) struct ReadRequest<'a, R> {
    pub(crate) source: Source<'a, R>,
}
pub(crate) enum Source<'a, R> {
    Whole(&'a Path),
    Range(R),
}
impl<'a, R> ReadRequest<'a, R> {
    /// Read a complete path within the aggregate budget, without manual OPEN/CLOSE.
    pub fn whole_file<P: AsRef<Path> + ?Sized>(path: &'a P) -> Self {
        Self {
            source: Source::Whole(path.as_ref()),
        }
    }
    /// Wrap a positional request, preserving short results and EOF.
    pub fn range(request: R) -> Self {
        Self {
            source: Source::Range(request),
        }
    }
    /// Borrow the backend-defined range request, or return `None` for a whole file.
    /// External [`crate::Fs`] implementations can use this together with
    /// [`Self::whole_file_path`] to dispatch either source without exposing handles.
    pub fn range_ref(&self) -> Option<&R> {
        match &self.source {
            Source::Range(request) => Some(request),
            _ => None,
        }
    }
}
impl<'a, R> From<R> for ReadRequest<'a, R> {
    fn from(request: R) -> Self {
        Self::range(request)
    }
}
/// Aggregate logical byte budget, defaulting to the client's policy (16 MiB).
/// Range request lengths must fit even if EOF returns fewer bytes. Complete
/// files share the remaining returned-data budget and fail rather than truncate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOptions {
    max_total_bytes: Option<usize>,
}
impl ReadOptions {
    /// Inherit the client's aggregate read budget.
    pub const fn new() -> Self {
        Self {
            max_total_bytes: None,
        }
    }
    /// Override the aggregate budget for this call. Stream larger files instead
    /// of raising this when bounded memory is required.
    pub const fn max_total_bytes(mut self, bytes: usize) -> Self {
        self.max_total_bytes = Some(bytes);
        self
    }
    /// None inherits the client's configured read budget.
    pub const fn total_byte_limit(self) -> Option<usize> {
        self.max_total_bytes
    }
    pub(crate) fn limit_or(self, default: usize) -> usize {
        self.max_total_bytes.unwrap_or(default)
    }
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self::new()
    }
}
pub(crate) fn read_batch<R>(
    requests: &[ReadRequest<'_, R>],
    budget: usize,
    read_ranges: impl FnOnce(&[&R], usize) -> Result<Vec<OwnedReadResult>>,
    read_paths: impl FnOnce(&[&Path], usize) -> Result<Vec<Vec<u8>>>,
) -> Result<Vec<OwnedReadResult>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let (mut paths, mut path_indices, mut ranges, mut range_indices) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, r) in requests.iter().enumerate() {
        match &r.source {
            Source::Whole(path) => {
                paths.push(*path);
                path_indices.push(i);
            }
            Source::Range(range) => {
                ranges.push(range);
                range_indices.push(i);
            }
        }
    }
    let remap = |e: Error, indices: &[usize]| match e.index() {
        Some(i) if i < indices.len() => e.with_index(indices[i]),
        Some(_) => Error::transport(None, "invalid readv backend error index"),
        None => e,
    };
    // Group ranges before complete paths. Reads do not promise a snapshot.
    let range_results = if ranges.is_empty() {
        Vec::new()
    } else {
        read_ranges(&ranges, budget).map_err(|e| remap(e, &range_indices))?
    };
    if range_results.len() != ranges.len() {
        return Err(Error::transport(None, "invalid readv range result count"));
    }
    let mut remaining = budget;
    for r in &range_results {
        remaining = remaining
            .checked_sub(r.data.len())
            .ok_or_else(|| Error::transport(None, "readv backend exceeded byte budget"))?;
    }
    let path_results = if paths.is_empty() {
        Vec::new()
    } else {
        read_paths(&paths, remaining).map_err(|e| remap(e, &path_indices))?
    };
    if path_results.len() != paths.len() {
        return Err(Error::transport(None, "invalid readv path result count"));
    }
    for r in &path_results {
        remaining = remaining
            .checked_sub(r.len())
            .ok_or_else(|| Error::transport(None, "readv backend exceeded byte budget"))?;
    }
    let mut output: Vec<Option<OwnedReadResult>> = (0..requests.len()).map(|_| None).collect();
    for (i, r) in range_indices.into_iter().zip(range_results) {
        output[i] = Some(r);
    }
    for (i, data) in path_indices.into_iter().zip(path_results) {
        output[i] = Some(OwnedReadResult {
            offset: 0,
            data,
            eof: true,
        });
    }
    output
        .into_iter()
        .map(|r| r.ok_or_else(|| Error::transport(None, "readv omitted a result")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FsExt;
    #[cfg(all(feature = "auto", target_os = "linux"))]
    #[test]
    fn consuming_dispatch_rejects_bad_reply_shapes_and_does_not_replay() {
        for count in [0, 2] {
            let error = consume_ops::<crate::MountedFile>(
                [ReadOp::whole("/a")],
                16,
                |_, _| {
                    Ok((0..count)
                        .map(|_| OwnedReadResult {
                            offset: 0,
                            data: vec![],
                            eof: true,
                        })
                        .collect())
                },
                |_, _| panic!("no buffer operations"),
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        let temp = tempfile::tempdir().unwrap();
        let fs = crate::Mounted::new(temp.path()).unwrap();
        fs.write("/a", b"abc").unwrap();
        let file = fs.open("/a").unwrap();
        for (count, read) in [(0, 1), (2, 1), (1, 17)] {
            let mut buffer = [0; 1];
            let error = consume_ops(
                [ReadOp::into(&file, 0, &mut buffer)],
                16,
                |_, _| panic!("no owned operations"),
                |_, _| {
                    Ok(vec![
                        crate::ReadIntoResult {
                            offset: 0,
                            read,
                            eof: false
                        };
                        count
                    ])
                },
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        let mut calls = 0;
        let mut buffer = [0; 1];
        let error = consume_ops(
            [ReadOp::whole("/a"), ReadOp::into(&file, 0, &mut buffer)],
            16,
            |_, _| panic!("failure must stop later phases"),
            |_, _| {
                calls += 1;
                Err(Error::transport(None, "lost reply"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.index(), None);
        buffer.fill(7);
    }
    #[test]
    fn malformed_cardinality_and_budget_are_errors() {
        for count in [0, 2] {
            let error = read_batch(
                &[ReadRequest::range(())],
                1,
                |_, _| {
                    Ok((0..count)
                        .map(|_| OwnedReadResult {
                            offset: 0,
                            data: vec![0],
                            eof: false,
                        })
                        .collect())
                },
                |_, _| panic!("no paths"),
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        for count in [0, 2] {
            let error = read_batch::<()>(
                &[ReadRequest::whole_file("/a")],
                1,
                |_, _| panic!("no ranges"),
                |_, _| Ok(vec![vec![]; count]),
            )
            .unwrap_err();
            assert!(error.is_transport());
        }
        let error = read_batch::<()>(
            &[ReadRequest::whole_file("/a")],
            1,
            |_, _| panic!("no ranges"),
            |_, _| Ok(vec![vec![0, 1]]),
        )
        .unwrap_err();
        assert!(error.is_transport());
    }
    #[test]
    fn transport_errors_are_not_attributed_or_replayed() {
        let mut calls = 0;
        let error = read_batch::<()>(
            &[ReadRequest::whole_file("/a")],
            1,
            |_, _| panic!("no ranges"),
            |_, _| {
                calls += 1;
                Err(Error::transport(None, "lost reply"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.index(), None);
        let error = read_batch::<()>(
            &[ReadRequest::whole_file("/a")],
            1,
            |_, _| panic!("no ranges"),
            |_, _| Err(Error::transport(Some(9), "bad index")),
        )
        .unwrap_err();
        assert_eq!(error.index(), None);
        assert!(
            error
                .to_string()
                .contains("invalid readv backend error index")
        );
    }
    #[test]
    fn empty_requests_do_not_dispatch() {
        assert!(
            read_batch::<()>(
                &[],
                0,
                |_, _| panic!("range dispatch"),
                |_, _| panic!("path dispatch")
            )
            .unwrap()
            .is_empty()
        );
    }
}
