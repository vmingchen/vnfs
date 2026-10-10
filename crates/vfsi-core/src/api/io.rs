//! Explicit standard-I/O interoperability through the portable vector API.
use super::{
    Attributes, AttrsOptions, Error, ReadOp, ReadOptions, Result, Target, Vfsi, WriteOp,
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
/// Explicitly allocated caller buffers remain caller-managed. `write_all` uses
/// the standard `Write` loop over single-wave vector writes: acknowledged short
/// writes advance the cursor, and interruption retries only the remaining bytes.
/// Transport failures stop the loop without replaying the failed request.
pub(super) struct StdIo<'a, C: Vfsi + ?Sized> {
    client: &'a C,
    file: &'a C::File,
    position: u64,
}
impl<'a, C: Vfsi + ?Sized> StdIo<'a, C> {
    pub(super) fn new(client: &'a C, file: &'a C::File) -> Self {
        Self {
            client,
            file,
            position: 0,
        }
    }

    fn read_at(&mut self, buffer: &mut [u8], budget: usize) -> Result<(usize, bool)> {
        let length = buffer.len().min(budget);
        if !buffer.is_empty() && length == 0 {
            return Err(Error::client(0, libc::EFBIG as u32));
        }
        self.position
            .checked_add(length as u64)
            .ok_or_else(|| Error::client(0, libc::EOVERFLOW as u32))?;
        let values = self.client.vread(
            [ReadOp::into(
                self.file,
                self.position,
                &mut buffer[..length],
            )],
            ReadOptions::new().max_total_bytes(std::num::NonZeroUsize::new(budget)),
        )?;
        let [value] = values.as_slice() else {
            return Err(invalid("invalid read count"));
        };
        if value.offset() != self.position
            || value.data().is_some()
            || value.read() > length
            || (length != 0 && value.read() == 0 && !value.eof())
        {
            return Err(invalid("invalid read progress"));
        }
        self.position += value.read() as u64;
        Ok((value.read(), value.eof()))
    }
    fn read_uninterrupted(&mut self, buffer: &mut [u8], budget: usize) -> Result<(usize, bool)> {
        loop {
            match self.read_at(buffer, budget) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
    fn write_at(&mut self, buffer: &[u8]) -> Result<usize> {
        self.position
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| Error::client(0, libc::EOVERFLOW as u32))?;
        let values = self.client.vwrite(
            &[WriteOp::at(self.file, self.position, buffer)],
            WriteOptions::new(),
        )?;
        let [value] = values.as_slice() else {
            return Err(invalid("invalid write count"));
        };
        if value.written > buffer.len() {
            return Err(invalid("invalid write progress"));
        }
        // Append descriptors report the actual position selected by the backend.
        self.position = value
            .offset
            .checked_add(value.written as u64)
            .ok_or_else(|| invalid("write offset overflow"))?;
        Ok(value.written)
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
        self.read_at(buffer, self.client.limits().read_byte_limit())
            .map(|(count, _)| count)
            .map_err(Into::into)
    }
    fn read_to_end(&mut self, output: &mut Vec<u8>) -> io::Result<usize> {
        let budget = self.client.limits().read_byte_limit();
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
                    Err(Error::client(0, libc::EFBIG as u32).into())
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
        let count = self.read_to_end(&mut bytes)?;
        let text =
            std::str::from_utf8(&bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        output.push_str(text);
        Ok(count)
    }
}
impl<C: Vfsi + ?Sized> Write for StdIo<'_, C> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_at(buffer).map_err(Into::into)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.client
            .vfsync(&[self.file], SyncMode::Data)
            .map_err(Into::into)
    }
}
impl<C: Vfsi + ?Sized> Seek for StdIo<'_, C> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let (base, offset) = match position {
            SeekFrom::Start(position) => {
                self.position = position;
                return Ok(position);
            }
            SeekFrom::Current(offset) => (self.position, offset),
            SeekFrom::End(offset) => {
                let values = self.client.vgetattrs(
                    &[Target::file(self.file)],
                    AttrsOptions::new().fields(Attributes::SIZE),
                )?;
                let [value] = values.as_slice() else {
                    return Err(invalid("invalid metadata count").into());
                };
                (value.len().ok_or_else(|| Error::unsupported(0))?, offset)
            }
        };
        let position = i128::from(base) + i128::from(offset);
        self.position =
            u64::try_from(position).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.position)
    }
}
