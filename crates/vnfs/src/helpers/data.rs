//! Data-only bridges leave creation, publication and metadata policy to callers.
use crate::{Error, ReadStreamOptions, Result, StreamCompletion, Vfsi, VfsiExt};
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
    options: ReadStreamOptions,
) -> Result<u64> {
    let path = path.as_ref();
    let mut written = 0u64;
    let mut write_error = None;
    let completion = source.read_stream_with_options(path, options, |_, data| {
        if let Err(error) = writer.write_all(data) {
            write_error = Some(
                Error::client(0, error.raw_os_error().unwrap_or(libc::EIO) as u32)
                    .with_context("copy_to_writer", path),
            );
            return Ok(false);
        }
        written = written
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::client(0, libc::EOVERFLOW as u32))?;
        Ok(true)
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
