use super::support::FsReadResult as OwnedReadResult;
use super::*;
use crate::{FailureState, IoProgress, OpError, OpResult, VecResult};
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
    owned: impl FnOnce(&[ReadRequest<'a, R>], usize) -> VecResult<OwnedReadResult>,
    buffers: impl FnOnce(&mut [B], usize) -> VecResult<crate::api::ReadIntoResult>,
) -> VecResult<ReadResult> {
    let mut ops = ops.into_iter();
    let Some(first) = ops.next() else {
        return Ok(Vec::new());
    };
    let second = ops.next();
    if second.is_none() {
        let (offset, length, whole) = match &first.source {
            OpSource::Whole(_) => (0, 0, true),
            OpSource::Range(_, offset, length) => (*offset, *length, false),
            OpSource::Into(_, offset, buffer) => (*offset, buffer.len(), false),
        };
        if length > budget {
            return Err(Error::errno(0, libc::EFBIG as u32));
        }
        let mut remaining = budget;
        let request = match first.source {
            OpSource::Whole(path) => ReadRequest::whole_file(path),
            OpSource::Range(file, offset, length) => {
                ReadRequest::range(range(file, offset, length))
            }
            OpSource::Into(file, offset, buffer) => {
                return Ok(
                    read_phase(buffers(&mut [into(file, offset, buffer)], budget), 1)
                        .into_iter()
                        .map(|result| {
                            checked_read(
                                result.map(|read| {
                                    ReadResult::buffered(read.offset, read.bytes_read, read.eof)
                                }),
                                true,
                                Some((offset, length)),
                                false,
                                &mut remaining,
                            )
                        })
                        .collect(),
                );
            }
        };
        return Ok(read_phase(owned(&[request], budget), 1)
            .into_iter()
            .map(|result| {
                checked_read(
                    result.map(|read| ReadResult::owned(read.offset, read.data, read.eof)),
                    false,
                    (!whole).then_some((offset, length)),
                    whole,
                    &mut remaining,
                )
            })
            .collect());
    }
    let buffered_first = matches!(&first.source, OpSource::Into(..));
    let ops = std::iter::once(first).chain(second).chain(ops);
    // Bound speculative reservation for iterators with exaggerated size hints.
    let (lower, upper) = ops.size_hint();
    let capacity = upper
        .filter(|&upper| upper <= 1024)
        .unwrap_or(lower)
        .clamp(4, 1024);
    let mut owned_ops = Vec::with_capacity(if buffered_first { 0 } else { capacity });
    let mut owned_indices = Vec::new();
    let mut buffer_ops = Vec::with_capacity(if buffered_first { capacity } else { 0 });
    let mut buffer_indices = Vec::new();
    // Backend request projections are opaque. Retain the original bounds
    // lazily; singleton and all-whole-file batches need no extra allocation.
    let mut expected = Vec::new();
    let mut mixed = false;
    let mut requested = 0usize;
    let mut count = 0usize;
    for (index, op) in ops.enumerate() {
        count += 1;
        let bounds = match &op.source {
            OpSource::Whole(_) => None,
            OpSource::Range(_, offset, length) => Some((*offset, *length)),
            OpSource::Into(_, offset, buffer) => Some((*offset, buffer.len())),
        };
        let length = bounds.map_or(0, |(_, length)| length);
        if let Some(bounds) = bounds {
            if expected.is_empty() {
                expected.reserve(capacity);
                expected.resize(index, (0, 0));
            }
            expected.push(bounds);
        } else if !expected.is_empty() {
            expected.push((0, 0));
        }
        requested = requested
            .checked_add(length)
            .filter(|bytes| *bytes <= budget)
            .ok_or_else(|| Error::errno(index, libc::EFBIG as u32))?;
        match op.source {
            OpSource::Whole(path) => {
                if !mixed && !buffer_ops.is_empty() {
                    buffer_indices.extend(0..index);
                    mixed = true;
                }
                owned_ops.push(ReadRequest::whole_file(path));
                if mixed {
                    owned_indices.push(index);
                }
            }
            OpSource::Range(file, offset, length) => {
                if !mixed && !buffer_ops.is_empty() {
                    buffer_indices.extend(0..index);
                    mixed = true;
                }
                owned_ops.push(ReadRequest::range(range(file, offset, length)));
                if mixed {
                    owned_indices.push(index);
                }
            }
            OpSource::Into(file, offset, buffer) => {
                if !mixed && !owned_ops.is_empty() {
                    owned_indices.extend(0..index);
                    mixed = true;
                }
                buffer_ops.push(into(file, offset, buffer));
                if mixed {
                    buffer_indices.push(index);
                }
            }
        }
    }
    let buffer_count = buffer_ops.len();
    let mut remaining = budget;
    let buffered: Vec<_> = if buffer_count == 0 {
        Vec::new()
    } else {
        read_phase(buffers(&mut buffer_ops, budget), buffer_count)
            .into_iter()
            .enumerate()
            .map(|(local, result)| {
                let index = if mixed { buffer_indices[local] } else { local };
                checked_read(
                    result.map(|read| ReadResult::buffered(read.offset, read.bytes_read, read.eof)),
                    true,
                    Some(expected[index]),
                    false,
                    &mut remaining,
                )
            })
            .collect()
    };
    drop(buffer_ops);
    // An unsuccessful buffer phase must not discard known results or suppress
    // independent owned reads. Only confirmed data charges the shared budget.
    let allocated: Vec<_> = if owned_ops.is_empty() {
        Vec::new()
    } else {
        read_phase(owned(&owned_ops, remaining), owned_ops.len())
            .into_iter()
            .enumerate()
            .map(|(local, result)| {
                let index = if mixed { owned_indices[local] } else { local };
                let whole = matches!(owned_ops[local].source, Source::Whole(_));
                checked_read(
                    result.map(|read| ReadResult::owned(read.offset, read.data, read.eof)),
                    false,
                    if whole { None } else { Some(expected[index]) },
                    whole,
                    &mut remaining,
                )
            })
            .collect()
    };
    if buffer_count == 0 {
        return Ok(allocated);
    }
    if owned_ops.is_empty() {
        return Ok(buffered);
    }
    let mut output: Vec<Option<OpResult<ReadResult>>> = (0..count).map(|_| None).collect();
    for (index, result) in buffer_indices.into_iter().zip(buffered) {
        output[index] = Some(result);
    }
    for (index, result) in owned_indices.into_iter().zip(allocated) {
        output[index] = Some(result);
    }
    Ok(output
        .into_iter()
        .map(|slot| slot.expect("every read input assigned"))
        .collect())
}

// After one phase has dispatched, a later phase's rejection belongs to its own
// NotAttempted slots. Never erase earlier completions with an outer error.
fn read_phase<T>(result: VecResult<T>, expected: usize) -> Vec<OpResult<T>> {
    match result {
        Ok(mut outcomes) if outcomes.len() <= expected => {
            outcomes.resize_with(expected, || {
                Err(OpError::unknown(
                    Error::invalid_reply(None, "missing read outcome"),
                    None,
                ))
            });
            outcomes
        }
        Ok(_) => (0..expected)
            .map(|_| {
                Err(OpError::unknown(
                    Error::invalid_reply(None, "too many read outcomes"),
                    None,
                ))
            })
            .collect(),
        Err(cause) if cause.index().is_some_and(|index| index >= expected) => (0..expected)
            .map(|_| {
                Err(OpError::not_attempted(Error::invalid_reply(
                    None,
                    "invalid read backend error index",
                )))
            })
            .collect(),
        Err(cause) => (0..expected)
            .map(|_| Err(OpError::not_attempted(cause.clone())))
            .collect(),
    }
}

fn checked_read(
    result: OpResult<ReadResult>,
    buffered: bool,
    expected: Option<(u64, usize)>,
    whole: bool,
    remaining: &mut usize,
) -> OpResult<ReadResult> {
    let progress = match &result {
        Ok(read) => Some(read),
        Err(error) => match error.progress() {
            None => None,
            Some(IoProgress::Read(read)) if error.state() != FailureState::NotAttempted => {
                Some(read)
            }
            _ => {
                return Err(OpError::unknown(
                    Error::invalid_reply(None, "invalid read failure progress"),
                    None,
                ));
            }
        },
    };
    if let Some(read) = progress {
        let valid = read.is_buffered() == buffered
            && read.bytes_read() <= *remaining
            && read
                .offset()
                .checked_add(read.bytes_read() as u64)
                .is_some()
            && expected.is_none_or(|(offset, length)| {
                read.offset() == offset
                    && read.bytes_read() <= length
                    && (result.is_err() || length == 0 || read.bytes_read() != 0 || read.eof())
            })
            && (!whole || read.offset() == 0 && (result.is_err() || read.eof()));
        if !valid {
            return Err(OpError::unknown(
                Error::invalid_reply(None, "invalid read offset, length, payload or byte budget"),
                None,
            ));
        }
        *remaining -= read.bytes_read();
    }
    result
}

fn checked_owned(
    result: OpResult<OwnedReadResult>,
    whole: bool,
    remaining: &mut usize,
) -> OpResult<OwnedReadResult> {
    checked_read(
        result.map(|read| ReadResult::owned(read.offset, read.data, read.eof)),
        false,
        None,
        whole,
        remaining,
    )
    .map(|read| {
        let offset = read.offset();
        let eof = read.eof();
        OwnedReadResult {
            offset,
            eof,
            data: read.into_data().expect("owned read result"),
        }
    })
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
    read_ranges: impl FnOnce(&[&R], usize) -> VecResult<OwnedReadResult>,
    read_paths: impl FnOnce(&[&Path], usize) -> VecResult<Vec<u8>>,
) -> VecResult<OwnedReadResult> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let paths_first = matches!(requests[0].source, Source::Whole(_));
    let mut paths = Vec::with_capacity(if paths_first { requests.len() } else { 0 });
    let mut ranges = Vec::with_capacity(if paths_first { 0 } else { requests.len() });
    for r in requests {
        match &r.source {
            Source::Whole(path) => paths.push(*path),
            Source::Range(range) => ranges.push(range),
        }
    }
    // Keep index bookkeeping out of homogeneous hot loops altogether.
    // Only a mixed batch pays for a second pass and its exact-sized maps.
    let (mut path_indices, mut range_indices) = (Vec::new(), Vec::new());
    if !paths.is_empty() && !ranges.is_empty() {
        path_indices.reserve(paths.len());
        range_indices.reserve(ranges.len());
        for (i, request) in requests.iter().enumerate() {
            match request.source {
                Source::Whole(_) => path_indices.push(i),
                Source::Range(_) => range_indices.push(i),
            }
        }
    }
    let mut remaining = budget;
    let range_results: Vec<_> = if ranges.is_empty() {
        Vec::new()
    } else {
        read_phase(read_ranges(&ranges, budget), ranges.len())
            .into_iter()
            .map(|result| checked_owned(result, false, &mut remaining))
            .collect()
    };
    let path_results: Vec<_> = if paths.is_empty() {
        Vec::new()
    } else {
        read_phase(read_paths(&paths, remaining), paths.len())
            .into_iter()
            .map(|result| {
                checked_owned(
                    result.map(|data| OwnedReadResult {
                        offset: 0,
                        data,
                        eof: true,
                    }),
                    true,
                    &mut remaining,
                )
            })
            .collect()
    };
    if paths.is_empty() {
        return Ok(range_results);
    }
    if ranges.is_empty() {
        return Ok(path_results);
    }
    let mut output: Vec<Option<OpResult<OwnedReadResult>>> =
        (0..requests.len()).map(|_| None).collect();
    for (index, result) in range_indices.into_iter().zip(range_results) {
        output[index] = Some(result);
    }
    for (index, result) in path_indices.into_iter().zip(path_results) {
        output[index] = Some(result);
    }
    Ok(output
        .into_iter()
        .map(|slot| slot.expect("every owned read input assigned"))
        .collect())
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
