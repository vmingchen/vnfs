use super::support::FsReadResult as OwnedReadResult;
use super::*;
use std::path::Path;

/// One consuming read operation. Construction performs no I/O or allocation.
/// Caller buffers are borrowed only until `Vfsi::vread` returns, including on errors.
#[must_use = "configuration and requests have no effect until used"]
pub struct ReadOp<'a, H: crate::api::FileHandle + 'a> {
    source: OpSource<'a, H>,
}
enum OpSource<'a, H: crate::api::FileHandle + 'a> {
    Whole(&'a Path),
    Range(&'a H, u64, usize),
    Into(&'a H, u64, &'a mut [u8]),
}
impl<H: crate::api::FileHandle> std::fmt::Debug for ReadOp<'_, H> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            OpSource::Whole(path) => formatter.debug_tuple("ReadOp::whole").field(path).finish(),
            OpSource::Range(_, _, length) => formatter
                .debug_struct("ReadOp::range")
                .field("length", length)
                .finish_non_exhaustive(),
            OpSource::Into(_, _, buffer) => formatter
                .debug_struct("ReadOp::into")
                .field("length", &buffer.len())
                .finish_non_exhaustive(),
        }
    }
}
impl<'a, H: crate::api::FileHandle + 'a> From<&'a str> for ReadOp<'a, H> {
    fn from(path: &'a str) -> Self {
        Self::whole(path)
    }
}
impl<'a, H: crate::api::FileHandle + 'a> From<&'a Path> for ReadOp<'a, H> {
    fn from(path: &'a Path) -> Self {
        Self::whole(path)
    }
}
impl<'a, H: crate::api::FileHandle + 'a> ReadOp<'a, H> {
    /// Collect a complete file; fail instead of truncating at the byte budget.
    pub fn whole<P: AsRef<Path> + ?Sized>(path: &'a P) -> Self {
        Self {
            source: OpSource::Whole(path.as_ref()),
        }
    }
    /// Allocate a positional range. Short progress and EOF are reported explicitly.
    pub fn range(file: &'a H, offset: u64, length: usize) -> Self {
        Self {
            source: OpSource::Range(file, offset, length),
        }
    }
    /// Fill caller storage at an absolute offset, without changing the cursor.
    /// The request length is exactly `buffer.len()`.
    pub fn into(file: &'a H, offset: u64, buffer: &'a mut [u8]) -> Self {
        Self {
            source: OpSource::Into(file, offset, buffer),
        }
    }
    /// Inspect a complete-file path from an external `Vfsi` implementation.
    pub fn whole_file_path(&self) -> Option<&'a Path> {
        match &self.source {
            OpSource::Whole(path) => Some(*path),
            _ => None,
        }
    }
    /// Inspect the handle, absolute offset, and length of an allocating range.
    pub fn range_parts(&self) -> Option<(&'a H, u64, usize)> {
        match &self.source {
            OpSource::Range(file, offset, length) => Some((*file, *offset, *length)),
            _ => None,
        }
    }
    /// Borrow the handle, absolute offset, and caller storage for direct dispatch.
    pub fn buffer_parts_mut(&mut self) -> Option<(&'a H, u64, &mut [u8])> {
        match &mut self.source {
            OpSource::Into(file, offset, buffer) => Some((*file, *offset, buffer)),
            _ => None,
        }
    }
}

/// Progress for one operation, with no borrowed references to caller storage.
#[derive(Clone, PartialEq, Eq)]
pub struct ReadResult {
    offset: u64,
    eof: bool,
    payload: ReadPayload,
}
#[derive(Clone, PartialEq, Eq)]
enum ReadPayload {
    Owned(Vec<u8>),
    Buffered(usize),
}
impl std::fmt::Debug for ReadResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadResult")
            .field("offset", &self.offset())
            .field("bytes_read", &self.bytes_read())
            .field("eof", &self.eof())
            .field("data", &self.data())
            .finish()
    }
}
impl ReadResult {
    /// Owned contents. The byte count is derived, never independently supplied.
    pub fn owned(offset: u64, data: Vec<u8>, eof: bool) -> Self {
        Self {
            offset,
            eof,
            payload: ReadPayload::Owned(data),
        }
    }
    /// Progress in caller storage, for implementing Vfsi. The implementation
    /// must keep bytes_read within the corresponding destination length.
    pub const fn buffered(offset: u64, bytes_read: usize, eof: bool) -> Self {
        Self {
            offset,
            eof,
            payload: ReadPayload::Buffered(bytes_read),
        }
    }
    pub const fn offset(&self) -> u64 {
        self.offset
    }
    /// Number of valid bytes in owned data or caller-provided storage.
    pub const fn bytes_read(&self) -> usize {
        match &self.payload {
            ReadPayload::Owned(data) => data.len(),
            ReadPayload::Buffered(count) => *count,
        }
    }
    pub const fn eof(&self) -> bool {
        self.eof
    }
    pub fn data(&self) -> Option<&[u8]> {
        match &self.payload {
            ReadPayload::Owned(data) => Some(data.as_slice()),
            ReadPayload::Buffered(_) => None,
        }
    }
    pub fn into_data(self) -> Option<Vec<u8>> {
        match self.payload {
            ReadPayload::Owned(data) => Some(data),
            ReadPayload::Buffered(_) => None,
        }
    }
    pub const fn is_buffered(&self) -> bool {
        matches!(&self.payload, ReadPayload::Buffered(_))
    }
}

/// Fluent configuration must be retained; discarding a changed copy is diagnosed.
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// use vfsi_core::api::ReadOptions;
/// let options = ReadOptions::new();
/// options.with_max_total_bytes(std::num::NonZeroUsize::new(4096).unwrap());
/// ```
///
/// Aggregate logical byte budget, defaulting to the client's policy (16 MiB).
/// Range request lengths must fit even if EOF returns fewer bytes. Complete
/// files share the remaining returned-data budget and fail rather than truncate.
///
/// ```
/// use std::num::NonZeroUsize;
/// use vfsi_core::api::ReadOptions;
/// let inherited = ReadOptions::new();
/// let limited = inherited.with_max_total_bytes(NonZeroUsize::new(4096).unwrap());
/// assert_eq!(limited.max_total_bytes(), NonZeroUsize::new(4096));
/// assert_eq!(limited.max_total_bytes_or(8192), 4096);
/// assert_eq!(inherited.max_total_bytes(), None);
/// assert_eq!(inherited.max_total_bytes_or(8192), 8192);
/// ```
/// Overrides require an explicitly validated nonzero value.
///
/// ```compile_fail
/// use vfsi_core::api::ReadOptions;
/// ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "configuration and requests have no effect until used"]
pub struct ReadOptions {
    max_total_bytes: Option<std::num::NonZeroUsize>,
}
impl ReadOptions {
    /// Inherit the client's aggregate read budget.
    pub const fn new() -> Self {
        Self {
            max_total_bytes: None,
        }
    }
    /// Override the aggregate budget for this call. New options inherit client policy.
    /// Only nonzero overrides are representable. Stream larger files instead
    /// of raising this when bounded memory is required.
    pub const fn with_max_total_bytes(mut self, bytes: std::num::NonZeroUsize) -> Self {
        self.max_total_bytes = Some(bytes);
        self
    }
    /// Return the configured override, not the effective client budget.
    /// `None` inherits the client's budget; it does not mean unlimited.
    pub const fn max_total_bytes(self) -> Option<std::num::NonZeroUsize> {
        self.max_total_bytes
    }
    /// Resolve an inherited aggregate budget against the client's default.
    pub fn max_total_bytes_or(self, default: usize) -> usize {
        self.max_total_bytes
            .map_or(default, std::num::NonZeroUsize::get)
    }
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self::new()
    }
}
pub fn consume_ops<'a, H: crate::api::FileHandle + 'a, R, B>(
    ops: impl IntoIterator<Item = ReadOp<'a, H>>,
    budget: usize,
    range: impl Fn(&'a H, u64, usize) -> R,
    into: impl Fn(&'a H, u64, &'a mut [u8]) -> B,
    owned: impl FnOnce(&[ReadRequest<'a, R>], usize) -> Result<Vec<OwnedReadResult>>,
    buffers: impl FnOnce(&mut [B], usize) -> Result<Vec<crate::api::ReadIntoResult>>,
) -> Result<Vec<ReadResult>> {
    let mut ops = ops.into_iter();
    let Some(first) = ops.next() else {
        return Ok(Vec::new());
    };
    let second = ops.next();
    if second.is_none() {
        let length = match &first.source {
            OpSource::Whole(_) => 0,
            OpSource::Range(_, _, length) => *length,
            OpSource::Into(_, _, buffer) => buffer.len(),
        };
        if length > budget {
            return Err(Error::client(0, libc::EFBIG as u32));
        }
        let remap = |error: Error| match error.index() {
            Some(0) | None => error,
            Some(_) => Error::transport(None, "invalid vread_native backend error index"),
        };
        let request = match first.source {
            OpSource::Whole(path) => ReadRequest::whole_file(path),
            OpSource::Range(file, offset, length) => {
                ReadRequest::range(range(file, offset, length))
            }
            OpSource::Into(file, offset, buffer) => {
                let values = buffers(&mut [into(file, offset, buffer)], budget).map_err(remap)?;
                if values.len() != 1 {
                    return Err(Error::transport(
                        None,
                        "invalid vread_native buffer result count",
                    ));
                }
                if values[0].bytes_read > budget {
                    return Err(Error::transport(
                        None,
                        "vread_native backend exceeded byte budget",
                    ));
                }
                return Ok(values
                    .into_iter()
                    .map(|result| {
                        ReadResult::buffered(result.offset, result.bytes_read, result.eof)
                    })
                    .collect());
            }
        };
        let values = owned(&[request], budget).map_err(remap)?;
        if values.len() != 1 {
            return Err(Error::transport(
                None,
                "invalid vread_native owned result count",
            ));
        }
        if values[0].data.len() > budget {
            return Err(Error::transport(
                None,
                "vread_native backend exceeded byte budget",
            ));
        }
        return Ok(values
            .into_iter()
            .map(|result| ReadResult::owned(result.offset, result.data, result.eof))
            .collect());
    }
    let ops = std::iter::once(first).chain(second).chain(ops);
    let mut owned_ops = Vec::new();
    let mut owned_indices = Vec::new();
    let mut buffer_ops = Vec::new();
    let mut buffer_indices = Vec::new();
    let mut requested = 0usize;
    let mut count = 0usize;
    for (index, op) in ops.enumerate() {
        count += 1;
        let length = match &op.source {
            OpSource::Whole(_) => 0,
            OpSource::Range(_, _, length) => *length,
            OpSource::Into(_, _, buffer) => buffer.len(),
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
            OpSource::Range(file, offset, length) => {
                owned_ops.push(ReadRequest::range(range(file, offset, length)));
                owned_indices.push(index);
            }
            OpSource::Into(file, offset, buffer) => {
                buffer_ops.push(into(file, offset, buffer));
                buffer_indices.push(index);
            }
        }
    }
    let remap = |error: Error, indices: &[usize]| match error.index() {
        Some(index) if index < indices.len() => error.with_index(indices[index]),
        Some(_) => Error::transport(None, "invalid vread_native backend error index"),
        None => error,
    };
    let buffered = if buffer_ops.is_empty() {
        Vec::new()
    } else {
        buffers(&mut buffer_ops, budget).map_err(|e| remap(e, &buffer_indices))?
    };
    drop(buffer_ops);
    if buffered.len() != buffer_indices.len() {
        return Err(Error::transport(
            None,
            "invalid vread_native buffer result count",
        ));
    }
    let mut remaining = budget;
    for result in &buffered {
        remaining = remaining
            .checked_sub(result.bytes_read)
            .ok_or_else(|| Error::transport(None, "vread_native backend exceeded byte budget"))?;
    }
    let allocated = if owned_ops.is_empty() {
        Vec::new()
    } else {
        owned(&owned_ops, remaining).map_err(|e| remap(e, &owned_indices))?
    };
    if allocated.len() != owned_indices.len() {
        return Err(Error::transport(
            None,
            "invalid vread_native owned result count",
        ));
    }
    for result in &allocated {
        remaining = remaining
            .checked_sub(result.data.len())
            .ok_or_else(|| Error::transport(None, "vread_native backend exceeded byte budget"))?;
    }
    if buffer_indices.is_empty() {
        return Ok(allocated
            .into_iter()
            .map(|result| ReadResult::owned(result.offset, result.data, result.eof))
            .collect());
    }
    if owned_indices.is_empty() {
        return Ok(buffered
            .into_iter()
            .map(|result| ReadResult::buffered(result.offset, result.bytes_read, result.eof))
            .collect());
    }
    let mut output: Vec<Option<ReadResult>> = (0..count).map(|_| None).collect();
    for (index, result) in buffer_indices.into_iter().zip(buffered) {
        output[index] = Some(ReadResult::buffered(
            result.offset,
            result.bytes_read,
            result.eof,
        ));
    }
    for (index, result) in owned_indices.into_iter().zip(allocated) {
        output[index] = Some(ReadResult::owned(result.offset, result.data, result.eof));
    }
    output
        .into_iter()
        .map(|result| result.ok_or_else(|| Error::transport(None, "vread_native omitted a result")))
        .collect()
}
/// Internal projection for the native owned-result batch path.
pub struct ReadRequest<'a, R> {
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
    /// External [`crate::api::Vfsi`] implementations can use this together with
    /// a whole-file dispatch path without exposing handles.
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
pub fn read_batch<R>(
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
        Some(_) => Error::transport(None, "invalid vread_native backend error index"),
        None => e,
    };
    // Group ranges before complete paths. Reads do not promise a snapshot.
    let range_results = if ranges.is_empty() {
        Vec::new()
    } else {
        read_ranges(&ranges, budget).map_err(|e| remap(e, &range_indices))?
    };
    if range_results.len() != ranges.len() {
        return Err(Error::transport(
            None,
            "invalid vread_native range result count",
        ));
    }
    let mut remaining = budget;
    for r in &range_results {
        remaining = remaining
            .checked_sub(r.data.len())
            .ok_or_else(|| Error::transport(None, "vread_native backend exceeded byte budget"))?;
    }
    let path_results = if paths.is_empty() {
        Vec::new()
    } else {
        read_paths(&paths, remaining).map_err(|e| remap(e, &path_indices))?
    };
    if path_results.len() != paths.len() {
        return Err(Error::transport(
            None,
            "invalid vread_native path result count",
        ));
    }
    for r in &path_results {
        remaining = remaining
            .checked_sub(r.len())
            .ok_or_else(|| Error::transport(None, "vread_native backend exceeded byte budget"))?;
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
        .map(|r| r.ok_or_else(|| Error::transport(None, "vread_native omitted a result")))
        .collect()
}

#[cfg(test)]
mod option_layout_tests {
    use super::ReadOptions;
    use std::num::NonZeroUsize;
    #[test]
    fn read_options_are_one_word_and_keep_inheritance_distinct() {
        assert_eq!(
            std::mem::size_of::<ReadOptions>(),
            std::mem::size_of::<usize>()
        );
        let mut options = ReadOptions::new();
        assert_eq!(options.max_total_bytes_or(7), 7);
        for bytes in [1, 42, usize::MAX] {
            let limit = NonZeroUsize::new(bytes).unwrap();
            options = options.with_max_total_bytes(limit);
            assert_eq!(options.max_total_bytes(), Some(limit));
            assert_eq!(options.max_total_bytes_or(7), bytes);
        }
        options = ReadOptions::new();
        assert_eq!(options.max_total_bytes(), None);
        assert_eq!(options.max_total_bytes_or(7), 7);
        assert_eq!(
            options.max_total_bytes_or(0),
            0,
            "an inherited zero policy must not become unlimited"
        );
    }
}

#[cfg(test)]
mod compact_read_tests {
    use super::*;
    #[test]
    fn compact_read_results_preserve_owned_and_buffered_empty_progress() {
        if cfg!(target_pointer_width = "64") {
            assert_eq!(std::mem::size_of::<ReadResult>(), 40);
        }
        let owned = ReadResult::owned(7, Vec::new(), true);
        const BUFFERED: ReadResult = ReadResult::buffered(7, 0, true);
        assert_eq!(owned.bytes_read(), 0);
        assert_eq!(owned.offset(), BUFFERED.offset());
        assert_eq!(owned.eof(), BUFFERED.eof());
        assert_eq!(owned.data(), Some(b"".as_slice()));
        assert_eq!(BUFFERED.data(), None);
        assert_ne!(owned, BUFFERED);
        assert!(!owned.is_buffered());
        assert!(BUFFERED.is_buffered());
        assert_eq!(owned.clone().into_data(), Some(Vec::new()));
        assert_eq!(BUFFERED.clone().into_data(), None);
        let data = vec![1, 2, 3];
        let pointer = data.as_ptr();
        let result = ReadResult::owned(9, data, false);
        assert_eq!(result.bytes_read(), 3);
        let extracted = result.into_data().unwrap();
        assert_eq!(extracted.as_ptr(), pointer);
        assert_eq!(
            ReadResult::buffered(0, usize::MAX, false).bytes_read(),
            usize::MAX
        );
    }
}
