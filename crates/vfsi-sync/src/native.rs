//! Rust-native scalar and vector contracts.
//!
//! [`VecFs`] remains the compatibility/backend implementation trait. New
//! applications should bound generic code by these smaller interfaces.

use crate::*;

/// Core synchronous scalar filesystem operations.
pub trait FileSystem {
    fn capabilities(&self) -> Capabilities;
    fn open_one(&mut self, request: &OpenRequest) -> VfResult<VfFile>;
    fn close_one(&mut self, file: &VfFile) -> VfResult<()>;
    fn sync_data(&mut self, file: &VfFile) -> VfResult<()>;
    fn sync_all(&mut self, file: &VfFile) -> VfResult<()>;
    fn read_one(&mut self, request: &ReadOp) -> VfResult<ReadResult>;
    fn write_one(&mut self, request: WriteOpRef<'_>) -> VfResult<WriteResult>;
    fn seek_one(&mut self, file: &VfFile, position: std::io::SeekFrom) -> VfResult<u64>;
    fn metadata(&mut self, query: MetadataQuery) -> VfResult<VfAttrs>;
    fn set_attributes(&mut self, update: SetAttributes) -> VfResult<()>;
}

/// Optimized ordered vectors. This is deliberately separate from the scalar
/// contract so a backend can implement scalar semantics without pretending
/// to support native batching.
pub trait VectorFileSystem: FileSystem {
    fn open_many(&mut self, requests: &[OpenRequest]) -> VfResult<Vec<VfFile>>;
    fn read_many(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>>;
    fn write_many(&mut self, requests: &[WriteOpRef<'_>]) -> VfResult<Vec<WriteResult>>;
    fn read_many_outcomes(&mut self, requests: &[ReadOp]) -> BatchOutcome<ReadResult>;
    fn write_many_outcomes(&mut self, requests: &[WriteOp]) -> BatchOutcome<WriteResult>;
    fn remove_many(&mut self, files: &[VfFile]) -> BatchOutcome<()>;
    fn rename_many(&mut self, pairs: &[(VfFile, VfFile)]) -> BatchOutcome<()>;
}

impl<T: VecFs + ?Sized> FileSystem for T {
    fn capabilities(&self) -> Capabilities {
        self.typed_capabilities()
    }

    fn open_one(&mut self, request: &OpenRequest) -> VfResult<VfFile> {
        self.open(
            request.path.as_path(),
            request.flags.to_libc()?,
            request.mode,
        )
        .map_err(|error| error.with_context("open", &request.path))
    }

    fn close_one(&mut self, file: &VfFile) -> VfResult<()> {
        self.close(file)
    }
    fn sync_data(&mut self, file: &VfFile) -> VfResult<()> {
        VecFs::sync_data(self, file)
    }
    fn sync_all(&mut self, file: &VfFile) -> VfResult<()> {
        VecFs::sync_all(self, file)
    }

    fn read_one(&mut self, request: &ReadOp) -> VfResult<ReadResult> {
        self.readv(std::slice::from_ref(request))?
            .pop()
            .ok_or_else(|| VfError::transport(None, "backend returned no read result"))
    }

    fn write_one(&mut self, request: WriteOpRef<'_>) -> VfResult<WriteResult> {
        self.writev_borrowed(std::slice::from_ref(&request))?
            .pop()
            .ok_or_else(|| VfError::transport(None, "backend returned no write result"))
    }

    fn seek_one(&mut self, file: &VfFile, position: std::io::SeekFrom) -> VfResult<u64> {
        let (offset, whence) = match position {
            std::io::SeekFrom::Start(offset) => (
                i64::try_from(offset).map_err(|_| VfError::failure(0, libc::EOVERFLOW as u32))?,
                SeekFrom::Set,
            ),
            std::io::SeekFrom::End(offset) => (offset, SeekFrom::End),
            std::io::SeekFrom::Current(offset) => (offset, SeekFrom::Cur),
        };
        u64::try_from(self.fseek(file, offset, whence)?).map_err(|_| VfError::failure(0, ERR_INVAL))
    }

    fn metadata(&mut self, query: MetadataQuery) -> VfResult<VfAttrs> {
        let path = query.file.path().map(std::path::Path::to_path_buf);
        let mut attrs = VfAttrs {
            file: query.file,
            masks: query.attributes,
            ..VfAttrs::default()
        };
        let result = if query.follow_symlinks {
            self.getattrsv(std::slice::from_mut(&mut attrs))
        } else {
            self.lgetattrsv(std::slice::from_mut(&mut attrs))
        };
        result.map_err(|error| match path {
            Some(path) => error.with_context("metadata", path),
            None => error,
        })?;
        Ok(attrs)
    }

    fn set_attributes(&mut self, update: SetAttributes) -> VfResult<()> {
        let follow = update.follow_symlinks;
        let attrs = update.into_legacy();
        let path = attrs.file.path().map(std::path::Path::to_path_buf);
        let result = if follow {
            self.setattrsv(&[attrs])
        } else {
            self.lsetattrsv(&[attrs])
        };
        result.map_err(|error| match path {
            Some(path) => error.with_context("set_attributes", path),
            None => error,
        })
    }
}

impl<T: VecFs + ?Sized> VectorFileSystem for T {
    fn open_many(&mut self, requests: &[OpenRequest]) -> VfResult<Vec<VfFile>> {
        let paths: Vec<&std::path::Path> = requests
            .iter()
            .map(|request| request.path.as_path())
            .collect();
        let flags: Vec<i32> = requests
            .iter()
            .map(|request| request.flags.to_libc())
            .collect::<VfResult<_>>()?;
        let modes: Vec<u32> = requests.iter().map(|request| request.mode).collect();
        self.openv(&paths, &flags, &modes)
    }

    fn read_many(&mut self, requests: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        self.readv(requests)
    }

    fn write_many(&mut self, requests: &[WriteOpRef<'_>]) -> VfResult<Vec<WriteResult>> {
        self.writev_borrowed(requests)
    }

    fn read_many_outcomes(&mut self, requests: &[ReadOp]) -> BatchOutcome<ReadResult> {
        self.readv_outcomes(requests)
    }

    fn write_many_outcomes(&mut self, requests: &[WriteOp]) -> BatchOutcome<WriteResult> {
        self.writev_outcomes(requests)
    }

    fn remove_many(&mut self, files: &[VfFile]) -> BatchOutcome<()> {
        self.removev_outcomes(files)
    }

    fn rename_many(&mut self, pairs: &[(VfFile, VfFile)]) -> BatchOutcome<()> {
        self.renamev_outcomes(pairs)
    }
}
