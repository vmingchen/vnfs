//! Explicit standard-I/O interoperability through the portable vector API.
use super::{
    Attributes, AttrsOptions, Error, ReadOp, ReadOptions, Target, Vfsi, VfsiExt, WriteOp,
    WriteOptions,
};
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Durability requested for every handle in a synchronization vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// File data, using fdatasync where available.
    Data,
    /// File data and metadata supported by the backend.
    All,
}

/// Borrowed standard-I/O adapter with its own cursor, initially zero.
/// Every I/O request uses the supplied client's vector engine. The file stays
/// owned by the caller. `read_to_end` and `read_to_string` bound newly collected
/// bytes by the client's read budget; on failure the cursor may have advanced.
/// A failed read with a confirmed prefix acknowledges those bytes, then reports
/// its saved failure before further I/O; collectors retain the prefix on error.
/// Explicitly allocated caller buffers remain caller-managed. `write_all` uses
/// the standard `Write` loop over single-wave vector writes: acknowledged short
/// writes advance the cursor, and interruption retries only the remaining bytes.
/// A prefix carried by a failed write is acknowledged first; the saved error is
/// reported before further I/O, on flush, or on seek. Re-establish that unresolved
/// cursor with an explicit seek after observing the error. Nonempty write_all
/// reports its saved failure even when the entire payload was acknowledged.
/// Transport failures stop the loop without replaying the failed request.
pub(super) struct StdIo<'a, C: Vfsi + ?Sized> {
    client: &'a C,
    file: &'a C::File,
    cursor: Cursor,
}

// Read prefixes have a known position; write prefixes lack an actual append
// offset. Keep each pending diagnostic with its corresponding cursor state.
enum Cursor {
    At {
        position: u64,
        pending: Option<io::Error>,
    },
    Unresolved(Option<io::Error>),
}
impl Cursor {
    fn at(position: u64) -> Self {
        Self::At {
            position,
            pending: None,
        }
    }
}
impl<'a, C: Vfsi + ?Sized> StdIo<'a, C> {
    pub(super) fn new(client: &'a C, file: &'a C::File) -> Self {
        Self {
            client,
            file,
            cursor: Cursor::at(0),
        }
    }

    fn report_pending_error(&mut self) -> io::Result<()> {
        let error = match &mut self.cursor {
            Cursor::At { pending, .. } | Cursor::Unresolved(pending) => pending,
        };
        error.take().map_or(Ok(()), Err)
    }

    fn known_position(&self) -> io::Result<u64> {
        match self.cursor {
            Cursor::At { position, .. } => Ok(position),
            Cursor::Unresolved(_) => Err(io::Error::other(
                "write prefix has no resolved cursor; inspect the error and seek explicitly",
            )),
        }
    }

    fn read_at(&mut self, buffer: &mut [u8], budget: usize) -> io::Result<(usize, bool)> {
        if buffer.is_empty() {
            return Ok((0, false));
        }
        self.report_pending_error()?;
        let position = self.known_position()?;
        let length = buffer.len().min(budget);
        if !buffer.is_empty() && length == 0 {
            return Err(Error::errno(0, libc::EFBIG as u32).into());
        }
        position
            .checked_add(length as u64)
            .ok_or_else(|| Error::errno(0, libc::EOVERFLOW as u32))?;
        let values = self.client.vread(
            [ReadOp::into(self.file, position, &mut buffer[..length])],
            ReadOptions::new().with_max_total_bytes(std::num::NonZeroUsize::new(budget).unwrap()),
        )?;
        if values.len() != 1 {
            return Err(invalid("invalid read count").into());
        }
        let mut pending = None;
        let value = match values.into_iter().next().expect("validated read count") {
            Ok(value) => value,
            Err(error) => {
                match error.progress() {
                    Some(crate::IoProgress::Read(prefix))
                        if prefix.is_buffered()
                            && prefix.offset() == position
                            && prefix.bytes_read() > 0
                            && prefix.bytes_read() <= length =>
                    {
                        let prefix = prefix.clone();
                        let mut kind = error.cause().kind();
                        // A collector may retry zero-progress interruptions, but
                        // this logical failure already delivered a prefix.
                        if kind == io::ErrorKind::Interrupted {
                            kind = io::ErrorKind::Other;
                        }
                        pending = Some(io::Error::new(kind, error));
                        prefix
                    }
                    _ => return Err(error.into_cause().with_index(0).into()),
                }
            }
        };
        if value.offset() != position
            || value.data().is_some()
            || value.bytes_read() > length
            || (length != 0 && value.bytes_read() == 0 && !value.eof())
        {
            return Err(invalid("invalid read progress").into());
        }
        let eof = pending.is_none() && value.eof();
        self.cursor = Cursor::At {
            position: position + value.bytes_read() as u64,
            pending,
        };
        Ok((value.bytes_read(), eof))
    }
    fn read_uninterrupted(
        &mut self,
        buffer: &mut [u8],
        budget: usize,
    ) -> io::Result<(usize, bool)> {
        loop {
            match self.read_at(buffer, budget) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
    fn write_at(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.report_pending_error()?;
        let position = self.known_position()?;
        // The backend applies append semantics before validating the range;
        // the requested cursor may be ignored by an append handle.
        let mut values = self.client.vwrite(
            &[WriteOp::at(self.file, position, buffer)],
            WriteOptions::new(),
        )?;
        if values.len() != 1 {
            return Err(invalid("invalid write count").into());
        }
        let value = match values.pop().expect("one write outcome") {
            Ok(value) => value,
            Err(error) => {
                let valid = match error.progress() {
                    None => true,
                    Some(crate::IoProgress::Write { bytes_written }) => {
                        error.state() != crate::FailureState::NotAttempted
                            && *bytes_written <= buffer.len()
                    }
                    Some(crate::IoProgress::Read(_) | crate::IoProgress::Stream { .. }) => false,
                };
                if !valid {
                    return Err(invalid("invalid write failure progress").into());
                }
                let mut kind = error.cause().kind();
                // The standard write_all implementation retries Interrupted.
                // Never let that replay an uncertain or acknowledged prefix.
                if kind == io::ErrorKind::Interrupted
                    && (error.state() == crate::FailureState::Unknown || error.progress().is_some())
                {
                    kind = io::ErrorKind::Other;
                }
                let confirmed = match error.progress() {
                    Some(crate::IoProgress::Write { bytes_written }) => *bytes_written,
                    _ => 0,
                };
                let error = io::Error::new(kind, error);
                if confirmed > 0 {
                    // Write must acknowledge consumed bytes. Surface the saved
                    // failure before another operation can replay its suffix.
                    // Progress has no actual append offset, so do not invent
                    // a cursor even when this adapter's requested offset is known.
                    self.cursor = Cursor::Unresolved(Some(error));
                    return Ok(confirmed);
                }
                return Err(error);
            }
        };
        if value.bytes_written > buffer.len() {
            return Err(invalid("invalid write progress").into());
        }
        // Append descriptors report the actual position selected by the backend.
        self.cursor = Cursor::at(
            value
                .offset
                .checked_add(value.bytes_written as u64)
                .ok_or_else(|| invalid("write offset overflow"))?,
        );
        Ok(value.bytes_written)
    }
}
fn invalid(message: &'static str) -> Error {
    Error::transport_with_kind(None, crate::TransportKind::InvalidReply, message)
}
impl<C: Vfsi + ?Sized> Read for StdIo<'_, C> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.read_at(buffer, self.client.limits().max_read_bytes())
            .map(|(count, _)| count)
    }
    fn read_to_end(&mut self, output: &mut Vec<u8>) -> io::Result<usize> {
        self.report_pending_error()?;
        let budget = self.client.limits().max_read_bytes();
        let mut added = 0;
        loop {
            let remaining = budget - added;
            let length = remaining.min(super::DEFAULT_READ_STREAM_CHUNK_BYTES);
            if length == 0 {
                // Distinguish exact-budget EOF from oversized content without
                // allocating beyond the budget. A failed probe advances cursor.
                let (count, _) = self.read_uninterrupted(&mut [0; 1], 1)?;
                return if count == 0 {
                    Ok(added)
                } else {
                    Err(Error::errno(0, libc::EFBIG as u32).into())
                };
            }
            // Reserve before I/O and fill caller storage directly. Reuse the
            // stream chunk policy without an additional scratch allocation.
            output
                .try_reserve(length)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            let start = output.len();
            output.resize(start + length, 0);
            let result = self.read_uninterrupted(&mut output[start..], remaining);
            output.truncate(start + result.as_ref().map_or(0, |(count, _)| *count));
            let (count, eof) = result?;
            added += count;
            if eof {
                return Ok(added);
            }
        }
    }
    fn read_to_string(&mut self, output: &mut String) -> io::Result<usize> {
        let mut bytes = Vec::new();
        let result = self.read_to_end(&mut bytes);
        match std::str::from_utf8(&bytes) {
            Ok(text) => {
                output.push_str(text);
                result
            }
            Err(_) => result.and_then(|_| Err(io::ErrorKind::InvalidData.into())),
        }
    }
}
impl<C: Vfsi + ?Sized> Write for StdIo<'_, C> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_at(buffer)
    }
    fn write_all(&mut self, mut buffer: &[u8]) -> io::Result<()> {
        if buffer.is_empty() {
            return Ok(());
        }
        // Preserve the standard short-write/interruption loop, then report
        // even an error following acknowledgement of the entire last chunk.
        while !buffer.is_empty() {
            match self.write_at(buffer) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => buffer = &buffer[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.report_pending_error()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.report_pending_error()?;
        self.client.sync_data(self.file).map_err(Into::into)
    }
}
impl<C: Vfsi + ?Sized> Seek for StdIo<'_, C> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.report_pending_error()?;
        let (base, offset) = match position {
            SeekFrom::Start(position) => {
                self.cursor = Cursor::at(position);
                return Ok(position);
            }
            SeekFrom::Current(offset) => (self.known_position()?, offset),
            SeekFrom::End(offset) => {
                let values = self.client.getattrs_all(
                    &[Target::file(self.file)],
                    AttrsOptions::new().with_fields(Attributes::SIZE),
                )?;
                let [value] = values.as_slice() else {
                    return Err(invalid("invalid metadata count").into());
                };
                (value.len().ok_or_else(|| Error::unsupported(0))?, offset)
            }
        };
        let position = i128::from(base) + i128::from(offset);
        let position =
            u64::try_from(position).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        self.cursor = Cursor::at(position);
        Ok(position)
    }
}
