//! SMB2/3 [`VecFs`] backend.
//!
//! The backend talks to Samba (or any modern SMB server) with the pure-Rust
//! `smb2` crate. Path-based stat, rename, delete, and small whole-file I/O use
//! SMB related compounds. Large transfers use the server's negotiated I/O
//! limits, and server-side copies use `FSCTL_SRV_COPYCHUNK` with an automatic
//! read/write fallback.
//! Independent small path operations in a vector call are multiplexed on the
//! authenticated connection; stateful descriptor I/O and dependent mutations
//! stay ordered.
//!
//! SMB paths are Unicode. A `Path` containing non-UTF-8 bytes is rejected with
//! `EILSEQ`; characters which are illegal in SMB names are reversibly mapped
//! by the `smb2` crate.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use futures_util::future::join_all;
use smb2::client::{ClientConfig, CompoundOp, Connection, SmbClient, Tree};
use smb2::msg::close::CloseRequest;
use smb2::msg::create::{
    CreateDisposition, CreateRequest, CreateResponse, ImpersonationLevel, ShareAccess,
};
use smb2::msg::flush::FlushRequest;
use smb2::msg::read::{ReadRequest, ReadResponse, SMB2_CHANNEL_NONE};
use smb2::msg::set_info::{InfoType, SetInfoRequest};
use smb2::msg::write::{SMB2_WRITEFLAG_WRITE_THROUGH, WriteRequest, WriteResponse};
use smb2::pack::{FileTime, ReadCursor, Unpack};
use smb2::types::flags::FileAccessMask;
use smb2::types::status::NtStatus;
use smb2::types::{Command, CreditCharge, Dialect, FileId, OplockLevel, TreeId};
use smb2::{Error as SmbError, ErrorKind as SmbErrorKind};
use tokio::runtime::{Builder, Runtime};

use crate::path::{normalize_bytes, path_bytes, path_from_bytes};
use crate::vecfs::{
    Adb, AttrMask, ERR_ACCES, ERR_EBADF, ERR_EXIST, ERR_INVAL, ERR_ISDIR, ERR_NOENT, ERR_NOTDIR,
    ExtentPair, Fd, ReadOp, ReadResult, SeekFrom, VF_CAP_SERVER_COPY, VecFs, VfAttrs, VfError,
    VfFile, VfOffset, VfPathBase, VfRes, VfResult, VfType, WriteOp, WriteResult,
};

const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_END_OF_FILE_INFORMATION: u8 = 20;

#[derive(Debug, Clone)]
struct SmbOpen {
    file_id: FileId,
    path: PathBuf,
    cur_offset: u64,
    append: bool,
    readable: bool,
    writable: bool,
}

/// A synchronous vectorized filesystem client backed by an SMB2/3 share.
///
/// `server` may be `host`, `host:port`, or `[ipv6]:port`; port 445 is added
/// when no port is present. An empty username/password requests guest access.
pub struct SmbVecFs {
    runtime: Runtime,
    client: SmbClient,
    tree: Tree,
    cwd: PathBuf,
    next_fd: Fd,
    open_files: HashMap<Fd, SmbOpen>,
    server_copy_enabled: bool,
}

impl SmbVecFs {
    /// Connect to an SMB share using NTLM credentials (or guest access when
    /// `username` and `password` are empty).
    pub fn connect(
        server: &str,
        share: &str,
        username: &str,
        password: &str,
        domain: &str,
    ) -> VfResult<Self> {
        if server.is_empty() || share.is_empty() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| VfError::transport(None, format!("create SMB runtime: {e}")))?;
        let config = ClientConfig {
            addr: normalize_server(server),
            timeout: Duration::from_secs(30),
            username: username.to_owned(),
            password: password.to_owned(),
            domain: domain.to_owned(),
            auto_reconnect: true,
            compression: true,
            dfs_enabled: true,
            dfs_target_overrides: HashMap::new(),
        };
        let (client, tree) = runtime
            .block_on(async {
                let mut client = SmbClient::connect(config).await?;
                let tree = client.connect_share(share).await?;
                Ok::<_, SmbError>((client, tree))
            })
            .map_err(|e| smb_error(e, 0))?;
        Ok(Self {
            runtime,
            client,
            tree,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: HashMap::new(),
            server_copy_enabled: cfg!(feature = "server-copy"),
        })
    }

    /// The dialect negotiated with the server.
    pub fn dialect(&self) -> Dialect {
        self.client
            .params()
            .expect("SMB negotiation completed before SmbVecFs construction")
            .dialect
    }

    fn path_string(&self, path: &Path) -> VfResult<String> {
        std::str::from_utf8(path_bytes(path))
            .map(str::to_owned)
            .map_err(|_| VfError::failure(0, libc::EILSEQ as u32))
    }

    fn file_path(&self, file: &VfFile) -> VfResult<PathBuf> {
        match file {
            VfFile::Descriptor(fd) => self
                .open_files
                .get(fd)
                .map(|open| open.path.clone())
                .ok_or_else(|| VfError::failure(0, ERR_EBADF)),
            VfFile::Saved => Err(VfError::unsupported(0)),
            _ => self.vf_path(file),
        }
    }

    fn local_path_string(&self, path: &Path) -> VfResult<String> {
        self.path_string(&self.abs_path(path))
    }

    fn insert_open_file(&mut self, open: SmbOpen) -> VfResult<Fd> {
        crate::vecfs::insert_fd(&mut self.next_fd, &mut self.open_files, open)
    }

    fn wire_path(&self, path: &str) -> String {
        let encoded = smb2::name::encode_path(path);
        if self.tree.is_dfs {
            let host = self
                .tree
                .server
                .split(':')
                .next()
                .unwrap_or(&self.tree.server);
            if encoded.is_empty() {
                format!("{host}\\{}", self.tree.share_name)
            } else {
                format!("{host}\\{}\\{encoded}", self.tree.share_name)
            }
        } else {
            encoded
        }
    }

    fn open_request(&self, path: &str, flags: i32) -> VfResult<CreateRequest> {
        let access_mode = flags & libc::O_ACCMODE;
        let readable = access_mode != libc::O_WRONLY;
        let writable = access_mode != libc::O_RDONLY;
        if flags & libc::O_TRUNC != 0 && !writable {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let mut access = FileAccessMask::FILE_READ_ATTRIBUTES | FileAccessMask::SYNCHRONIZE;
        if readable {
            access |= FileAccessMask::FILE_READ_DATA;
        }
        if writable {
            access |= FileAccessMask::FILE_WRITE_DATA | FileAccessMask::FILE_WRITE_ATTRIBUTES;
        }
        let create = flags & libc::O_CREAT != 0;
        let exclusive = flags & libc::O_EXCL != 0;
        let truncate = flags & libc::O_TRUNC != 0;
        let create_disposition = match (create, exclusive, truncate) {
            (true, true, _) => CreateDisposition::FileCreate,
            (true, false, true) => CreateDisposition::FileOverwriteIf,
            (true, false, false) => CreateDisposition::FileOpenIf,
            (false, _, true) => CreateDisposition::FileOverwrite,
            (false, _, false) => CreateDisposition::FileOpen,
        };
        let is_dir = flags & libc::O_DIRECTORY != 0;
        Ok(CreateRequest {
            requested_oplock_level: OplockLevel::None,
            impersonation_level: ImpersonationLevel::Impersonation,
            desired_access: FileAccessMask::new(access),
            file_attributes: if is_dir { 0x10 } else { FILE_ATTRIBUTE_NORMAL },
            share_access: ShareAccess(
                ShareAccess::FILE_SHARE_READ
                    | ShareAccess::FILE_SHARE_WRITE
                    | ShareAccess::FILE_SHARE_DELETE,
            ),
            create_disposition,
            create_options: if is_dir {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            },
            name: self.wire_path(path),
            create_contexts: Vec::new(),
        })
    }

    fn raw_open(&mut self, path: &str, flags: i32) -> VfResult<(FileId, u64)> {
        let request = self.open_request(path, flags)?;
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::Create,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::Create, 0)?;
        let response = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(|e| smb_error(e, 0))?;
        Ok((response.file_id, response.end_of_file))
    }

    fn raw_close(&mut self, file_id: FileId) -> VfResult<()> {
        let request = CloseRequest { flags: 0, file_id };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(
                self.client
                    .connection_mut()
                    .execute(Command::Close, &request, Some(tree_id)),
            )
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::Close, 0)
    }

    fn raw_flush(&mut self, file_id: FileId) -> VfResult<()> {
        let request = FlushRequest { file_id };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(
                self.client
                    .connection_mut()
                    .execute(Command::Flush, &request, Some(tree_id)),
            )
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::Flush, 0)
    }

    fn raw_read(&mut self, file_id: FileId, offset: u64, length: usize) -> VfResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let max_read = self
            .client
            .params()
            .map(|p| p.max_read_size as usize)
            .unwrap_or(65_536)
            .max(1);
        let tree_id = self.tree.tree_id;
        let mut data = Vec::with_capacity(length);
        let mut position = offset;
        while data.len() < length {
            let chunk = (length - data.len()).min(max_read).min(u32::MAX as usize) as u32;
            let request = ReadRequest {
                padding: 0x50,
                flags: 0,
                length: chunk,
                offset: position,
                file_id,
                minimum_count: 0,
                channel: SMB2_CHANNEL_NONE,
                remaining_bytes: 0,
                read_channel_info: Vec::new(),
            };
            let frame = self
                .runtime
                .block_on(self.client.connection_mut().execute_with_credits(
                    Command::Read,
                    &request,
                    Some(tree_id),
                    CreditCharge(credit_charge(chunk as usize)),
                ))
                .map_err(|e| smb_error(e, 0))?;
            if frame.header.status == NtStatus::END_OF_FILE {
                break;
            }
            require_status(&frame, Command::Read, 0)?;
            let response = ReadResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(|e| smb_error(e, 0))?;
            let got = response.data.len();
            data.extend_from_slice(&response.data);
            position = position.saturating_add(got as u64);
            if got < chunk as usize {
                break;
            }
        }
        Ok(data)
    }

    fn raw_write(&mut self, file_id: FileId, offset: u64, data: &[u8]) -> VfResult<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let max_write = self
            .client
            .params()
            .map(|p| p.max_write_size as usize)
            .unwrap_or(65_536)
            .max(1);
        let tree_id = self.tree.tree_id;
        let mut written = 0usize;
        while written < data.len() {
            let end = (written + max_write).min(data.len());
            let chunk = &data[written..end];
            let request = WriteRequest {
                data_offset: 0x70,
                offset: offset.saturating_add(written as u64),
                file_id,
                channel: 0,
                remaining_bytes: (data.len() - end).min(u32::MAX as usize) as u32,
                write_channel_info_offset: 0,
                write_channel_info_length: 0,
                flags: SMB2_WRITEFLAG_WRITE_THROUGH,
                data: chunk.to_vec(),
            };
            let frame = self
                .runtime
                .block_on(self.client.connection_mut().execute_with_credits(
                    Command::Write,
                    &request,
                    Some(tree_id),
                    CreditCharge(credit_charge(chunk.len())),
                ))
                .map_err(|e| smb_error(e, 0))?;
            require_status(&frame, Command::Write, 0)?;
            let response = WriteResponse::unpack(&mut ReadCursor::new(&frame.body))
                .map_err(|e| smb_error(e, 0))?;
            let count = response.count as usize;
            if count == 0 {
                return Err(VfError::transport(
                    None,
                    "SMB server reported a zero-byte successful write",
                ));
            }
            if count > chunk.len() {
                return Err(VfError::transport(
                    None,
                    "SMB server reported writing more bytes than requested",
                ));
            }
            written = written.saturating_add(count);
        }
        Ok(written)
    }

    /// Read a range from a path as CREATE + READ + CLOSE in one related
    /// compound. This is the SMB counterpart of a path-based NFSv4 COMPOUND.
    fn compound_read_path(&mut self, path: &str, offset: u64, length: usize) -> VfResult<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let max_read = self
            .client
            .params()
            .map(|p| p.max_read_size as usize)
            .unwrap_or(65_536);
        if length > max_read || length > u32::MAX as usize {
            let (file_id, _) = self.raw_open(path, libc::O_RDONLY)?;
            let result = self.raw_read(file_id, offset, length);
            let close = self.raw_close(file_id);
            return match result {
                Ok(data) => close.map(|_| data),
                Err(error) => Err(error),
            };
        }
        let create = self.open_request(path, libc::O_RDONLY)?;
        let read = ReadRequest {
            padding: 0x50,
            flags: 0,
            length: length as u32,
            offset,
            file_id: FileId::SENTINEL,
            minimum_count: 0,
            channel: SMB2_CHANNEL_NONE,
            remaining_bytes: 0,
            read_channel_info: Vec::new(),
        };
        let close = CloseRequest {
            flags: 0,
            file_id: FileId::SENTINEL,
        };
        let tree_id = self.tree.tree_id;
        let operations = [
            CompoundOp {
                command: Command::Create,
                body: &create,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::Read,
                body: &read,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(credit_charge(length)),
            },
            CompoundOp {
                command: Command::Close,
                body: &close,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
        ];
        let responses = self
            .runtime
            .block_on(self.client.connection_mut().execute_compound(&operations))
            .map_err(|e| smb_error(e, 0))?;
        let mut responses = collect_compound(responses, operations.len())?;
        require_status(&responses[0], Command::Create, 0)?;
        let opened = CreateResponse::unpack(&mut ReadCursor::new(&responses[0].body))
            .map_err(|e| smb_error(e, 0))?
            .file_id;
        if responses[1].header.status == NtStatus::END_OF_FILE {
            if responses[2].header.status != NtStatus::SUCCESS {
                let _ = self.raw_close(opened);
            }
            return Ok(Vec::new());
        }
        if let Err(error) = require_status(&responses[1], Command::Read, 0) {
            let _ = self.raw_close(opened);
            return Err(error);
        }
        let response = ReadResponse::unpack(&mut ReadCursor::new(&responses[1].body))
            .map_err(|e| smb_error(e, 0))?;
        if responses[2].header.status != NtStatus::SUCCESS {
            let _ = self.raw_close(opened);
        }
        // Release the response buffers promptly; a large read can otherwise
        // remain duplicated until this stack frame returns.
        responses.clear();
        Ok(response.data)
    }

    /// Write a range as CREATE + WRITE + FLUSH + CLOSE in one related
    /// compound when it fits the negotiated MaxWriteSize.
    fn compound_write_path(
        &mut self,
        path: &str,
        offset: u64,
        data: &[u8],
        creation: bool,
        truncate: bool,
    ) -> VfResult<usize> {
        let max_write = self
            .client
            .params()
            .map(|p| p.max_write_size as usize)
            .unwrap_or(65_536);
        let mut flags = libc::O_WRONLY;
        if creation {
            flags |= libc::O_CREAT;
        }
        if truncate {
            flags |= libc::O_TRUNC;
        }
        if data.is_empty() {
            let (file_id, _) = self.raw_open(path, flags)?;
            let result = self.raw_flush(file_id);
            let close = self.raw_close(file_id);
            return result.and(close).map(|()| 0);
        }
        if data.len() > max_write || data.len() > u32::MAX as usize {
            let (file_id, _) = self.raw_open(path, flags)?;
            let result = self
                .raw_write(file_id, offset, data)
                .and_then(|written| self.raw_flush(file_id).map(|_| written));
            let close = self.raw_close(file_id);
            return match result {
                Ok(written) => close.map(|_| written),
                Err(error) => Err(error),
            };
        }
        let create = self.open_request(path, flags)?;
        let write = WriteRequest {
            data_offset: 0x70,
            offset,
            file_id: FileId::SENTINEL,
            channel: 0,
            remaining_bytes: 0,
            write_channel_info_offset: 0,
            write_channel_info_length: 0,
            flags: 0,
            data: data.to_vec(),
        };
        let flush = FlushRequest {
            file_id: FileId::SENTINEL,
        };
        let close = CloseRequest {
            flags: 0,
            file_id: FileId::SENTINEL,
        };
        let tree_id = self.tree.tree_id;
        let operations = [
            CompoundOp {
                command: Command::Create,
                body: &create,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::Write,
                body: &write,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(credit_charge(data.len())),
            },
            CompoundOp {
                command: Command::Flush,
                body: &flush,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
            CompoundOp {
                command: Command::Close,
                body: &close,
                tree_id: Some(tree_id),
                credit_charge: CreditCharge(1),
            },
        ];
        let responses = self
            .runtime
            .block_on(self.client.connection_mut().execute_compound(&operations))
            .map_err(|e| smb_error(e, 0))?;
        let responses = collect_compound(responses, operations.len())?;
        require_status(&responses[0], Command::Create, 0)?;
        let opened = CreateResponse::unpack(&mut ReadCursor::new(&responses[0].body))
            .map_err(|e| smb_error(e, 0))?
            .file_id;
        for (position, command) in [Command::Write, Command::Flush].into_iter().enumerate() {
            if let Err(error) = require_status(&responses[position + 1], command, 0) {
                let _ = self.raw_close(opened);
                return Err(error);
            }
        }
        let response = WriteResponse::unpack(&mut ReadCursor::new(&responses[1].body))
            .map_err(|e| smb_error(e, 0))?;
        if response.count as usize > data.len() {
            return Err(VfError::transport(
                None,
                "SMB server reported writing more bytes than requested",
            ));
        }
        if responses[3].header.status != NtStatus::SUCCESS {
            let _ = self.raw_close(opened);
        }
        Ok(response.count as usize)
    }

    fn query_size(&mut self, file_id: FileId) -> VfResult<u64> {
        use smb2::msg::query_info::{InfoType, QueryInfoRequest, QueryInfoResponse};
        let request = QueryInfoRequest {
            info_type: InfoType::File,
            file_info_class: 5,
            output_buffer_length: 24,
            additional_information: 0,
            flags: 0,
            file_id,
            input_buffer: Vec::new(),
        };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::QueryInfo,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::QueryInfo, 0)?;
        let response = QueryInfoResponse::unpack(&mut ReadCursor::new(&frame.body))
            .map_err(|e| smb_error(e, 0))?;
        if response.output_buffer.len() < 16 {
            return Err(VfError::transport(
                None,
                "short SMB FileStandardInformation response",
            ));
        }
        Ok(u64::from_le_bytes(
            response.output_buffer[8..16]
                .try_into()
                .expect("length checked"),
        ))
    }

    fn set_size_handle(&mut self, file_id: FileId, size: u64) -> VfResult<()> {
        let request = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: FILE_END_OF_FILE_INFORMATION,
            additional_information: 0,
            file_id,
            buffer: size.to_le_bytes().to_vec(),
        };
        let tree_id = self.tree.tree_id;
        let frame = self
            .runtime
            .block_on(self.client.connection_mut().execute(
                Command::SetInfo,
                &request,
                Some(tree_id),
            ))
            .map_err(|e| smb_error(e, 0))?;
        require_status(&frame, Command::SetInfo, 0)
    }

    fn set_size_path(&mut self, path: &Path, size: u64) -> VfResult<()> {
        let path = self.local_path_string(path)?;
        let (file_id, _) = self.raw_open(&path, libc::O_WRONLY)?;
        let result = self.set_size_handle(file_id, size);
        let close = self.raw_close(file_id);
        result.and(close)
    }

    fn client_stat(&mut self, path: &str) -> VfResult<smb2::client::FileInfo> {
        self.runtime
            .block_on(self.client.stat(&mut self.tree, path))
            .map_err(|e| smb_error(e, 0))
    }

    fn read_whole_recovering(&mut self, path: &str) -> VfResult<Vec<u8>> {
        match self
            .runtime
            .block_on(self.client.read_file(&mut self.tree, path))
        {
            Ok(data) => Ok(data),
            Err(error) if error.kind() == SmbErrorKind::TooLarge => self
                .runtime
                .block_on(self.client.read_file_pipelined(&mut self.tree, path))
                .map_err(|error| smb_error(error, 0)),
            Err(error) if error.kind() == SmbErrorKind::Unsupported => {
                let info = self.client_stat(path)?;
                if info.is_directory {
                    Err(VfError::failure(0, ERR_ISDIR))
                } else {
                    Err(smb_error(error, 0))
                }
            }
            Err(error) => Err(smb_error(error, 0)),
        }
    }

    fn fill_attrs(a: &mut VfAttrs, info: &smb2::client::FileInfo) {
        a.ftype = if info.is_directory {
            VfType::Directory
        } else {
            VfType::Regular
        };
        a.returned = AttrMask::empty();
        if a.masks.contains(AttrMask::MODE) {
            a.mode = if info.is_directory {
                libc::S_IFDIR | 0o777
            } else {
                libc::S_IFREG | 0o666
            };
            a.returned.insert(AttrMask::MODE);
        }
        if a.masks.contains(AttrMask::SIZE) {
            a.size = info.size;
            a.returned.insert(AttrMask::SIZE);
        }
        if a.masks.contains(AttrMask::ATIME) {
            (a.atime_sec, a.atime_nsec) = filetime_parts(info.accessed);
            a.returned.insert(AttrMask::ATIME);
        }
        if a.masks.contains(AttrMask::MTIME) {
            (a.mtime_sec, a.mtime_nsec) = filetime_parts(info.modified);
            a.returned.insert(AttrMask::MTIME);
        }
        // The high-level SMB FileInfo does not expose change time, link count,
        // file ID, Unix ownership, blocks, or named attributes. Their returned
        // bits deliberately remain clear instead of inventing values.
    }

    fn listdir_rec(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        output: &mut Vec<VfAttrs>,
    ) -> VfResult<()> {
        let resolved = self.abs_path(dir);
        let path = self.path_string(&resolved)?;
        let entries = self
            .runtime
            .block_on(self.client.list_directory(&mut self.tree, &path))
            .map_err(|e| smb_error(e, 0))?;
        let mut taken = 0usize;
        for entry in entries {
            if matches!(entry.name.as_str(), "." | "..") {
                continue;
            }
            if max_count != 0 && taken >= max_count {
                break;
            }
            let child = resolved.join(&entry.name);
            let mut attrs = VfAttrs {
                file: VfFile::Path {
                    base: VfPathBase::Abs,
                    path: Path::new("/").join(&child),
                },
                masks,
                ftype: if entry.is_directory {
                    VfType::Directory
                } else {
                    VfType::Regular
                },
                ..VfAttrs::default()
            };
            attrs.returned = AttrMask::empty();
            if masks.contains(AttrMask::MODE) {
                attrs.mode = if entry.is_directory {
                    libc::S_IFDIR | 0o777
                } else {
                    libc::S_IFREG | 0o666
                };
                attrs.returned.insert(AttrMask::MODE);
            }
            if masks.contains(AttrMask::SIZE) {
                attrs.size = entry.size;
                attrs.returned.insert(AttrMask::SIZE);
            }
            if masks.contains(AttrMask::MTIME) {
                (attrs.mtime_sec, attrs.mtime_nsec) = filetime_parts(entry.modified);
                attrs.returned.insert(AttrMask::MTIME);
            }
            let descend = recursive && entry.is_directory;
            output.push(attrs);
            taken += 1;
            if descend {
                self.listdir_rec(&child, masks, max_count, true, output)?;
            }
        }
        Ok(())
    }

    fn read_one(&mut self, op: &ReadOp) -> VfResult<ReadResult> {
        if !op.file.is_descriptor() {
            if matches!(op.file, VfFile::Saved) {
                return Err(VfError::unsupported(0));
            }
            let path = self.path_string(&self.file_path(&op.file)?)?;
            let offset = match op.offset {
                VfOffset::At(offset) => offset,
                VfOffset::End => self.client_stat(&path)?.size,
                VfOffset::Cur => return Err(VfError::failure(0, ERR_INVAL)),
            };
            let data = match self.compound_read_path(&path, offset, op.length) {
                Ok(data) => data,
                Err(_) if self.client.is_disconnected() => {
                    let all = self.read_whole_recovering(&path)?;
                    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(all.len());
                    let end = start.saturating_add(op.length).min(all.len());
                    all[start..end].to_vec()
                }
                Err(error) => return Err(error),
            };
            let eof = op.length > 0 && data.len() < op.length;
            return Ok(ReadResult {
                file: op.file.clone(),
                offset,
                data,
                eof,
            });
        }
        let (file_id, temporary, descriptor) = match &op.file {
            VfFile::Descriptor(fd) => {
                let open = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
                if !open.readable {
                    return Err(VfError::failure(0, ERR_EBADF));
                }
                (open.file_id, false, Some(*fd))
            }
            VfFile::Saved => return Err(VfError::unsupported(0)),
            _ => {
                let path = self.path_string(&self.file_path(&op.file)?)?;
                let (id, _) = self.raw_open(&path, libc::O_RDONLY)?;
                (id, true, None)
            }
        };
        let size = self.query_size(file_id);
        let result = size.and_then(|size| {
            let offset = self.resolve_offset(descriptor, op.offset, size)?;
            let data = self.raw_read(file_id, offset, op.length)?;
            let eof = !data.is_empty() && offset.saturating_add(data.len() as u64) >= size
                || op.length > 0 && data.len() < op.length;
            Ok((offset, data, eof))
        });
        let close_result = temporary.then(|| self.raw_close(file_id));
        let (offset, data, eof) = result?;
        if let Some(close) = close_result {
            close?;
        }
        if let Some(fd) = descriptor
            && let Some(open) = self.open_files.get_mut(&fd)
        {
            open.cur_offset = offset.saturating_add(data.len() as u64);
        }
        Ok(ReadResult {
            file: op.file.clone(),
            offset,
            data,
            eof,
        })
    }

    fn write_one(&mut self, op: &WriteOp) -> VfResult<WriteResult> {
        if !op.file.is_descriptor() {
            if matches!(op.file, VfFile::Saved) {
                return Err(VfError::unsupported(0));
            }
            let path = self.path_string(&self.file_path(&op.file)?)?;
            let offset = match op.offset {
                VfOffset::At(offset) => offset,
                VfOffset::End => self.client_stat(&path)?.size,
                VfOffset::Cur => return Err(VfError::failure(0, ERR_INVAL)),
            };
            let written =
                self.compound_write_path(&path, offset, &op.data, op.creation, op.truncate)?;
            return Ok(WriteResult {
                file: op.file.clone(),
                offset,
                written,
                stable: true,
            });
        }
        let (file_id, temporary, descriptor, append) = match &op.file {
            VfFile::Descriptor(fd) => {
                let open = self
                    .open_files
                    .get(fd)
                    .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
                if !open.writable {
                    return Err(VfError::failure(0, ERR_EBADF));
                }
                (open.file_id, false, Some(*fd), open.append)
            }
            VfFile::Saved => return Err(VfError::unsupported(0)),
            _ => {
                let path = self.path_string(&self.file_path(&op.file)?)?;
                let mut flags = libc::O_WRONLY;
                if op.creation {
                    flags |= libc::O_CREAT;
                }
                if op.truncate {
                    flags |= libc::O_TRUNC;
                }
                let (id, _) = self.raw_open(&path, flags)?;
                (id, true, None, false)
            }
        };
        let size = self.query_size(file_id);
        let result = size.and_then(|size| {
            let offset = if append {
                size
            } else {
                self.resolve_offset(descriptor, op.offset, size)?
            };
            let written = self.raw_write(file_id, offset, &op.data)?;
            self.raw_flush(file_id)?;
            Ok((offset, written))
        });
        let close_result = temporary.then(|| self.raw_close(file_id));
        let (offset, written) = result?;
        if let Some(close) = close_result {
            close?;
        }
        if let Some(fd) = descriptor
            && let Some(open) = self.open_files.get_mut(&fd)
        {
            open.cur_offset = offset.saturating_add(written as u64);
        }
        Ok(WriteResult {
            file: op.file.clone(),
            offset,
            written,
            stable: true,
        })
    }

    fn resolve_offset(&self, fd: Option<Fd>, offset: VfOffset, size: u64) -> VfResult<u64> {
        match offset {
            VfOffset::At(offset) => Ok(offset),
            VfOffset::End => Ok(size),
            VfOffset::Cur => fd
                .and_then(|fd| self.open_files.get(&fd).map(|open| open.cur_offset))
                .ok_or_else(|| VfError::failure(0, ERR_INVAL)),
        }
    }

    fn copy_client_side(&mut self, pair: &ExtentPair) -> VfResult<()> {
        let source = self.local_path_string(&pair.src_path)?;
        let destination = self.local_path_string(&pair.dst_path)?;
        let (src_id, src_size) = self.raw_open(&source, libc::O_RDONLY)?;
        let (dst_id, _) = match self.raw_open(&destination, libc::O_WRONLY | libc::O_CREAT) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.raw_close(src_id);
                return Err(error);
            }
        };
        let available = src_size.saturating_sub(pair.src_offset);
        let length = pair.length.unwrap_or(available).min(available);
        let mut copied = 0u64;
        let result = (|| {
            while copied < length {
                let wanted = (length - copied).min(1 << 20) as usize;
                let data = self.raw_read(src_id, pair.src_offset + copied, wanted)?;
                if data.is_empty() {
                    break;
                }
                self.raw_write(dst_id, pair.dst_offset + copied, &data)?;
                copied += data.len() as u64;
            }
            self.set_size_handle(dst_id, pair.dst_offset.saturating_add(copied))?;
            self.raw_flush(dst_id)
        })();
        let _ = self.raw_close(dst_id);
        let _ = self.raw_close(src_id);
        result
    }

    fn rm_one(&mut self, path: &Path, recursive: bool) -> VfResult<()> {
        let attrs = self.stat(path)?;
        if attrs.ftype == VfType::Directory && recursive {
            let entries = self.listdir(path, AttrMask::empty(), 0, false)?;
            for entry in entries {
                let child = entry
                    .file
                    .path()
                    .ok_or_else(|| VfError::failure(0, ERR_INVAL))?
                    .to_path_buf();
                self.rm_one(&child, true)?;
            }
        }
        self.removev(&[VfFile::from_os_path(path)])
    }
}

impl VecFs for SmbVecFs {
    fn smb_dialect(&self) -> Option<u16> {
        Some(self.dialect() as u16)
    }

    fn capabilities(&self) -> u64 {
        if self.server_copy_enabled {
            VF_CAP_SERVER_COPY
        } else {
            0
        }
    }

    fn abs_path(&self, path: &Path) -> PathBuf {
        let root_relative = if path.is_absolute() {
            path.strip_prefix("/").unwrap_or(path).to_path_buf()
        } else {
            self.cwd.join(path)
        };
        path_from_bytes(&normalize_bytes(path_bytes(&root_relative)))
    }

    fn open_by_path(
        &mut self,
        base: VfPathBase,
        pathname: &Path,
        flags: i32,
        _mode: u32,
    ) -> VfResult<VfFile> {
        let path = match base {
            VfPathBase::Abs => path_from_bytes(&normalize_bytes(path_bytes(pathname))),
            VfPathBase::Cwd => self.abs_path(pathname),
        };
        let path_string = self.path_string(&path)?;
        let (file_id, size) = self.raw_open(&path_string, flags)?;
        let access_mode = flags & libc::O_ACCMODE;
        let fd = self.insert_open_file(SmbOpen {
            file_id,
            path,
            cur_offset: if flags & libc::O_APPEND != 0 { size } else { 0 },
            append: flags & libc::O_APPEND != 0,
            readable: access_mode != libc::O_WRONLY,
            writable: access_mode != libc::O_RDONLY,
        })?;
        Ok(VfFile::from_fd(fd))
    }

    fn openv(&mut self, paths: &[&Path], flags: &[i32], modes: &[u32]) -> VfResult<Vec<VfFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        let resolved: Vec<PathBuf> = paths.iter().map(|path| self.abs_path(path)).collect();
        if paths.len() <= 1 || self.tree.is_dfs || !paths_are_independent(&resolved) {
            let mut output = Vec::with_capacity(paths.len());
            for (index, ((path, flags), mode)) in paths.iter().zip(flags).zip(modes).enumerate() {
                output.push(
                    self.open(path, *flags, *mode)
                        .map_err(|error| error.with_index(index))?,
                );
            }
            return Ok(output);
        }

        let mut requests = Vec::with_capacity(paths.len());
        for (index, (path, flags)) in resolved.iter().zip(flags).enumerate() {
            let path_string = self
                .path_string(path)
                .map_err(|error| error.with_index(index))?;
            let request = self
                .open_request(&path_string, *flags)
                .map_err(|error| error.with_index(index))?;
            requests.push(request);
        }
        let connection = self.client.connection_mut().clone();
        let tree_id = self.tree.tree_id;
        let jobs = requests.into_iter().map(|request| {
            let connection = connection.clone();
            async move {
                let frame = connection
                    .execute(Command::Create, &request, Some(tree_id))
                    .await
                    .map_err(|error| smb_error(error, 0))?;
                require_status(&frame, Command::Create, 0)?;
                let response = CreateResponse::unpack(&mut ReadCursor::new(&frame.body))
                    .map_err(|error| smb_error(error, 0))?;
                Ok::<_, VfError>((response.file_id, response.end_of_file))
            }
        });
        let results = self.runtime.block_on(join_all(jobs));
        if let Some((failed, error)) = results
            .iter()
            .enumerate()
            .find_map(|(index, result)| result.as_ref().err().map(|error| (index, error.clone())))
        {
            let closes = results
                .into_iter()
                .filter_map(Result::ok)
                .map(|(file_id, _)| {
                    let connection = connection.clone();
                    async move { close_on_connection(&connection, tree_id, file_id).await }
                });
            self.runtime.block_on(join_all(closes));
            return Err(error.with_index(failed));
        }

        let mut output = Vec::with_capacity(paths.len());
        for (index, (((path, flags), _mode), result)) in resolved
            .into_iter()
            .zip(flags)
            .zip(modes)
            .zip(results)
            .enumerate()
        {
            let (file_id, size) = result.expect("all concurrent opens checked");
            let access_mode = *flags & libc::O_ACCMODE;
            let fd = match self.insert_open_file(SmbOpen {
                file_id,
                path,
                cur_offset: if *flags & libc::O_APPEND != 0 {
                    size
                } else {
                    0
                },
                append: *flags & libc::O_APPEND != 0,
                readable: access_mode != libc::O_WRONLY,
                writable: access_mode != libc::O_RDONLY,
            }) {
                Ok(fd) => fd,
                Err(error) => {
                    let _ = self.raw_close(file_id);
                    return Err(error.with_index(index));
                }
            };
            output.push(VfFile::from_fd(fd));
        }
        Ok(output)
    }

    fn close(&mut self, file: &VfFile) -> VfResult<()> {
        let fd = file.fd().ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        let open = self
            .open_files
            .remove(&fd)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        self.raw_close(open.file_id)
    }

    fn closev(&mut self, files: &[VfFile]) -> VfRes {
        let mut file_ids = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let fd = file
                .fd()
                .ok_or_else(|| VfError::failure(index, ERR_INVAL))?;
            let open = self
                .open_files
                .remove(&fd)
                .ok_or_else(|| VfError::failure(index, ERR_EBADF))?;
            file_ids.push(open.file_id);
        }
        let connection = self.client.connection_mut().clone();
        let tree_id = self.tree.tree_id;
        let jobs = file_ids.into_iter().map(|file_id| {
            let connection = connection.clone();
            async move { close_on_connection_result(&connection, tree_id, file_id).await }
        });
        let results = self.runtime.block_on(join_all(jobs));
        for (index, result) in results.into_iter().enumerate() {
            result.map_err(|error| error.with_index(index))?;
        }
        Ok(())
    }

    fn chdir(&mut self, path: &Path) -> VfResult<()> {
        let attrs = self.stat(path)?;
        if attrs.ftype != VfType::Directory {
            return Err(VfError::failure(0, ERR_NOTDIR));
        }
        self.cwd = self.abs_path(path);
        Ok(())
    }

    fn getcwd(&self) -> PathBuf {
        Path::new("/").join(&self.cwd)
    }

    fn readv(&mut self, reads: &[ReadOp]) -> VfResult<Vec<ReadResult>> {
        let max_read = self
            .client
            .params()
            .map(|params| params.max_read_size as usize)
            .unwrap_or(65_536);
        let concurrent = reads.len() > 1
            && reads.iter().all(|read| {
                !read.file.is_descriptor()
                    && !matches!(read.file, VfFile::Saved)
                    && matches!(read.offset, VfOffset::At(_))
                    && read.length > 0
                    && read.length <= max_read
                    && read.length <= u32::MAX as usize
            });
        if concurrent {
            let connection = self.client.connection_mut().clone();
            let tree_id = self.tree.tree_id;
            let mut jobs = Vec::with_capacity(reads.len());
            for (index, read) in reads.iter().enumerate() {
                let path = self
                    .path_string(
                        &self
                            .file_path(&read.file)
                            .map_err(|e| e.with_index(index))?,
                    )
                    .map_err(|e| e.with_index(index))?;
                let create = self
                    .open_request(&path, libc::O_RDONLY)
                    .map_err(|e| e.with_index(index))?;
                let VfOffset::At(offset) = read.offset else {
                    unreachable!("concurrent read eligibility checked")
                };
                jobs.push(concurrent_compound_read(
                    connection.clone(),
                    tree_id,
                    create,
                    offset,
                    read.length,
                ));
            }
            let results = self.runtime.block_on(join_all(jobs));
            if results.iter().any(Result::is_err) && self.client.is_disconnected() {
                let mut output = Vec::with_capacity(reads.len());
                for (index, read) in reads.iter().enumerate() {
                    output.push(
                        self.read_one(read)
                            .map_err(|error| error.with_index(index))?,
                    );
                }
                return Ok(output);
            }
            let mut output = Vec::with_capacity(reads.len());
            for (index, (read, result)) in reads.iter().zip(results).enumerate() {
                let data = result.map_err(|error| error.with_index(index))?;
                let VfOffset::At(offset) = read.offset else {
                    unreachable!("concurrent read eligibility checked")
                };
                output.push(ReadResult {
                    file: read.file.clone(),
                    offset,
                    eof: data.len() < read.length,
                    data,
                });
            }
            return Ok(output);
        }
        let mut output = Vec::with_capacity(reads.len());
        for (index, read) in reads.iter().enumerate() {
            output.push(self.read_one(read).map_err(|e| e.with_index(index))?);
        }
        Ok(output)
    }

    fn read_allv(&mut self, files: &[VfFile]) -> VfResult<Vec<Vec<u8>>> {
        if files.len() > 1
            && !self.tree.is_dfs
            && files
                .iter()
                .all(|file| !file.is_descriptor() && !matches!(file, VfFile::Saved))
        {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let mut jobs = Vec::with_capacity(files.len());
            let mut paths = Vec::with_capacity(files.len());
            for (index, file) in files.iter().enumerate() {
                let path = self
                    .path_string(&self.file_path(file).map_err(|e| e.with_index(index))?)
                    .map_err(|e| e.with_index(index))?;
                paths.push(path.clone());
                jobs.push(concurrent_read_whole(
                    tree.clone(),
                    connection.clone(),
                    path,
                ));
            }
            let results = self.runtime.block_on(join_all(jobs));
            if results.iter().any(Result::is_err) && self.client.is_disconnected() {
                let mut output = Vec::with_capacity(paths.len());
                for (index, path) in paths.iter().enumerate() {
                    output.push(
                        self.read_whole_recovering(path)
                            .map_err(|error| error.with_index(index))?,
                    );
                }
                return Ok(output);
            }
            return results
                .into_iter()
                .enumerate()
                .map(|(index, result)| result.map_err(|error| error.with_index(index)))
                .collect();
        }
        let mut output = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            if file.is_descriptor() {
                let fd = file.fd().expect("descriptor checked");
                let id = self
                    .open_files
                    .get(&fd)
                    .ok_or_else(|| VfError::failure(index, ERR_EBADF))?
                    .file_id;
                let size = self.query_size(id).map_err(|e| e.with_index(index))?;
                output.push(
                    self.raw_read(id, 0, usize::try_from(size).unwrap_or(usize::MAX))
                        .map_err(|e| e.with_index(index))?,
                );
            } else {
                let path = self
                    .path_string(&self.file_path(file).map_err(|e| e.with_index(index))?)
                    .map_err(|e| e.with_index(index))?;
                let data = self
                    .read_whole_recovering(&path)
                    .map_err(|error| error.with_index(index))?;
                output.push(data);
            }
        }
        Ok(output)
    }

    fn writev(&mut self, writes: &[WriteOp]) -> VfResult<Vec<WriteResult>> {
        let max_write = self
            .client
            .params()
            .map(|params| params.max_write_size as usize)
            .unwrap_or(65_536);
        let concurrent = writes.len() > 1
            && writes.iter().all(|write| {
                !write.file.is_descriptor()
                    && !matches!(write.file, VfFile::Saved)
                    && matches!(write.offset, VfOffset::At(_))
                    && !write.data.is_empty()
                    && write.data.len() <= max_write
                    && write.data.len() <= u32::MAX as usize
            });
        if concurrent {
            let mut prepared = Vec::with_capacity(writes.len());
            let mut paths = HashSet::with_capacity(writes.len());
            for (index, write) in writes.iter().enumerate() {
                let path = self
                    .path_string(
                        &self
                            .file_path(&write.file)
                            .map_err(|e| e.with_index(index))?,
                    )
                    .map_err(|e| e.with_index(index))?;
                if !paths.insert(path.clone()) {
                    prepared.clear();
                    break;
                }
                let mut flags = libc::O_WRONLY;
                if write.creation {
                    flags |= libc::O_CREAT;
                }
                if write.truncate {
                    flags |= libc::O_TRUNC;
                }
                let create = self
                    .open_request(&path, flags)
                    .map_err(|e| e.with_index(index))?;
                let VfOffset::At(offset) = write.offset else {
                    unreachable!("concurrent write eligibility checked")
                };
                prepared.push((create, offset, write.data.clone()));
            }
            if prepared.len() == writes.len() {
                let connection = self.client.connection_mut().clone();
                let tree_id = self.tree.tree_id;
                let jobs = prepared.into_iter().map(|(create, offset, data)| {
                    concurrent_compound_write(connection.clone(), tree_id, create, offset, data)
                });
                let results = self.runtime.block_on(join_all(jobs));
                let mut output = Vec::with_capacity(writes.len());
                for (index, (write, result)) in writes.iter().zip(results).enumerate() {
                    let written = result.map_err(|error| error.with_index(index))?;
                    let VfOffset::At(offset) = write.offset else {
                        unreachable!("concurrent write eligibility checked")
                    };
                    output.push(WriteResult {
                        file: write.file.clone(),
                        offset,
                        written,
                        stable: true,
                    });
                }
                return Ok(output);
            }
        }
        let mut output = Vec::with_capacity(writes.len());
        for (index, write) in writes.iter().enumerate() {
            output.push(self.write_one(write).map_err(|e| e.with_index(index))?);
        }
        Ok(output)
    }

    fn fseek(&mut self, file: &VfFile, offset: i64, whence: SeekFrom) -> VfResult<i64> {
        let fd = file.fd().ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
        let open = self
            .open_files
            .get(&fd)
            .ok_or_else(|| VfError::failure(0, ERR_EBADF))?;
        let base = match whence {
            SeekFrom::Set => 0i128,
            SeekFrom::Cur => open.cur_offset as i128,
            SeekFrom::End => self.query_size(open.file_id)? as i128,
        };
        let new = base + offset as i128;
        if !(0..=i64::MAX as i128).contains(&new) {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        self.open_files
            .get_mut(&fd)
            .expect("descriptor validated")
            .cur_offset = new as u64;
        Ok(new as i64)
    }

    fn getattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if attrs.len() > 1
            && !self.tree.is_dfs
            && attrs.iter().all(|attrs| !attrs.file.is_descriptor())
        {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let mut jobs = Vec::with_capacity(attrs.len());
            for (index, attrs) in attrs.iter().enumerate() {
                let path = self
                    .path_string(
                        &self
                            .file_path(&attrs.file)
                            .map_err(|e| e.with_index(index))?,
                    )
                    .map_err(|e| e.with_index(index))?;
                let tree = tree.clone();
                let mut connection = connection.clone();
                jobs.push(async move { tree.stat(&mut connection, &path).await });
            }
            let results = self.runtime.block_on(join_all(jobs));
            if results.iter().any(Result::is_err) && self.client.is_disconnected() {
                for (index, attrs) in attrs.iter_mut().enumerate() {
                    let path = self
                        .path_string(
                            &self
                                .file_path(&attrs.file)
                                .map_err(|error| error.with_index(index))?,
                        )
                        .map_err(|error| error.with_index(index))?;
                    let info = self
                        .client_stat(&path)
                        .map_err(|error| error.with_index(index))?;
                    Self::fill_attrs(attrs, &info);
                }
                return Ok(());
            }
            for (index, (attrs, result)) in attrs.iter_mut().zip(results).enumerate() {
                let info = result.map_err(|error| smb_error(error, index))?;
                Self::fill_attrs(attrs, &info);
            }
            return Ok(());
        }
        for (index, attrs) in attrs.iter_mut().enumerate() {
            let path = self
                .path_string(
                    &self
                        .file_path(&attrs.file)
                        .map_err(|e| e.with_index(index))?,
                )
                .map_err(|e| e.with_index(index))?;
            let info = self.client_stat(&path).map_err(|e| e.with_index(index))?;
            Self::fill_attrs(attrs, &info);
        }
        Ok(())
    }

    fn lgetattrsv(&mut self, attrs: &mut [VfAttrs]) -> VfRes {
        if attrs.is_empty() {
            Ok(())
        } else {
            // Standard SMB metadata follows reparse points. Reporting it as
            // lstat data would silently violate VecFs's no-follow contract.
            Err(VfError::unsupported(0))
        }
    }

    fn exists(&mut self, path: &Path) -> VfResult<bool> {
        let path = self.local_path_string(path)?;
        match self.client_stat(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.err_no() == ERR_NOENT => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn file_type(&mut self, path: &Path) -> VfResult<VfType> {
        let path = self.local_path_string(path)?;
        let info = self.client_stat(&path)?;
        Ok(if info.is_directory {
            VfType::Directory
        } else {
            VfType::Regular
        })
    }

    fn setattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        for (index, attrs) in attrs.iter().enumerate() {
            if !attrs.masks.difference(AttrMask::SIZE).is_empty() {
                return Err(VfError::unsupported(index));
            }
            if attrs.masks.contains(AttrMask::SIZE) {
                match attrs.file {
                    VfFile::Descriptor(fd) => {
                        let id = self
                            .open_files
                            .get(&fd)
                            .ok_or_else(|| VfError::failure(index, ERR_EBADF))?
                            .file_id;
                        self.set_size_handle(id, attrs.size)
                            .map_err(|e| e.with_index(index))?;
                    }
                    _ => {
                        let path = self
                            .file_path(&attrs.file)
                            .map_err(|e| e.with_index(index))?;
                        self.set_size_path(&path, attrs.size)
                            .map_err(|e| e.with_index(index))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn lsetattrsv(&mut self, attrs: &[VfAttrs]) -> VfRes {
        match attrs.iter().position(|attrs| !attrs.masks.is_empty()) {
            Some(index) => Err(VfError::unsupported(index)),
            None => Ok(()),
        }
    }

    fn listdir(
        &mut self,
        dir: &Path,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> VfResult<Vec<VfAttrs>> {
        let mut output = Vec::new();
        self.listdir_rec(dir, masks, max_count, recursive, &mut output)?;
        Ok(output)
    }

    fn renamev(&mut self, pairs: &[(VfFile, VfFile)]) -> VfRes {
        let mut prepared = Vec::with_capacity(pairs.len());
        let mut dependency_paths = Vec::with_capacity(pairs.len() * 2);
        for (index, (source, destination)) in pairs.iter().enumerate() {
            let source_path = self.file_path(source).map_err(|e| e.with_index(index))?;
            let destination_path = self
                .file_path(destination)
                .map_err(|e| e.with_index(index))?;
            let source = self
                .path_string(&source_path)
                .map_err(|e| e.with_index(index))?;
            let destination = self
                .path_string(&destination_path)
                .map_err(|e| e.with_index(index))?;
            dependency_paths.push(source_path);
            dependency_paths.push(destination_path);
            prepared.push((source, destination));
        }
        if prepared.len() > 1 && !self.tree.is_dfs && paths_are_independent(&dependency_paths) {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let jobs = prepared.into_iter().map(|(source, destination)| {
                let tree = tree.clone();
                let mut connection = connection.clone();
                async move { tree.rename(&mut connection, &source, &destination).await }
            });
            for (index, result) in self
                .runtime
                .block_on(join_all(jobs))
                .into_iter()
                .enumerate()
            {
                result.map_err(|error| smb_error(error, index))?;
            }
        } else {
            for (index, (source, destination)) in prepared.into_iter().enumerate() {
                self.runtime
                    .block_on(self.client.rename(&mut self.tree, &source, &destination))
                    .map_err(|e| smb_error(e, index))?;
            }
        }
        Ok(())
    }

    fn removev(&mut self, files: &[VfFile]) -> VfRes {
        let mut prepared = Vec::with_capacity(files.len());
        for (index, file) in files.iter().enumerate() {
            let path = self.file_path(file).map_err(|e| e.with_index(index))?;
            let wire = self.path_string(&path).map_err(|e| e.with_index(index))?;
            prepared.push((path, wire));
        }
        let path_list: Vec<PathBuf> = prepared.iter().map(|(path, _)| path.clone()).collect();
        if prepared.len() > 1 && !self.tree.is_dfs && paths_are_independent(&path_list) {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let stat_jobs = prepared.iter().map(|(_, path)| {
                let tree = tree.clone();
                let mut connection = connection.clone();
                let path = path.clone();
                async move { tree.stat(&mut connection, &path).await }
            });
            let infos = self.runtime.block_on(join_all(stat_jobs));
            let mut kinds = Vec::with_capacity(infos.len());
            for (index, result) in infos.into_iter().enumerate() {
                kinds.push(
                    result
                        .map_err(|error| smb_error(error, index))?
                        .is_directory,
                );
            }
            let delete_jobs = prepared.into_iter().zip(kinds).map(|((_, path), is_dir)| {
                let tree = tree.clone();
                let mut connection = connection.clone();
                async move {
                    if is_dir {
                        tree.delete_directory(&mut connection, &path).await
                    } else {
                        tree.delete_file(&mut connection, &path).await
                    }
                }
            });
            for (index, result) in self
                .runtime
                .block_on(join_all(delete_jobs))
                .into_iter()
                .enumerate()
            {
                result.map_err(|error| smb_error(error, index))?;
            }
        } else {
            for (index, (_, path)) in prepared.into_iter().enumerate() {
                let info = self.client_stat(&path).map_err(|e| e.with_index(index))?;
                let result = if info.is_directory {
                    self.runtime
                        .block_on(self.client.delete_directory(&mut self.tree, &path))
                } else {
                    self.runtime
                        .block_on(self.client.delete_file(&mut self.tree, &path))
                };
                result.map_err(|e| smb_error(e, index))?;
            }
        }
        Ok(())
    }

    fn mkdirv(&mut self, dirs: &[VfAttrs]) -> VfRes {
        let mut prepared = Vec::with_capacity(dirs.len());
        for (index, attrs) in dirs.iter().enumerate() {
            let path = self
                .file_path(&attrs.file)
                .map_err(|e| e.with_index(index))?;
            let wire = self.path_string(&path).map_err(|e| e.with_index(index))?;
            prepared.push((path, wire));
        }
        let path_list: Vec<PathBuf> = prepared.iter().map(|(path, _)| path.clone()).collect();
        if prepared.len() > 1 && !self.tree.is_dfs && paths_are_independent(&path_list) {
            let connection = self.client.connection_mut().clone();
            let tree = self.tree.clone();
            let jobs = prepared.into_iter().map(|(_, path)| {
                let tree = tree.clone();
                let mut connection = connection.clone();
                async move { tree.create_directory(&mut connection, &path).await }
            });
            for (index, result) in self
                .runtime
                .block_on(join_all(jobs))
                .into_iter()
                .enumerate()
            {
                result.map_err(|error| smb_error(error, index))?;
            }
        } else {
            for (index, (_, path)) in prepared.into_iter().enumerate() {
                self.runtime
                    .block_on(self.client.create_directory(&mut self.tree, &path))
                    .map_err(|e| smb_error(e, index))?;
            }
        }
        Ok(())
    }

    fn symlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        if oldpaths.is_empty() {
            Ok(())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn readlinkv(&mut self, paths: &[&Path]) -> VfResult<Vec<Vec<u8>>> {
        if paths.is_empty() {
            Ok(Vec::new())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn hardlinkv(&mut self, oldpaths: &[&Path], newpaths: &[&Path]) -> VfRes {
        if oldpaths.len() != newpaths.len() {
            return Err(VfError::failure(0, ERR_INVAL));
        }
        if oldpaths.is_empty() {
            Ok(())
        } else {
            Err(VfError::unsupported(0))
        }
    }

    fn dupv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        for (index, pair) in pairs.iter().enumerate() {
            self.copy_client_side(pair)
                .map_err(|e| e.with_index(index))?;
        }
        Ok(())
    }

    fn lcopyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        self.dupv(pairs)
    }

    fn copyv(&mut self, pairs: &[ExtentPair]) -> VfRes {
        if !self.server_copy_enabled {
            return self.dupv(pairs);
        }
        for (index, pair) in pairs.iter().enumerate() {
            let source = self
                .local_path_string(&pair.src_path)
                .map_err(|e| e.with_index(index))?;
            let destination = self
                .local_path_string(&pair.dst_path)
                .map_err(|e| e.with_index(index))?;
            let source_size = self
                .client_stat(&source)
                .map_err(|e| e.with_index(index))?
                .size;
            let length = pair
                .length
                .unwrap_or_else(|| source_size.saturating_sub(pair.src_offset))
                .min(source_size.saturating_sub(pair.src_offset));
            let result = self
                .runtime
                .block_on(self.client.server_side_copy_file_range(
                    &self.tree,
                    &source,
                    pair.src_offset,
                    &destination,
                    pair.dst_offset,
                    length,
                ));
            match result {
                Ok(copied) => self
                    .set_size_path(&pair.dst_path, pair.dst_offset.saturating_add(copied))
                    .map_err(|e| e.with_index(index))?,
                Err(error) if error.kind() == SmbErrorKind::Unsupported => {
                    self.server_copy_enabled = false;
                    return self.dupv(&pairs[index..]).map_err(|e| {
                        let relative = e.index();
                        e.with_index(index + relative)
                    });
                }
                Err(error) => return Err(smb_error(error, index)),
            }
        }
        Ok(())
    }

    fn write_adb(&mut self, patterns: &[Adb]) -> VfResult<Vec<usize>> {
        let mut counts = Vec::with_capacity(patterns.len());
        for (index, pattern) in patterns.iter().enumerate() {
            let file = self
                .open(&pattern.path, libc::O_WRONLY | libc::O_CREAT, 0o666)
                .map_err(|e| e.with_index(index))?;
            let result = (|| {
                for block in 0..pattern.adb_block_count {
                    let base = pattern
                        .adb_offset
                        .saturating_add(block as u64 * pattern.adb_block_size);
                    if let Some(relative) = pattern.adb_reloff_blocknum {
                        self.write(
                            &file,
                            base.saturating_add(relative),
                            &(pattern.adb_block_num + block as u64).to_be_bytes(),
                        )?;
                    }
                    if let Some(relative) = pattern.adb_reloff_pattern
                        && !pattern.adb_pattern_data.is_empty()
                    {
                        self.write(
                            &file,
                            base.saturating_add(relative),
                            &pattern.adb_pattern_data,
                        )?;
                    }
                }
                Ok::<_, VfError>(pattern.adb_block_count)
            })();
            let close = self.close(&file);
            counts.push(result.map_err(|e| e.with_index(index))?);
            close.map_err(|e| e.with_index(index))?;
        }
        Ok(counts)
    }

    fn rm(&mut self, objects: &[&Path], recursive: bool) -> VfRes {
        for (index, object) in objects.iter().enumerate() {
            self.rm_one(object, recursive)
                .map_err(|e| e.with_index(index))?;
        }
        Ok(())
    }

    fn cp_recursive(
        &mut self,
        source: &Path,
        destination: &Path,
        _symlinks: bool,
        use_server_side_copy: bool,
    ) -> VfRes {
        if !self.exists(destination)? {
            self.ensure_dir(destination, 0o755)?;
        }
        let entries = self.listdir(source, AttrMask::MODE | AttrMask::SIZE, 0, false)?;
        for entry in entries {
            let name = entry
                .file
                .path()
                .and_then(Path::file_name)
                .ok_or_else(|| VfError::failure(0, ERR_INVAL))?;
            let source_child = source.join(name);
            let destination_child = destination.join(name);
            if entry.ftype == VfType::Directory {
                self.cp_recursive(
                    &source_child,
                    &destination_child,
                    false,
                    use_server_side_copy,
                )?;
            } else {
                let pair = ExtentPair::from_os_paths(&source_child, 0, &destination_child, 0, None);
                if use_server_side_copy {
                    self.copyv(&[pair])?;
                } else {
                    self.dupv(&[pair])?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for SmbVecFs {
    fn drop(&mut self) {
        let file_ids: Vec<FileId> = self
            .open_files
            .drain()
            .map(|(_, open)| open.file_id)
            .collect();
        if !file_ids.is_empty() {
            let connection = self.client.connection_mut().clone();
            let tree_id = self.tree.tree_id;
            let jobs = file_ids.into_iter().map(|file_id| {
                let connection = connection.clone();
                async move { close_on_connection(&connection, tree_id, file_id).await }
            });
            self.runtime.block_on(join_all(jobs));
        }
        let _ = self
            .runtime
            .block_on(self.client.disconnect_share(&self.tree));
    }
}

fn paths_are_independent(paths: &[PathBuf]) -> bool {
    for (index, path) in paths.iter().enumerate() {
        if paths[index + 1..]
            .iter()
            .any(|other| path == other || path.starts_with(other) || other.starts_with(path))
        {
            return false;
        }
    }
    true
}

async fn concurrent_read_whole(
    tree: Tree,
    mut connection: Connection,
    path: String,
) -> VfResult<Vec<u8>> {
    match tree.read_file(&mut connection, &path).await {
        Ok(data) => Ok(data),
        Err(error) if error.kind() == SmbErrorKind::TooLarge => tree
            .read_file_pipelined(&mut connection, &path)
            .await
            .map_err(|error| smb_error(error, 0)),
        Err(error) if error.kind() == SmbErrorKind::Unsupported => {
            let info = tree
                .stat(&mut connection, &path)
                .await
                .map_err(|error| smb_error(error, 0))?;
            if info.is_directory {
                Err(VfError::failure(0, ERR_ISDIR))
            } else {
                Err(smb_error(error, 0))
            }
        }
        Err(error) => Err(smb_error(error, 0)),
    }
}

async fn close_on_connection(connection: &Connection, tree_id: TreeId, file_id: FileId) {
    let _ = close_on_connection_result(connection, tree_id, file_id).await;
}

async fn close_on_connection_result(
    connection: &Connection,
    tree_id: TreeId,
    file_id: FileId,
) -> VfRes {
    let request = CloseRequest { flags: 0, file_id };
    let frame = connection
        .execute(Command::Close, &request, Some(tree_id))
        .await
        .map_err(|error| smb_error(error, 0))?;
    require_status(&frame, Command::Close, 0)
}

async fn concurrent_compound_read(
    connection: Connection,
    tree_id: TreeId,
    create: CreateRequest,
    offset: u64,
    length: usize,
) -> VfResult<Vec<u8>> {
    let read = ReadRequest {
        padding: 0x50,
        flags: 0,
        length: length as u32,
        offset,
        file_id: FileId::SENTINEL,
        minimum_count: 0,
        channel: SMB2_CHANNEL_NONE,
        remaining_bytes: 0,
        read_channel_info: Vec::new(),
    };
    let close = CloseRequest {
        flags: 0,
        file_id: FileId::SENTINEL,
    };
    let operations = [
        CompoundOp {
            command: Command::Create,
            body: &create,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Read,
            body: &read,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(credit_charge(length)),
        },
        CompoundOp {
            command: Command::Close,
            body: &close,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
    ];
    let responses = connection
        .execute_compound(&operations)
        .await
        .map_err(|error| smb_error(error, 0))?;
    let responses = collect_compound(responses, operations.len())?;
    require_status(&responses[0], Command::Create, 0)?;
    let opened = CreateResponse::unpack(&mut ReadCursor::new(&responses[0].body))
        .map_err(|error| smb_error(error, 0))?
        .file_id;
    if responses[1].header.status == NtStatus::END_OF_FILE {
        if responses[2].header.status != NtStatus::SUCCESS {
            close_on_connection(&connection, tree_id, opened).await;
        }
        return Ok(Vec::new());
    }
    if let Err(error) = require_status(&responses[1], Command::Read, 0) {
        close_on_connection(&connection, tree_id, opened).await;
        return Err(error);
    }
    let response = ReadResponse::unpack(&mut ReadCursor::new(&responses[1].body))
        .map_err(|error| smb_error(error, 0))?;
    if responses[2].header.status != NtStatus::SUCCESS {
        close_on_connection(&connection, tree_id, opened).await;
    }
    Ok(response.data)
}

async fn concurrent_compound_write(
    connection: Connection,
    tree_id: TreeId,
    create: CreateRequest,
    offset: u64,
    data: Vec<u8>,
) -> VfResult<usize> {
    let write = WriteRequest {
        data_offset: 0x70,
        offset,
        file_id: FileId::SENTINEL,
        channel: 0,
        remaining_bytes: 0,
        write_channel_info_offset: 0,
        write_channel_info_length: 0,
        flags: 0,
        data,
    };
    let flush = FlushRequest {
        file_id: FileId::SENTINEL,
    };
    let close = CloseRequest {
        flags: 0,
        file_id: FileId::SENTINEL,
    };
    let operations = [
        CompoundOp {
            command: Command::Create,
            body: &create,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Write,
            body: &write,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(credit_charge(write.data.len())),
        },
        CompoundOp {
            command: Command::Flush,
            body: &flush,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
        CompoundOp {
            command: Command::Close,
            body: &close,
            tree_id: Some(tree_id),
            credit_charge: CreditCharge(1),
        },
    ];
    let responses = connection
        .execute_compound(&operations)
        .await
        .map_err(|error| smb_error(error, 0))?;
    let responses = collect_compound(responses, operations.len())?;
    require_status(&responses[0], Command::Create, 0)?;
    let opened = CreateResponse::unpack(&mut ReadCursor::new(&responses[0].body))
        .map_err(|error| smb_error(error, 0))?
        .file_id;
    for (position, command) in [Command::Write, Command::Flush].into_iter().enumerate() {
        if let Err(error) = require_status(&responses[position + 1], command, 0) {
            close_on_connection(&connection, tree_id, opened).await;
            return Err(error);
        }
    }
    let response = WriteResponse::unpack(&mut ReadCursor::new(&responses[1].body))
        .map_err(|error| smb_error(error, 0))?;
    if response.count as usize > write.data.len() {
        close_on_connection(&connection, tree_id, opened).await;
        return Err(VfError::transport(
            None,
            "SMB server reported writing more bytes than requested",
        ));
    }
    if responses[3].header.status != NtStatus::SUCCESS {
        close_on_connection(&connection, tree_id, opened).await;
    }
    Ok(response.count as usize)
}

fn normalize_server(server: &str) -> String {
    if server.starts_with('[') {
        if server.ends_with(']') {
            format!("{server}:445")
        } else {
            server.to_owned()
        }
    } else if server
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        server.to_owned()
    } else {
        format!("{server}:445")
    }
}

fn credit_charge(bytes: usize) -> u16 {
    let charge = bytes.max(1).div_ceil(65_536);
    u16::try_from(charge).unwrap_or(u16::MAX).max(1)
}

fn require_status(frame: &smb2::client::Frame, command: Command, index: usize) -> VfResult<()> {
    if frame.header.status == NtStatus::SUCCESS {
        Ok(())
    } else {
        Err(smb_error(
            SmbError::Protocol {
                status: frame.header.status,
                command,
            },
            index,
        ))
    }
}

fn collect_compound(
    responses: Vec<Result<smb2::client::Frame, SmbError>>,
    expected: usize,
) -> VfResult<Vec<smb2::client::Frame>> {
    if responses.len() != expected {
        return Err(VfError::transport(
            None,
            format!(
                "SMB compound returned {} responses, expected {expected}",
                responses.len()
            ),
        ));
    }
    responses
        .into_iter()
        .map(|result| result.map_err(|error| smb_error(error, 0)))
        .collect()
}

fn smb_error(error: SmbError, index: usize) -> VfError {
    let errno = match error.kind() {
        SmbErrorKind::NotFound => Some(ERR_NOENT),
        SmbErrorKind::AlreadyExists => Some(ERR_EXIST),
        SmbErrorKind::AccessDenied | SmbErrorKind::AuthRequired | SmbErrorKind::SigningRequired => {
            Some(ERR_ACCES)
        }
        SmbErrorKind::IsADirectory => Some(ERR_ISDIR),
        SmbErrorKind::NotADirectory => Some(ERR_NOTDIR),
        SmbErrorKind::DiskFull => Some(libc::ENOSPC as u32),
        SmbErrorKind::SharingViolation => Some(libc::EBUSY as u32),
        SmbErrorKind::InvalidName | SmbErrorKind::InvalidData => Some(ERR_INVAL),
        SmbErrorKind::Unsupported => Some(crate::vecfs::VF_ERR_UNSUPPORTED),
        SmbErrorKind::Other if error.status() == Some(NtStatus::DIRECTORY_NOT_EMPTY) => {
            Some(libc::ENOTEMPTY as u32)
        }
        SmbErrorKind::Other if error.status() == Some(NtStatus::INVALID_PARAMETER) => {
            Some(ERR_INVAL)
        }
        _ => None,
    };
    match errno {
        Some(err_no) => VfError::failure(index, err_no),
        None => VfError::Transport {
            index: Some(index),
            message: error.to_string(),
        },
    }
}

fn filetime_parts(time: FileTime) -> (i64, u32) {
    let Some(system) = time.to_system_time() else {
        return (0, 0);
    };
    match system.duration_since(UNIX_EPOCH) {
        Ok(duration) => (
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            duration.subsec_nanos(),
        ),
        Err(error) => {
            let duration = error.duration();
            (
                -i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                duration.subsec_nanos(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_address_defaults_to_port_445() {
        assert_eq!(normalize_server("samba.example"), "samba.example:445");
        assert_eq!(normalize_server("samba.example:1445"), "samba.example:1445");
        assert_eq!(normalize_server("[::1]"), "[::1]:445");
        assert_eq!(normalize_server("[::1]:445"), "[::1]:445");
    }

    #[test]
    fn credit_charge_rounds_up_and_never_returns_zero() {
        assert_eq!(credit_charge(0), 1);
        assert_eq!(credit_charge(1), 1);
        assert_eq!(credit_charge(65_536), 1);
        assert_eq!(credit_charge(65_537), 2);
    }

    #[test]
    fn smb_transport_failures_preserve_the_message() {
        let error = smb_error(SmbError::Disconnected, 3);
        assert_eq!(error.index(), 3);
        assert_eq!(error.err_no(), crate::vecfs::VF_ERR_RPC);
        assert!(error.to_string().contains("Disconnected"));
    }
}
