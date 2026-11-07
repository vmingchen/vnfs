//! Data-only bridges leave creation, publication and metadata policy to callers.
use crate::{Error, Result, StreamCompletion, StreamOptions, Vfsi, VfsiExt};
use std::{io::Write, path::Path};

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

/// Stream corresponding source paths into caller-owned writers.
/// Uses one vector stream so native clients can prefetch bounded sibling chunks.
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
pub fn copy_to_writers<P: AsRef<Path>, W: Write>(
    source: &impl Vfsi,
    paths: &[P],
    writers: &mut [W],
    options: StreamOptions,
) -> Result<Vec<u64>> {
    if paths.len() != writers.len() {
        return Err(Error::errno(0, libc::EINVAL as u32));
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
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
        if let Err(error) = writers[index].write_all(data) {
            write_error = Some(
                Error::from_io(index, error).with_context("copy_to_writers", paths[index].as_ref()),
            );
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
