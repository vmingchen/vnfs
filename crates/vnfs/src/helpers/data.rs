//! Data-only bridges leave creation, publication and metadata policy to callers.
use crate::files::{OpenFlags, OpenOp, ReadOp, ReadOptions};
use crate::{Error, Result, StreamCompletion, StreamOptions, Vfsi, VfsiExt};
use std::{io::Write, num::NonZeroUsize, path::Path};

/// Stream a source into a caller-owned writer with bounded read storage.
///
/// Does not open/truncate the destination, copy metadata, flush it, or replay
/// failed writes. Useful when source I/O is direct NFS but destinations must
/// retain kernel-client semantics. The caller owns any partial output on error.
pub fn copy_to_writer(
    source: &impl Vfsi,
    path: impl AsRef<Path>,
    writer: &mut impl Write,
    options: StreamOptions,
) -> Result<u64> {
    let path = path.as_ref();
    let mut written = 0u64;
    let mut write_error = None;
    let completion = source.read_stream_with_options(path, options, |_, data| {
        if let Err(error) = writer.write_all(data) {
            write_error = Some(Error::from_io(0, error).with_context("copy_to_writer", path));
            return Ok(std::ops::ControlFlow::Break(()));
        }
        written = written
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::errno(0, libc::EOVERFLOW as u32))?;
        Ok(std::ops::ControlFlow::Continue(()))
    });
    if let Some(error) = write_error {
        return Err(error);
    }
    if completion? != StreamCompletion::Complete {
        return Err(
            Error::transport(None, "incomplete source stream").with_context("copy_to_writer", path)
        );
    }
    Ok(written)
}

/// Scheduling for [copy_to_writers]; ordered streaming is the default.
///
/// Existing [StreamOptions] values convert to ordered writer copies.
/// This helper-specific policy does not change [Vfsi::vstream].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use = "configuration must be passed to copy_to_writers"]
pub struct WriterCopyOptions {
    stream: StreamOptions,
    round_robin_bytes: Option<NonZeroUsize>,
}

impl WriterCopyOptions {
    pub const fn new() -> Self {
        Self {
            stream: StreamOptions::new(),
            round_robin_bytes: None,
        }
    }

    /// Opt into interleaved reads and writes within bounded file cohorts.
    ///
    /// Each round reads one chunk per active file, then delivers those chunks
    /// in input order. An earlier file need not finish before a sibling advances.
    /// At most 32 sources are open at once. The aggregate requested read payload
    /// is bounded by this budget and the client's read limit; chunks shrink when
    /// necessary to fit. This is not a process-RSS or destination-storage limit.
    ///
    /// Errors can leave several writers partial, including on a failed read or
    /// close. Errors do not describe a completed prefix. No output is replayed.
    pub const fn with_round_robin(mut self, max_read_bytes: NonZeroUsize) -> Self {
        self.round_robin_bytes = Some(max_read_bytes);
        self
    }
}

impl From<StreamOptions> for WriterCopyOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::new()
        }
    }
}

/// Stream corresponding source paths into caller-owned writers.
/// By default, uses one ordered vector stream, preserving existing delivery and
/// early-stop behavior. Opt-in round-robin copies batch continuation reads too,
/// allowing several outputs to advance before any one file finishes.
/// Does not create, truncate, flush, or publish destinations. Writers may already
/// contain partial output on any error; neither reads nor writes are replayed.
/// Returns bytes written for each input only after every stream completes.
/// Writers start at their current position; source offsets start at zero.
/// Backend read-ahead may occur before a writer fails, but no writes are replayed.
///
/// ```no_run
/// use vnfs::files::{StreamOptions, Vfsi};
/// use vnfs::helpers::copy_to_writers;
/// # fn example(fs: &impl Vfsi) -> vnfs::Result<()> {
/// let mut writers = [Vec::new(), Vec::new()];
/// let sizes = copy_to_writers(fs, &["/first", "/second"], &mut writers,
///     StreamOptions::new())?;
/// # let _ = sizes;
/// # Ok(())
/// # }
/// ```
///
/// Opt into a 2 MiB aggregate read window without changing ordered streams:
///
/// ```no_run
/// use std::num::NonZeroUsize;
/// use vnfs::{Vfsi, helpers::{copy_to_writers, WriterCopyOptions}};
/// # fn example(fs: &impl Vfsi) -> vnfs::Result<()> {
/// let mut writers = [Vec::new(), Vec::new()];
/// let options = WriterCopyOptions::new()
///     .with_round_robin(NonZeroUsize::new(2 * 1024 * 1024).unwrap());
/// copy_to_writers(fs, &["/first", "/second"], &mut writers, options)?;
/// # Ok(())
/// # }
/// ```
pub fn copy_to_writers<P: AsRef<Path>, W: Write>(
    source: &impl Vfsi,
    paths: &[P],
    writers: &mut [W],
    options: impl Into<WriterCopyOptions>,
) -> Result<Vec<u64>> {
    if paths.len() != writers.len() {
        return Err(Error::errno(0, libc::EINVAL as u32));
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let options = options.into();
    if let Some(budget) = options.round_robin_bytes {
        return copy_to_writers_round_robin(source, paths, writers, options.stream, budget);
    }
    copy_to_writers_ordered(source, paths, writers, options.stream)
}

fn copy_to_writers_ordered<P: AsRef<Path>, W: Write>(
    source: &impl Vfsi,
    paths: &[P],
    writers: &mut [W],
    options: StreamOptions,
) -> Result<Vec<u64>> {
    let mut written = vec![0u64; paths.len()];
    let mut write_error = None;
    let completion = source.vstream(paths, options, |index, offset, data| {
        let Some(position) = written.get_mut(index) else {
            return Err(Error::invalid_reply(None, "invalid writer stream index"));
        };
        if offset != *position {
            return Err(Error::invalid_reply(
                Some(index),
                "invalid writer stream offset",
            ));
        }
        let next = position
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::errno(index, libc::EOVERFLOW as u32))?;
        if let Err(error) = write_chunk(&mut writers[index], data, index, paths[index].as_ref()) {
            write_error = Some(error);
            return Ok(std::ops::ControlFlow::Break(()));
        }
        *position = next;
        Ok(std::ops::ControlFlow::Continue(()))
    });
    if let Some(error) = write_error {
        return Err(error);
    }
    let completion = completion?;
    if completion.len() != paths.len() {
        return Err(Error::invalid_reply(
            None,
            "invalid writer stream completion count",
        ));
    }
    for (index, completion) in completion.into_iter().enumerate() {
        if completion != StreamCompletion::Complete {
            return Err(Error::transport(Some(index), "incomplete source stream")
                .with_context("copy_to_writers", paths[index].as_ref()));
        }
    }
    Ok(written)
}

// This engine uses the portable vector operations, so backend implementations
// keep their existing ordered stream/cancellation contracts.
fn copy_to_writers_round_robin<P: AsRef<Path>, W: Write>(
    source: &impl Vfsi,
    paths: &[P],
    writers: &mut [W],
    options: StreamOptions,
    budget: NonZeroUsize,
) -> Result<Vec<u64>> {
    let budget = budget.get().min(source.limits().max_read_bytes());
    let Some(budget) = NonZeroUsize::new(budget) else {
        return Err(Error::errno(0, libc::EINVAL as u32));
    };
    let chunk = options.chunk_size().min(budget.get());
    let capacity = (budget.get() / chunk).clamp(1, 32);
    let reads = ReadOptions::new().with_max_total_bytes(budget);
    let mut written = vec![0u64; paths.len()];

    for (cohort, paths) in paths.chunks(capacity).enumerate() {
        let base = cohort * capacity;
        let requests: Vec<_> = paths
            .iter()
            .map(|path| OpenOp::new(path.as_ref(), OpenFlags::READ))
            .collect();
        let mut files = source.vopen(&requests).map_err(|error| {
            writer_stream_error(error, base, |i| (i < paths.len()).then_some(i))
        })?;
        let operation = (|| {
            if files.len() != paths.len() {
                return Err(Error::invalid_reply(
                    None,
                    "invalid writer stream open count",
                ));
            }
            let mut active: Vec<_> = (0..files.len()).collect();
            while !active.is_empty() {
                let requests: Vec<_> = active
                    .iter()
                    .map(|&i| ReadOp::range(&files[i], written[base + i], chunk))
                    .collect();
                let results = source.vread(requests, reads).map_err(|error| {
                    writer_stream_error(error, base, |i| active.get(i).copied())
                })?;
                if results.len() != active.len() {
                    return Err(Error::invalid_reply(
                        None,
                        "invalid writer stream read count",
                    ));
                }
                let mut pending = Vec::with_capacity(active.len());
                for (&local, result) in active.iter().zip(results) {
                    let index = base + local;
                    if result.offset() != written[index] {
                        return Err(Error::invalid_reply(
                            Some(index),
                            "invalid writer stream offset",
                        ));
                    }
                    let data = result.data().ok_or_else(|| {
                        Error::invalid_reply(Some(index), "missing writer stream data")
                    })?;
                    if data.len() > chunk || (data.is_empty() && !result.eof()) {
                        return Err(Error::invalid_reply(
                            Some(index),
                            "invalid writer stream progress",
                        ));
                    }
                    let next = written[index]
                        .checked_add(data.len() as u64)
                        .ok_or_else(|| Error::errno(index, libc::EOVERFLOW as u32))?;
                    if !data.is_empty() {
                        write_chunk(&mut writers[index], data, index, paths[local].as_ref())?;
                    }
                    written[index] = next;
                    if !result.eof() {
                        pending.push(local);
                    }
                }
                active = pending;
            }
            Ok(())
        })();
        // Always close the entire opened cohort, including after read/write
        // failures. Preserve the operation error when cleanup also fails.
        let count = files.len();
        let close = source
            .vclose(&mut files)
            .map_err(|error| writer_stream_error(error, base, |i| (i < count).then_some(i)));
        operation?;
        close?;
    }
    Ok(written)
}

fn write_chunk(writer: &mut impl Write, data: &[u8], index: usize, path: &Path) -> Result<()> {
    writer
        .write_all(data)
        .map_err(|error| Error::from_io(index, error).with_context("copy_to_writers", path))
}

fn writer_stream_error(
    error: Error,
    base: usize,
    input_index: impl FnOnce(usize) -> Option<usize>,
) -> Error {
    match error.index() {
        None => error,
        Some(index) => match input_index(index) {
            Some(index) => error.with_index(base + index),
            None => Error::invalid_reply(None, "invalid writer stream error index"),
        },
    }
}
