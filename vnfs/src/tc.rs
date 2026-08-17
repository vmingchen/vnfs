//! Rust client API mirroring the `tc_api.h` vectorized NFSv4 client.
//!
//! The C API in `tc_client/include/tc_api.h` exposes a vectorized NFSv4
//! client with functions prefixed `tc_`. This module re-implements that
//! surface in idiomatic Rust: `TxnClient` replaces the opaque module handle
//! from `tc_init()`, `TcFile` replaces the `tc_file` struct, and the vector
//! operations take Rust slices. Transactional (`tx_*`) support is not
//! provided; the `is_transaction` flag accepted by the operations is ignored
//! and every compound runs as a normal, non-transactional request.

use std::path::{Path, PathBuf};

use nfsv41_sys::*;

use crate::client::{FileHandle, NfsClient, OpenCreate};

// ---------------------------------------------------------------------------
// Constants (mirroring tc_api.h)
// ---------------------------------------------------------------------------

pub const TC_FD_NULL: i32 = -1;
pub const TC_FD_CWD: i32 = -2;
pub const TC_FD_ABS: i32 = -3;

pub const TC_OFFSET_END: u64 = u64::MAX;
pub const TC_OFFSET_CUR: u64 = u64::MAX - 1;

/// Errors that have no NFS status (transport / client side).
pub const TC_ERR_RPC: u32 = 0xFFFF_FFFF;
/// Requested feature is not implemented by this client.
pub const TC_ERR_UNSUPPORTED: u32 = 0xFFFF_FFFE;

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Index of the first failed operation plus its error number, mirroring the
/// C `tc_res` struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcError {
    pub index: usize,
    pub err_no: u32,
}

impl TcError {
    pub fn failure(index: usize, err_no: u32) -> TcError {
        TcError { index, err_no }
    }

    /// Convert a low-level [`RpcError`] (which already carries the failing op
    /// index and NFS status) into a `tc` error. `index` is the caller's
    /// operation index, which may differ from the compound-internal op index.
    pub fn from_rpc(index: usize, e: crate::error::RpcError) -> TcError {
        TcError {
            index,
            err_no: e.status,
        }
    }

    pub fn unsupported(index: usize) -> TcError {
        TcError::failure(index, TC_ERR_UNSUPPORTED)
    }
}

impl std::fmt::Display for TcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "op {} failed: {}", self.index, self.err_no)
    }
}

impl std::error::Error for TcError {}

pub type TcResult<T> = Result<T, TcError>;
/// Result of a compound-style operation: `()` on success, or the index and
/// error of the first failing operation.
pub type TcRes = TcResult<()>;

// ---------------------------------------------------------------------------
// File references
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcFileType {
    Null,
    Descriptor,
    Path,
    Handle,
    Current,
    Saved,
}

/// An open file: resolved handle, open stateid, share access and the current
/// read/write offset (for `tc_fseek` / `TC_OFFSET_CUR`).
#[derive(Debug, Clone)]
pub struct OpenFile {
    pub fh: FileHandle,
    pub stateid: stateid4,
    pub access: u32,
    pub path: String,
    pub cur_offset: u64,
}

/// A reference to a file, mirroring the C `tc_file` struct.
#[derive(Debug, Clone)]
pub struct TcFile {
    pub ftype: TcFileType,
    /// For `Path`: `TC_FD_CWD` / `TC_FD_ABS` (or an open fd for fd-relative
    /// paths). For `Descriptor`: the client-assigned file descriptor.
    pub fd: i32,
    /// The path for `Path`/`Current` variants.
    pub path: Option<PathBuf>,
    /// Open state for `Descriptor`.
    pub open: Option<OpenFile>,
}

impl TcFile {
    pub fn from_path(path: &str) -> TcFile {
        let fd = if path.starts_with('/') {
            TC_FD_ABS
        } else {
            TC_FD_CWD
        };
        TcFile {
            ftype: TcFileType::Path,
            fd,
            path: Some(PathBuf::from(path)),
            open: None,
        }
    }

    pub fn from_fd(fd: i32, open: OpenFile) -> TcFile {
        TcFile {
            ftype: TcFileType::Descriptor,
            fd,
            path: None,
            open: Some(open),
        }
    }

    /// TC_FILE_CURRENT, with an optional path relative to the client's
    /// current working directory.
    pub fn current(relpath: Option<&str>) -> TcFile {
        TcFile {
            ftype: TcFileType::Current,
            fd: -1,
            path: relpath.map(PathBuf::from),
            open: None,
        }
    }

    pub fn saved() -> TcFile {
        TcFile {
            ftype: TcFileType::Saved,
            fd: -1,
            path: None,
            open: None,
        }
    }
}

impl Default for TcFile {
    fn default() -> TcFile {
        TcFile {
            ftype: TcFileType::Null,
            fd: TC_FD_NULL,
            path: None,
            open: None,
        }
    }
}

// ---------------------------------------------------------------------------
// I/O vectors and attributes
// ---------------------------------------------------------------------------

/// One element of a readv/writev call, mirroring `struct tc_iovec`.
#[derive(Debug, Clone)]
pub struct TcIoVec {
    pub file: TcFile,
    /// IN: read/write offset.
    pub offset: u64,
    /// IN: requested bytes; OUT: bytes read/written.
    pub length: usize,
    /// IN: data to write; OUT: data read.
    pub data: Vec<u8>,
    /// IN: create the file if it does not exist (writev).
    pub is_creation: bool,
    /// OUT: this element failed.
    pub is_failure: bool,
    /// OUT: read reached end-of-file.
    pub is_eof: bool,
    /// IN/OUT: stable write.
    pub is_write_stable: bool,
}

impl TcIoVec {
    pub fn new(file: TcFile, offset: u64, length: usize, data: Vec<u8>) -> TcIoVec {
        TcIoVec {
            file,
            offset,
            length,
            data,
            is_creation: false,
            is_failure: false,
            is_eof: false,
            is_write_stable: true,
        }
    }

    pub fn from_path(path: &str, offset: u64, length: usize, data: Vec<u8>) -> TcIoVec {
        TcIoVec::new(TcFile::from_path(path), offset, length, data)
    }

    /// An iovec for an open file descriptor, for `TC_OFFSET_CUR` reads.
    pub fn from_fd(fd: i32, offset: u64, length: usize, data: Vec<u8>) -> TcIoVec {
        TcIoVec::new(
            TcFile {
                ftype: TcFileType::Descriptor,
                fd,
                path: None,
                open: None,
            },
            offset,
            length,
            data,
        )
    }
}

/// One extent to copy, mirroring `struct tc_extent_pair`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentPair {
    pub src_path: String,
    pub dst_path: String,
    pub src_offset: u64,
    pub dst_offset: u64,
    /// Bytes to copy; `u64::MAX` means "from src_offset to end of file".
    pub length: u64,
}

impl ExtentPair {
    /// `tc_fill_extent_pair()`.
    pub fn new(
        src_path: &str,
        src_offset: u64,
        dst_path: &str,
        dst_offset: u64,
        length: u64,
    ) -> ExtentPair {
        ExtentPair {
            src_path: src_path.to_string(),
            dst_path: dst_path.to_string(),
            src_offset,
            dst_offset,
            length,
        }
    }
}

/// An Application Data Block (ADB) pattern, mirroring `struct tc_adb`.
#[derive(Debug, Clone)]
pub struct Adb {
    pub path: String,
    pub adb_offset: u64,
    pub adb_block_size: u64,
    /// IN: blocks requested; OUT: blocks written.
    pub adb_block_count: usize,
    /// Relative offset within a block to write the ADBN; `u64::MAX` = none.
    pub adb_reloff_blocknum: u64,
    /// ADBN of the first ADB.
    pub adb_block_num: u64,
    /// Relative offset within a block to write the pattern; `u64::MAX` = none.
    pub adb_reloff_pattern: u64,
    pub adb_pattern_size: usize,
    pub adb_pattern_data: Vec<u8>,
}

impl Adb {
    /// An ADB writing only block numbers at `reloff_blocknum`.
    pub fn blocknum_only(
        path: &str,
        offset: u64,
        block_size: u64,
        block_count: usize,
        reloff_blocknum: u64,
        first_adbn: u64,
    ) -> Adb {
        Adb {
            path: path.to_string(),
            adb_offset: offset,
            adb_block_size: block_size,
            adb_block_count: block_count,
            adb_reloff_blocknum: reloff_blocknum,
            adb_block_num: first_adbn,
            adb_reloff_pattern: u64::MAX,
            adb_pattern_size: 0,
            adb_pattern_data: Vec::new(),
        }
    }
}

/// Presence mask for `TcAttrs`, mirroring `struct tc_attrs_masks`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttrMask {
    pub has_mode: bool,
    pub has_size: bool,
    pub has_nlink: bool,
    pub has_fileid: bool,
    pub has_blocks: bool,
    pub has_uid: bool,
    pub has_gid: bool,
    pub has_rdev: bool,
    pub has_atime: bool,
    pub has_mtime: bool,
    pub has_ctime: bool,
}

impl AttrMask {
    pub fn all() -> AttrMask {
        AttrMask {
            has_mode: true,
            has_size: true,
            has_nlink: true,
            has_fileid: true,
            has_blocks: true,
            has_uid: true,
            has_gid: true,
            has_rdev: true,
            has_atime: true,
            has_mtime: true,
            has_ctime: true,
        }
    }
}

/// File attributes, mirroring `struct tc_attrs`.
#[derive(Debug, Clone, Default)]
pub struct TcAttrs {
    pub file: TcFile,
    pub masks: AttrMask,
    pub ftype: u32,
    pub mode: u32,
    pub size: u64,
    pub nlink: u32,
    pub fileid: u64,
}

/// Parsed values of a GETATTR reply for the supported FATTR4 attributes.
#[derive(Debug, Clone, Default)]
pub struct AttrValues {
    pub ftype: Option<u32>,
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub nlink: Option<u32>,
    pub fileid: Option<u64>,
}

/// Supported FATTR4 attribute ids, in the order they are encoded.
const ATTR_IDS: [u32; 5] = [
    FATTR4_TYPE,
    FATTR4_SIZE,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
];

/// Parse a GETATTR / READDIR-entry attribute list encoded for `ATTR_IDS`.
fn parse_attrs(list: &[u8]) -> TcResult<AttrValues> {
    let mut off = 0usize;
    let mut v = AttrValues::default();
    let rd32 = |off: &mut usize| -> TcResult<u32> {
        if *off + 4 > list.len() {
            return Err(TcError::failure(0, TC_ERR_RPC));
        }
        let r = u32::from_be_bytes([list[*off], list[*off + 1], list[*off + 2], list[*off + 3]]);
        *off += 4;
        Ok(r)
    };
    let rd64 = |off: &mut usize| -> TcResult<u64> {
        if *off + 8 > list.len() {
            return Err(TcError::failure(0, TC_ERR_RPC));
        }
        let r = u64::from_be_bytes(list[*off..*off + 8].try_into().unwrap());
        *off += 8;
        Ok(r)
    };
    for id in ATTR_IDS {
        match id {
            FATTR4_TYPE => v.ftype = Some(rd32(&mut off)?),
            FATTR4_SIZE => v.size = Some(rd64(&mut off)?),
            FATTR4_FILEID => v.fileid = Some(rd64(&mut off)?),
            FATTR4_MODE => v.mode = Some(rd32(&mut off)?),
            FATTR4_NUMLINKS => v.nlink = Some(rd32(&mut off)?),
            _ => unreachable!(),
        }
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// An NFSv4.1 client exposing the `tc_api.h`-style API. Created with
/// [`TxnClient::connect`] (the analog of `tc_init`); it tears down the
/// session and destroys the clientid on the server when dropped (the analog
/// of `tc_deinit`).
pub struct TxnClient {
    nfs: NfsClient,
    cwd: PathBuf,
    next_fd: i32,
    /// Canonical open-file state, keyed by the client-assigned descriptor.
    open_files: std::collections::HashMap<i32, OpenFile>,
}

impl TxnClient {
    /// Connect to the NFS server at `host` and resolve the export root.
    pub fn connect(host: &str) -> TcResult<TxnClient> {
        let nfs = NfsClient::connect(host).map_err(|e| TcError::from_rpc(0, e))?;
        Ok(TxnClient {
            nfs,
            cwd: PathBuf::new(),
            next_fd: 0,
            open_files: std::collections::HashMap::new(),
        })
    }

    // -- path handling ------------------------------------------------------

    /// Resolve `path` (absolute, or relative to the client cwd) to a handle.
    fn resolve(&mut self, path: &str) -> TcResult<FileHandle> {
        let rel = if path.starts_with('/') {
            path.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(path).to_string_lossy().to_string()
        };
        self.nfs.resolve(&rel).map_err(|e| TcError::from_rpc(0, e))
    }

    /// Split `path` into its parent directory path and final component.
    fn split_path(path: &str) -> TcResult<(&str, &str)> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Err(TcError::failure(0, nfsstat4_NFS4ERR_NOENT));
        }
        match trimmed.rfind('/') {
            Some(i) => Ok((&trimmed[..i], &trimmed[i + 1..])),
            None => Ok(("", trimmed)),
        }
    }

    fn join_path(dir: &str, name: &str) -> String {
        if dir.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", dir, name)
        }
    }

    // -- open / close -------------------------------------------------------

    /// Map fcntl-style flags to an NFSv4 share access mode.
    fn flags_to_access(flags: i32) -> u32 {
        use libc::{O_RDWR, O_WRONLY};
        if flags & O_RDWR != 0 {
            OPEN4_SHARE_ACCESS_BOTH
        } else if flags & O_WRONLY != 0 {
            OPEN4_SHARE_ACCESS_WRITE
        } else {
            OPEN4_SHARE_ACCESS_READ
        }
    }

    fn open_impl(
        &mut self,
        dir: &str,
        name: &str,
        access: u32,
        create: bool,
        excl: bool,
    ) -> TcResult<(FileHandle, stateid4)> {
        let dirfh = self.nfs.resolve(dir).map_err(|e| TcError::from_rpc(0, e))?;
        let mode = match (create, excl) {
            (false, _) => OpenCreate::NoCreate,
            (true, true) => OpenCreate::Exclusive,
            (true, false) => OpenCreate::Guarded,
        };
        self.nfs
            .open(&dirfh, name, access, mode)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    /// Open a file by path, similar to `tc_open_by_path(2)`. `dirfd` may be
    /// `TC_FD_CWD` or `TC_FD_ABS`; fd-relative paths are not supported. When
    /// `O_CREAT` is set, `mode` is applied to the new file.
    pub fn open_by_path(
        &mut self,
        dirfd: i32,
        pathname: &str,
        flags: i32,
        mode: u32,
    ) -> TcResult<TcFile> {
        use libc::{O_CREAT, O_EXCL, O_TRUNC};
        let full = if pathname.starts_with('/') {
            pathname.trim_start_matches('/').to_string()
        } else if dirfd == TC_FD_CWD {
            self.cwd.join(pathname).to_string_lossy().to_string()
        } else {
            return Err(TcError::unsupported(0));
        };
        let (dir, name) = Self::split_path(&full)?;
        let access = Self::flags_to_access(flags);
        let create = flags & O_CREAT != 0;
        let excl = flags & O_EXCL != 0;
        let (fh, stateid) = self.open_impl(dir, name, access, create, excl)?;
        if create {
            self.nfs
                .setattr(&fh, Some(mode & 0o7777), None)
                .map_err(|e| TcError::from_rpc(0, e))?;
        }
        if flags & O_TRUNC != 0 {
            self.nfs
                .setattr(&fh, None, Some(0))
                .map_err(|e| TcError::from_rpc(0, e))?;
        }
        self.next_fd += 1;
        let open = OpenFile {
            fh,
            stateid,
            access,
            path: full,
            cur_offset: 0,
        };
        self.open_files.insert(self.next_fd, open.clone());
        Ok(TcFile::from_fd(self.next_fd, open))
    }

    /// Open a file by path, `tc_open()`.
    pub fn open(&mut self, pathname: &str, flags: i32, mode: u32) -> TcResult<TcFile> {
        self.open_by_path(TC_FD_CWD, pathname, flags, mode)
    }

    /// Close an open file, `tc_close()`.
    pub fn close(&mut self, tcf: &TcFile) -> TcResult<()> {
        if tcf.ftype != TcFileType::Descriptor {
            return Err(TcError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let open = self
            .open_files
            .remove(&tcf.fd)
            .unwrap_or_else(|| tcf.open.clone().expect("open file state"));
        self.nfs
            .close(&open.fh, &open.stateid)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    /// Open several files at once, each with its own flags and mode,
    /// `tc_openv()`. The OPENs are coalesced into as few compounds as
    /// possible.
    pub fn openv(&mut self, paths: &[&str], flags: &[i32], modes: &[u32]) -> TcResult<Vec<TcFile>> {
        if paths.len() != flags.len() || paths.len() != modes.len() {
            return Err(TcError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        self.openv_impl(paths, flags, modes)
    }

    /// Open several files at once with a shared flags and mode,
    /// `tc_openv_simple()`.
    pub fn openv_simple(&mut self, paths: &[&str], flags: i32, mode: u32) -> TcResult<Vec<TcFile>> {
        let flags_v = vec![flags; paths.len()];
        let modes_v = vec![mode; paths.len()];
        self.openv_impl(paths, &flags_v, &modes_v)
    }

    /// Shared implementation of the openv variants using batched OPENs.
    fn openv_impl(
        &mut self,
        paths: &[&str],
        flags: &[i32],
        modes: &[u32],
    ) -> TcResult<Vec<TcFile>> {
        use libc::{O_CREAT, O_EXCL, O_TRUNC};
        let mut opens = Vec::with_capacity(paths.len());
        let mut accesses = Vec::with_capacity(paths.len());
        let mut fulls = Vec::with_capacity(paths.len());
        let mut need_mode = Vec::with_capacity(paths.len());
        let mut need_trunc = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let full = if p.starts_with('/') {
                p.trim_start_matches('/').to_string()
            } else {
                self.cwd.join(p).to_string_lossy().to_string()
            };
            let (dir, name) = Self::split_path(&full).map_err(|e| TcError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| TcError::from_rpc(i, e))?;
            let access = Self::flags_to_access(flags[i]);
            let create = flags[i] & O_CREAT != 0;
            let excl = flags[i] & O_EXCL != 0;
            let cmode = match (create, excl) {
                (false, _) => OpenCreate::NoCreate,
                (true, true) => OpenCreate::Exclusive,
                (true, false) => OpenCreate::Guarded,
            };
            opens.push(crate::client::OpenOp {
                dir: dirfh,
                name: name.to_string(),
                access,
                create: cmode,
            });
            accesses.push(access);
            fulls.push(full);
            need_mode.push(create);
            need_trunc.push(flags[i] & O_TRUNC != 0);
        }

        let results = self
            .nfs
            .open_many(&opens)
            .map_err(|e| TcError::from_rpc(0, e))?;

        // Apply per-file mode / O_TRUNC with a single SETATTR compound.
        let mut setattr_ops = Vec::new();
        for (i, (fh, _)) in results.iter().enumerate() {
            if need_mode[i] {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: Some(modes[i] & 0o7777),
                    size: None,
                });
            }
            if need_trunc[i] {
                setattr_ops.push(crate::client::SetattrOp {
                    fh: fh.clone(),
                    mode: None,
                    size: Some(0),
                });
            }
        }
        if !setattr_ops.is_empty() {
            self.nfs
                .setattr_many(&setattr_ops)
                .map_err(|e| TcError::from_rpc(0, e))?;
        }

        let mut out = Vec::with_capacity(results.len());
        for ((fh, stateid), (access, full)) in
            results.into_iter().zip(accesses.into_iter().zip(fulls))
        {
            self.next_fd += 1;
            let open = OpenFile {
                fh,
                stateid,
                access,
                path: full,
                cur_offset: 0,
            };
            self.open_files.insert(self.next_fd, open.clone());
            out.push(TcFile::from_fd(self.next_fd, open));
        }
        Ok(out)
    }

    /// Close several files, `tc_closev()`. The CLOSEs are coalesced into as
    /// few compounds as possible.
    pub fn closev(&mut self, files: &[TcFile]) -> TcRes {
        let mut ops = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            if f.ftype != TcFileType::Descriptor {
                return Err(TcError::failure(i, nfsstat4_NFS4ERR_INVAL));
            }
            let open = match self.open_files.remove(&f.fd) {
                Some(o) => o,
                None => f.open.clone().expect("open file state"),
            };
            ops.push(crate::client::CloseOp {
                fh: open.fh,
                stateid: open.stateid,
            });
        }
        self.nfs
            .close_many(&ops)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    // -- current working directory -----------------------------------------

    /// Change the client's current directory, `tc_chdir()`.
    pub fn chdir(&mut self, path: &str) -> TcResult<()> {
        let _fh = self.resolve(path)?; // verify it exists
        self.cwd = if path.starts_with('/') {
            PathBuf::from(path.trim_start_matches('/'))
        } else {
            self.cwd.join(path)
        };
        Ok(())
    }

    /// Current working directory, `tc_getcwd()`.
    pub fn getcwd(&self) -> String {
        format!("/{}", self.cwd.to_string_lossy())
    }

    // -- readv / writev -----------------------------------------------------

    /// Resolve the file of an iovec to an (fh, stateid) pair, opening it
    /// implicitly for path-based iovecs (`tc_readv`/`tc_writev` semantics).
    /// The third element reports whether the file was opened here and must be
    /// closed again (to avoid leaving open-owner state that would make the
    /// client undestroyable).
    fn resolve_iov_file(
        &mut self,
        iov: &TcIoVec,
        for_write: bool,
    ) -> TcResult<(FileHandle, stateid4, bool)> {
        match &iov.file {
            f if f.ftype == TcFileType::Descriptor => {
                let o = self
                    .open_files
                    .get(&f.fd)
                    .cloned()
                    .or_else(|| f.open.clone())
                    .ok_or_else(|| TcError::failure(0, nfsstat4_NFS4ERR_BAD_STATEID))?;
                Ok((o.fh, o.stateid, false))
            }
            f if f.ftype == TcFileType::Path || f.ftype == TcFileType::Current => {
                let path = match f.path.as_ref() {
                    Some(p) => p.to_string_lossy().to_string(),
                    None => return Err(TcError::failure(0, nfsstat4_NFS4ERR_NOENT)),
                };
                let full = if path.starts_with('/') {
                    path.trim_start_matches('/').to_string()
                } else {
                    self.cwd.join(&path).to_string_lossy().to_string()
                };
                let (dir, name) = Self::split_path(&full)?;
                let access = if for_write {
                    OPEN4_SHARE_ACCESS_BOTH
                } else {
                    OPEN4_SHARE_ACCESS_READ
                };
                let (fh, sid) = self.open_impl(dir, name, access, iov.is_creation, false)?;
                Ok((fh, sid, true))
            }
            _ => Err(TcError::unsupported(0)),
        }
    }

    /// Read from one or more files, `tc_readv()`. When every iovec references
    /// an already-open file, the reads are coalesced into as few compounds as
    /// possible (one `[PUTFH, READ]` pair per iovec); path-based iovecs fall
    /// back to open/read/close per file.
    ///
    /// Multiple READ ops per compound require an nfs-ganesha server; the
    /// kernel nfsd does not serve them correctly.
    pub fn readv(&mut self, reads: &mut [TcIoVec], _is_transaction: bool) -> TcRes {
        for iov in reads.iter_mut() {
            iov.is_failure = false;
            iov.is_eof = false;
        }
        if !reads.is_empty() && reads.iter().all(|i| i.file.ftype == TcFileType::Descriptor) {
            return self.readv_batch(reads);
        }
        for (i, iov) in reads.iter_mut().enumerate() {
            let res = self.readv_one(iov);
            match res {
                Ok(()) => {}
                Err(e) => {
                    iov.is_failure = true;
                    iov.length = 0;
                    return Err(TcError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            }
        }
        Ok(())
    }

    /// Batched readv for open (descriptor) iovecs: one compound per chunk of
    /// files, each carrying `[PUTFH, READ]` for every iovec.
    fn readv_batch(&mut self, reads: &mut [TcIoVec]) -> TcRes {
        let mut ops = Vec::with_capacity(reads.len());
        let mut offsets = Vec::with_capacity(reads.len());
        for (i, iov) in reads.iter_mut().enumerate() {
            let off = match self.resolve_offset(&iov.file, iov.offset) {
                Ok(off) => off,
                Err(e) => {
                    iov.is_failure = true;
                    return Err(TcError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            };
            let o = match self
                .open_files
                .get(&iov.file.fd)
                .cloned()
                .or_else(|| iov.file.open.clone())
            {
                Some(o) => o,
                None => {
                    iov.is_failure = true;
                    return Err(TcError::failure(i, nfsstat4_NFS4ERR_BAD_STATEID));
                }
            };
            ops.push(crate::client::ReadOp {
                fh: o.fh,
                stateid: o.stateid,
                offset: off,
                count: iov.length.min(u32::MAX as usize) as u32,
            });
            offsets.push(off);
        }
        match self.nfs.readv(&ops) {
            Ok(results) => {
                for ((iov, data), off) in reads.iter_mut().zip(results).zip(offsets) {
                    let requested = iov.length;
                    iov.data = data;
                    iov.length = iov.data.len();
                    iov.is_eof = iov.data.len() < requested;
                    self.advance_offset(&iov.file, off + iov.data.len() as u64);
                }
                Ok(())
            }
            Err(e) => {
                let idx = e.op_index.saturating_sub(2) / 2;
                let idx = idx.min(reads.len());
                for (i, iov) in reads.iter_mut().enumerate() {
                    if i >= idx {
                        iov.is_failure = true;
                        iov.length = 0;
                    }
                }
                Err(TcError {
                    index: idx,
                    err_no: e.status,
                })
            }
        }
    }

    /// Resolve a special offset (`TC_OFFSET_CUR` / `TC_OFFSET_END`) to a
    /// concrete file offset; plain offsets pass through.
    fn resolve_offset(&mut self, file: &TcFile, off: u64) -> TcResult<u64> {
        if off == TC_OFFSET_CUR {
            if file.ftype == TcFileType::Descriptor {
                Ok(self
                    .open_files
                    .get(&file.fd)
                    .map(|o| o.cur_offset)
                    .unwrap_or(0))
            } else {
                Ok(0)
            }
        } else if off == TC_OFFSET_END {
            let fh = self.resolve_tcfile(file)?;
            self.file_size(&fh)
        } else {
            Ok(off)
        }
    }

    /// Record the new read/write offset of an open (descriptor) file.
    fn advance_offset(&mut self, file: &TcFile, new_offset: u64) {
        if file.ftype == TcFileType::Descriptor {
            if let Some(o) = self.open_files.get_mut(&file.fd) {
                o.cur_offset = new_offset;
            }
        }
    }

    /// Reposition the read/write offset of an open file, `tc_fseek()`.
    /// `whence` is `SEEK_SET`, `SEEK_CUR` or `SEEK_END`.
    pub fn fseek(&mut self, tcf: &mut TcFile, offset: i64, whence: i32) -> TcResult<i64> {
        use libc::{SEEK_CUR, SEEK_END, SEEK_SET};
        if tcf.ftype != TcFileType::Descriptor {
            return Err(TcError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let cur = self
            .open_files
            .get(&tcf.fd)
            .map(|o| o.cur_offset)
            .ok_or_else(|| TcError::failure(0, nfsstat4_NFS4ERR_BAD_STATEID))?;
        let new = match whence {
            SEEK_SET => offset,
            SEEK_CUR => cur as i64 + offset,
            SEEK_END => {
                let fh = self.open_files.get(&tcf.fd).map(|o| o.fh.clone()).unwrap();
                let size = self.file_size(&fh)?;
                size as i64 + offset
            }
            _ => return Err(TcError::failure(0, nfsstat4_NFS4ERR_INVAL)),
        };
        if new < 0 {
            return Err(TcError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let new = new as u64;
        self.advance_offset(tcf, new);
        if let Some(o) = tcf.open.as_mut() {
            o.cur_offset = new;
        }
        Ok(new as i64)
    }

    fn readv_one(&mut self, iov: &mut TcIoVec) -> TcResult<()> {
        let (fh, stateid, close_after) = self.resolve_iov_file(iov, false)?;
        let want = iov.length.min(u32::MAX as usize) as u32;
        let mut offset = self.resolve_offset(&iov.file, iov.offset)?;
        let mut got = Vec::new();
        let result = loop {
            let remaining = want.saturating_sub(got.len() as u32);
            let chunk = self
                .nfs
                .read(&fh, &stateid, offset, remaining)
                .map_err(|e| TcError::from_rpc(0, e))?;
            if chunk.is_empty() {
                iov.is_eof = true;
                break Ok(());
            }
            let before = got.len();
            got.extend_from_slice(&chunk);
            offset += chunk.len() as u64;
            if chunk.len() < (want as usize).saturating_sub(before) {
                iov.is_eof = true;
            }
            if chunk.len() < remaining as usize || got.len() >= want as usize {
                break Ok(());
            }
        };
        if close_after {
            let _ = self.nfs.close(&fh, &stateid);
        }
        result?;
        iov.length = got.len();
        iov.data = got;
        self.advance_offset(&iov.file, offset);
        Ok(())
    }

    /// Write to one or more files, `tc_writev()`. Batches writes to open
    /// (descriptor) files into few compounds; path-based iovecs fall back to
    /// open/write/close per file.
    pub fn writev(&mut self, writes: &mut [TcIoVec], _is_transaction: bool) -> TcRes {
        for iov in writes.iter_mut() {
            iov.is_failure = false;
            iov.is_eof = false;
        }
        if !writes.is_empty()
            && writes
                .iter()
                .all(|i| i.file.ftype == TcFileType::Descriptor)
        {
            return self.writev_batch(writes);
        }
        for (i, iov) in writes.iter_mut().enumerate() {
            match self.writev_one(iov) {
                Ok(()) => {}
                Err(e) => {
                    iov.is_failure = true;
                    iov.length = 0;
                    return Err(TcError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            }
        }
        Ok(())
    }

    /// Batched writev for open (descriptor) iovecs.
    fn writev_batch(&mut self, writes: &mut [TcIoVec]) -> TcRes {
        let mut ops = Vec::with_capacity(writes.len());
        let mut offsets = Vec::with_capacity(writes.len());
        for (i, iov) in writes.iter_mut().enumerate() {
            let off = match self.resolve_offset(&iov.file, iov.offset) {
                Ok(off) => off,
                Err(e) => {
                    iov.is_failure = true;
                    return Err(TcError {
                        index: i,
                        err_no: e.err_no,
                    });
                }
            };
            let o = match self
                .open_files
                .get(&iov.file.fd)
                .cloned()
                .or_else(|| iov.file.open.clone())
            {
                Some(o) => o,
                None => {
                    iov.is_failure = true;
                    return Err(TcError::failure(i, nfsstat4_NFS4ERR_BAD_STATEID));
                }
            };
            ops.push(crate::client::WriteOp {
                fh: o.fh,
                stateid: o.stateid,
                offset: off,
                data: iov.data.clone(),
            });
            offsets.push(off);
        }
        match self.nfs.writev(&ops) {
            Ok(results) => {
                for ((iov, (n, committed)), off) in writes.iter_mut().zip(results).zip(offsets) {
                    iov.length = n as usize;
                    iov.is_write_stable = committed == stable_how4_FILE_SYNC4;
                    self.advance_offset(&iov.file, off + n as u64);
                }
                Ok(())
            }
            Err(e) => {
                let idx = e.op_index.saturating_sub(2) / 2;
                let idx = idx.min(writes.len());
                for (i, iov) in writes.iter_mut().enumerate() {
                    if i >= idx {
                        iov.is_failure = true;
                        iov.length = 0;
                    }
                }
                Err(TcError {
                    index: idx,
                    err_no: e.status,
                })
            }
        }
    }

    fn writev_one(&mut self, iov: &mut TcIoVec) -> TcResult<()> {
        let (fh, stateid, close_after) = self.resolve_iov_file(iov, true)?;
        let offset = self.resolve_offset(&iov.file, iov.offset)?;
        let result = self
            .nfs
            .write(&fh, &stateid, offset, &iov.data)
            .map_err(|e| TcError::from_rpc(0, e));
        if close_after {
            let _ = self.nfs.close(&fh, &stateid);
        }
        let (n, committed) = result?;
        iov.length = n as usize;
        iov.is_write_stable = committed == stable_how4_FILE_SYNC4;
        self.advance_offset(&iov.file, offset + n as u64);
        Ok(())
    }

    /// The size in bytes of `fh`.
    pub fn file_size(&mut self, fh: &FileHandle) -> TcResult<u64> {
        let list = self
            .nfs
            .getattr(fh, &[FATTR4_SIZE])
            .map_err(|e| TcError::from_rpc(0, e))?;
        let mut off = 0;
        read_u64(&list, &mut off)
    }

    // -- attributes ---------------------------------------------------------

    fn request_mask_to_attr_list(masks: &AttrMask) -> Vec<u32> {
        let mut ids = Vec::new();
        for id in ATTR_IDS {
            let wanted = match id {
                FATTR4_TYPE => true, // always fetch type (cheap, aids listdir)
                FATTR4_SIZE => masks.has_size,
                FATTR4_FILEID => masks.has_fileid,
                FATTR4_MODE => masks.has_mode,
                FATTR4_NUMLINKS => masks.has_nlink,
                _ => false,
            };
            if wanted {
                ids.push(id);
            }
        }
        ids
    }

    /// Fetch attributes for one file, filling `a` where the mask requests it.
    fn getattr_one(&mut self, a: &mut TcAttrs) -> TcResult<()> {
        let fh = self.resolve_tcfile(&a.file)?;
        let ids = Self::request_mask_to_attr_list(&a.masks);
        let list = self
            .nfs
            .getattr(&fh, &ids)
            .map_err(|e| TcError::from_rpc(0, e))?;
        let v = Self::parse_attr_list(&ids, &list)?;
        Self::apply_attrs(a, &v);
        Ok(())
    }

    /// Parse a raw GETATTR attribute list encoded for the given ids (in id
    /// order) into typed values.
    fn parse_attr_list(ids: &[u32], list: &[u8]) -> TcResult<AttrValues> {
        let mut v = AttrValues::default();
        let mut off = 0usize;
        for id in ids {
            match *id {
                FATTR4_TYPE => {
                    v.ftype = Some(read_u32(list, &mut off)?);
                }
                FATTR4_SIZE => {
                    v.size = Some(read_u64(list, &mut off)?);
                }
                FATTR4_FILEID => {
                    v.fileid = Some(read_u64(list, &mut off)?);
                }
                FATTR4_MODE => {
                    v.mode = Some(read_u32(list, &mut off)?);
                }
                FATTR4_NUMLINKS => {
                    v.nlink = Some(read_u32(list, &mut off)?);
                }
                _ => unreachable!(),
            }
        }
        Ok(v)
    }

    /// Fill `a` from parsed values where the mask requests the attribute.
    fn apply_attrs(a: &mut TcAttrs, v: &AttrValues) {
        a.ftype = v.ftype.unwrap_or(0);
        if a.masks.has_mode {
            if let Some(mode) = v.mode {
                a.mode = mode;
            }
        }
        if a.masks.has_size {
            if let Some(size) = v.size {
                a.size = size;
            }
        }
        if a.masks.has_nlink {
            if let Some(nlink) = v.nlink {
                a.nlink = nlink;
            }
        }
        if a.masks.has_fileid {
            if let Some(fileid) = v.fileid {
                a.fileid = fileid;
            }
        }
    }

    fn resolve_tcfile(&mut self, f: &TcFile) -> TcResult<FileHandle> {
        match f.ftype {
            TcFileType::Descriptor => Ok(f.open.as_ref().unwrap().fh.clone()),
            TcFileType::Path | TcFileType::Current => {
                let path = f
                    .path
                    .as_ref()
                    .ok_or_else(|| TcError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
                self.resolve(&path.to_string_lossy())
            }
            _ => Err(TcError::unsupported(0)),
        }
    }

    /// Get attributes of an array of files, `tc_getattrsv()`. All GETATTRs
    /// are coalesced into as few compounds as possible.
    pub fn getattrsv(&mut self, attrs: &mut [TcAttrs], _is_transaction: bool) -> TcRes {
        let mut ops = Vec::with_capacity(attrs.len());
        let mut ids_list = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let fh = self.resolve_tcfile(&a.file).map_err(|mut e| {
                e.index = i;
                e
            })?;
            let ids = Self::request_mask_to_attr_list(&a.masks);
            ids_list.push(ids.clone());
            ops.push(crate::client::GetattrOp { fh, attrs: ids });
        }
        let results = self
            .nfs
            .getattr_many(&ops)
            .map_err(|e| TcError::from_rpc(0, e))?;
        for ((a, ids), list) in attrs.iter_mut().zip(ids_list).zip(results) {
            let v = Self::parse_attr_list(&ids, &list)?;
            Self::apply_attrs(a, &v);
        }
        Ok(())
    }

    /// `tc_lgetattrsv()`: like getattrsv but does not follow symlinks.
    /// (This client does not implement symlink-following lookups; identical
    /// to getattrsv.)
    pub fn lgetattrsv(&mut self, attrs: &mut [TcAttrs], txn: bool) -> TcRes {
        self.getattrsv(attrs, txn)
    }

    /// Set attributes (mode / size) on an array of files, `tc_setattrsv()`.
    /// All SETATTRs are coalesced into as few compounds as possible.
    pub fn setattrsv(&mut self, attrs: &[TcAttrs], _is_transaction: bool) -> TcRes {
        let mut ops = Vec::with_capacity(attrs.len());
        for (i, a) in attrs.iter().enumerate() {
            let fh = self.resolve_tcfile(&a.file).map_err(|mut e| {
                e.index = i;
                e
            })?;
            let mode = if a.masks.has_mode { Some(a.mode) } else { None };
            let size = if a.masks.has_size { Some(a.size) } else { None };
            if mode.is_none() && size.is_none() {
                return Err(TcError {
                    index: i,
                    err_no: TC_ERR_UNSUPPORTED,
                });
            }
            ops.push(crate::client::SetattrOp { fh, mode, size });
        }
        self.nfs
            .setattr_many(&ops)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    /// `tc_lsetattrsv()`.
    pub fn lsetattrsv(&mut self, attrs: &[TcAttrs], txn: bool) -> TcRes {
        self.setattrsv(attrs, txn)
    }

    /// Stat a path, `tc_stat()`.
    pub fn stat(&mut self, path: &str) -> TcResult<TcAttrs> {
        let mut a = TcAttrs {
            file: TcFile::from_path(path),
            masks: AttrMask {
                has_mode: true,
                has_size: true,
                has_nlink: true,
                has_fileid: true,
                ..AttrMask::default()
            },
            ..TcAttrs::default()
        };
        self.getattr_one(&mut a)?;
        Ok(a)
    }

    /// `tc_lstat()`.
    pub fn lstat(&mut self, path: &str) -> TcResult<TcAttrs> {
        self.stat(path)
    }

    /// `tc_fstat()`.
    pub fn fstat(&mut self, tcf: &TcFile) -> TcResult<TcAttrs> {
        let mut a = TcAttrs {
            file: tcf.clone(),
            masks: AttrMask {
                has_mode: true,
                has_size: true,
                has_nlink: true,
                has_fileid: true,
                ..AttrMask::default()
            },
            ..TcAttrs::default()
        };
        self.getattr_one(&mut a)?;
        Ok(a)
    }

    /// `tc_exists()`.
    pub fn exists(&mut self, path: &str) -> bool {
        self.lstat(path).is_ok()
    }

    /// Return the file type of `path` (a `NF4*` value), for recursion tests.
    pub fn file_type(&mut self, path: &str) -> TcResult<u32> {
        let a = self.stat(path)?;
        Ok(a.ftype)
    }

    // -- directory listing --------------------------------------------------

    /// List a directory, `tc_listdir()`. Returns entry paths and attributes.
    pub fn listdir(
        &mut self,
        dir: &str,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
    ) -> TcResult<Vec<TcAttrs>> {
        let mut out = Vec::new();
        self.listdir_rec(dir, masks, max_count, recursive, &mut out)?;
        Ok(out)
    }

    fn listdir_rec(
        &mut self,
        dir: &str,
        masks: AttrMask,
        max_count: usize,
        recursive: bool,
        out: &mut Vec<TcAttrs>,
    ) -> TcRes {
        let reached_limit = |out: &Vec<TcAttrs>| max_count != 0 && out.len() >= max_count;
        if reached_limit(out) {
            return Ok(());
        }
        let dirfh = self.resolve(dir)?;
        let mut cookie = 0u64;
        loop {
            let entries = self
                .nfs
                .readdir(&dirfh, cookie)
                .map_err(|e| TcError::from_rpc(0, e))?;
            if entries.is_empty() {
                break;
            }
            for e in &entries {
                if reached_limit(out) {
                    return Ok(());
                }
                let path = Self::join_path(dir.trim_matches('/'), &e.name);
                let mut a = TcAttrs {
                    file: TcFile::from_path(&format!("/{}", path)),
                    masks,
                    ..TcAttrs::default()
                };
                // Attributes come back inline from READDIR for ATTR_IDS.
                let vals = parse_attrs(&e.attrs).unwrap_or_default();
                a.ftype = vals.ftype.unwrap_or(0);
                if masks.has_mode {
                    a.mode = vals.mode.unwrap_or(0);
                }
                if masks.has_size {
                    a.size = vals.size.unwrap_or(0);
                }
                if masks.has_nlink {
                    a.nlink = vals.nlink.unwrap_or(0);
                }
                if masks.has_fileid {
                    a.fileid = vals.fileid.unwrap_or(0);
                }
                let is_dir = a.ftype == nfs_ftype4_NF4DIR;
                out.push(a);
                if recursive && is_dir {
                    self.listdir_rec(&path, masks, max_count, recursive, out)?;
                }
            }
            cookie = entries.last().unwrap().cookie;
            if cookie == 0 {
                break;
            }
        }
        Ok(())
    }

    // -- namespace operations -----------------------------------------------

    /// Rename a list of file pairs, `tc_renamev()`. All RENAMEs are
    /// coalesced into as few compounds as possible.
    pub fn renamev(&mut self, pairs: &[(TcFile, TcFile)], _is_transaction: bool) -> TcRes {
        let mut ops = Vec::with_capacity(pairs.len());
        for (i, (src, dst)) in pairs.iter().enumerate() {
            let sp = src
                .path
                .as_ref()
                .ok_or_else(|| TcError::failure(i, nfsstat4_NFS4ERR_INVAL))?;
            let dp = dst
                .path
                .as_ref()
                .ok_or_else(|| TcError::failure(i, nfsstat4_NFS4ERR_INVAL))?;
            let s = sp.to_string_lossy().to_string();
            let d = dp.to_string_lossy().to_string();
            let (sdir, sname) = Self::split_path(&s).map_err(|e| TcError::failure(i, e.err_no))?;
            let (ddir, dname) = Self::split_path(&d).map_err(|e| TcError::failure(i, e.err_no))?;
            let sdirfh = self
                .nfs
                .resolve(sdir)
                .map_err(|e| TcError::from_rpc(i, e))?;
            let ddirfh = self
                .nfs
                .resolve(ddir)
                .map_err(|e| TcError::from_rpc(i, e))?;
            ops.push(crate::client::RenameOp {
                srcdir: sdirfh,
                oldname: sname.to_string(),
                dstdir: ddirfh,
                newname: dname.to_string(),
            });
        }
        self.nfs
            .rename_many(&ops)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    /// Remove a list of files, `tc_removev()`. Files sharing a parent
    /// directory are removed in a single `[PUTFH dir, REMOVE, REMOVE, ...]`
    /// compound.
    pub fn removev(&mut self, files: &[TcFile], _is_transaction: bool) -> TcRes {
        use std::collections::BTreeMap;
        // Group by parent directory to batch REMOVEs.
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            let path = f
                .path
                .as_ref()
                .ok_or_else(|| TcError::failure(i, nfsstat4_NFS4ERR_INVAL))?
                .to_string_lossy()
                .to_string();
            let (dir, name) = Self::split_path(&path).map_err(|e| TcError::failure(i, e.err_no))?;
            groups
                .entry(dir.to_string())
                .or_default()
                .push(name.to_string());
        }
        for (dir, names) in &groups {
            let dirfh = self.nfs.resolve(dir).map_err(|e| TcError::from_rpc(0, e))?;
            let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            self.nfs
                .remove_many(&dirfh, &refs)
                .map_err(|e| TcError::from_rpc(0, e))?;
        }
        Ok(())
    }

    /// `tc_unlink()`.
    pub fn unlink(&mut self, pathname: &str) -> TcResult<()> {
        self.removev(&[TcFile::from_path(pathname)], false)
    }

    /// `tc_unlinkv()`.
    pub fn unlinkv(&mut self, pathnames: &[&str]) -> TcRes {
        let files: Vec<TcFile> = pathnames.iter().map(|p| TcFile::from_path(p)).collect();
        self.removev(&files, false)
    }

    /// Create one or more directories, `tc_mkdirv()`. Each element's file is
    /// the directory path; mode from the masks is applied after creation.
    /// All CREATEs are coalesced into as few compounds as possible.
    pub fn mkdirv(&mut self, dirs: &[TcAttrs], _is_transaction: bool) -> TcRes {
        let mut creates = Vec::with_capacity(dirs.len());
        for (i, a) in dirs.iter().enumerate() {
            let path = a
                .file
                .path
                .as_ref()
                .ok_or_else(|| TcError::failure(i, nfsstat4_NFS4ERR_INVAL))?
                .to_string_lossy()
                .to_string();
            let full = if path.starts_with('/') {
                path.trim_start_matches('/').to_string()
            } else {
                self.cwd.join(&path).to_string_lossy().to_string()
            };
            let (dir, name) = Self::split_path(&full).map_err(|e| TcError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| TcError::from_rpc(i, e))?;
            creates.push(crate::client::CreateOp {
                dir: dirfh,
                name: name.to_string(),
                ftype: nfs_ftype4_NF4DIR,
                linkdata: None,
            });
        }
        self.nfs
            .create_many(&creates)
            .map_err(|e| TcError::from_rpc(0, e))?;

        // Apply modes (the handles come from re-resolving the new dirs).
        let mut setattrs = Vec::new();
        for (i, a) in dirs.iter().enumerate() {
            if a.masks.has_mode {
                let path = a.file.path.as_ref().unwrap().to_string_lossy().to_string();
                let fh = self
                    .resolve(&path)
                    .map_err(|e| TcError::failure(i, e.err_no))?;
                setattrs.push(crate::client::SetattrOp {
                    fh,
                    mode: Some(a.mode),
                    size: None,
                });
            }
        }
        if !setattrs.is_empty() {
            self.nfs
                .setattr_many(&setattrs)
                .map_err(|e| TcError::from_rpc(0, e))?;
        }
        Ok(())
    }

    /// Create a directory, convenience wrapper.
    pub fn mkdir(&mut self, path: &str, mode: u32) -> TcResult<()> {
        let a = TcAttrs {
            file: TcFile::from_path(path),
            masks: AttrMask {
                has_mode: true,
                ..AttrMask::default()
            },
            mode,
            ..TcAttrs::default()
        };
        self.mkdirv(std::slice::from_ref(&a), false)
    }

    /// Create a list of symlinks, `tc_symlinkv()`. All CREATEs are
    /// coalesced into as few compounds as possible.
    pub fn symlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str], _istxn: bool) -> TcRes {
        if oldpaths.len() != newpaths.len() {
            return Err(TcError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            let full = if new.starts_with('/') {
                new.trim_start_matches('/').to_string()
            } else {
                self.cwd.join(new).to_string_lossy().to_string()
            };
            let (dir, name) = Self::split_path(&full).map_err(|e| TcError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| TcError::from_rpc(i, e))?;
            ops.push(crate::client::CreateOp {
                dir: dirfh,
                name: name.to_string(),
                ftype: nfs_ftype4_NF4LNK,
                linkdata: Some(old.as_bytes().to_vec()),
            });
        }
        self.nfs
            .create_many(&ops)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    /// Create a symlink, `tc_symlink()`.
    pub fn symlink(&mut self, oldpath: &str, newpath: &str) -> TcResult<()> {
        self.symlinkv(
            std::slice::from_ref(&oldpath),
            std::slice::from_ref(&newpath),
            false,
        )
    }

    /// Read symlink targets, `tc_readlinkv()`. All READLINKs are coalesced
    /// into as few compounds as possible.
    pub fn readlinkv(&mut self, paths: &[&str], _istxn: bool) -> TcResult<Vec<Vec<u8>>> {
        let mut ops = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let fh = self.resolve(p).map_err(|e| TcError::failure(i, e.err_no))?;
            ops.push(crate::client::ReadlinkOp { fh });
        }
        self.nfs
            .readlink_many(&ops)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    /// Read a symlink target, `tc_readlink()`.
    pub fn readlink(&mut self, path: &str) -> TcResult<Vec<u8>> {
        let mut v = self.readlinkv(std::slice::from_ref(&path), false)?;
        Ok(v.remove(0))
    }

    /// Create hard links, `tc_hardlinkv()`. All LINKs are coalesced into as
    /// few compounds as possible.
    pub fn hardlinkv(&mut self, oldpaths: &[&str], newpaths: &[&str], _istxn: bool) -> TcRes {
        if oldpaths.len() != newpaths.len() {
            return Err(TcError::failure(0, nfsstat4_NFS4ERR_INVAL));
        }
        let mut ops = Vec::with_capacity(oldpaths.len());
        for (i, (old, new)) in oldpaths.iter().zip(newpaths).enumerate() {
            let src = self
                .resolve(old)
                .map_err(|e| TcError::failure(i, e.err_no))?;
            let full = if new.starts_with('/') {
                new.trim_start_matches('/').to_string()
            } else {
                self.cwd.join(new).to_string_lossy().to_string()
            };
            let (dir, name) = Self::split_path(&full).map_err(|e| TcError::failure(i, e.err_no))?;
            let dirfh = self.nfs.resolve(dir).map_err(|e| TcError::from_rpc(i, e))?;
            ops.push(crate::client::LinkOp {
                dstdir: dirfh,
                src,
                newname: name.to_string(),
            });
        }
        self.nfs
            .link_many(&ops)
            .map_err(|e| TcError::from_rpc(0, e))
    }

    /// Create a directory and all its ancestors, `tc_ensure_dir()`.
    pub fn ensure_dir(&mut self, dir: &str, mode: u32) -> TcResult<()> {
        let rel = if dir.starts_with('/') {
            dir.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(dir).to_string_lossy().to_string()
        };
        // Walk components, creating missing ones along the way.
        let mut so_far = String::new();
        for comp in rel.split('/').filter(|c| !c.is_empty()) {
            so_far = Self::join_path(&so_far, comp);
            if !self.exists(&format!("/{}", so_far)) {
                self.mkdir(&format!("/{}", so_far), mode)?;
            }
        }
        Ok(())
    }

    /// Remove a list of objects, recursively when `recursive`, `tc_rm()`.
    pub fn rm(&mut self, objs: &[&str], recursive: bool) -> TcRes {
        for (i, o) in objs.iter().enumerate() {
            self.rm_one(o, recursive).map_err(|mut e| {
                e.index = i;
                e
            })?;
        }
        Ok(())
    }

    fn rm_one(&mut self, path: &str, recursive: bool) -> TcResult<()> {
        let ft = self.file_type(path).unwrap_or(0);
        if ft == nfs_ftype4_NF4DIR && recursive {
            let entries = self.listdir(path, AttrMask::default(), usize::MAX, false)?;
            for e in entries {
                let p = e.file.path.unwrap().to_string_lossy().to_string();
                self.rm_one(&p, true)?;
            }
        }
        self.unlink(path)
    }

    // -- extent copy (tc_dupv / tc_lcopyv / tc_copyv / tc_lcopyv) -----------

    /// Copy one extent by reading `src` and writing `dst`, `tc_dupv()` /
    /// `tc_lcopyv()`.
    pub fn dupv(&mut self, pairs: &[ExtentPair], _is_transaction: bool) -> TcRes {
        for (i, p) in pairs.iter().enumerate() {
            self.copy_extent(p).map_err(|mut e| {
                e.index = i;
                e
            })?;
        }
        Ok(())
    }

    /// `tc_ldupv()`.
    pub fn ldupv(&mut self, pairs: &[ExtentPair], txn: bool) -> TcRes {
        self.dupv(pairs, txn)
    }

    /// `tc_copyv()` / `tc_lcopyv()`: server-side copy. The local nfsd speaks
    /// NFSv4.1 (no COPY operation), so this falls back to the read/write copy
    /// implemented by [`dupv`](Self::dupv).
    pub fn copyv(&mut self, pairs: &[ExtentPair], txn: bool) -> TcRes {
        self.dupv(pairs, txn)
    }

    /// `tc_lcopyv()`.
    pub fn lcopyv(&mut self, pairs: &[ExtentPair], txn: bool) -> TcRes {
        self.dupv(pairs, txn)
    }

    fn copy_extent(&mut self, p: &ExtentPair) -> TcResult<()> {
        let sfull = if p.src_path.starts_with('/') {
            p.src_path.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(&p.src_path).to_string_lossy().to_string()
        };
        let dfull = if p.dst_path.starts_with('/') {
            p.dst_path.trim_start_matches('/').to_string()
        } else {
            self.cwd.join(&p.dst_path).to_string_lossy().to_string()
        };
        let (sdir, sname) = Self::split_path(&sfull)?;
        let (ddir, dname) = Self::split_path(&dfull)?;
        let (sfh, ssid) = self.open_impl(sdir, sname, OPEN4_SHARE_ACCESS_READ, false, false)?;
        let (dfh, dsid) = self.open_impl(ddir, dname, OPEN4_SHARE_ACCESS_WRITE, true, false)?;

        let mut so = p.src_offset;
        let mut doff = p.dst_offset;
        let mut copied: u64 = 0;
        let result = loop {
            if p.length != u64::MAX && copied >= p.length {
                break Ok(());
            }
            let remaining = if p.length == u64::MAX {
                u64::MAX
            } else {
                p.length - copied
            };
            let chunk_len = remaining.min(1 << 20) as u32;
            let chunk = match self.nfs.read(&sfh, &ssid, so, chunk_len) {
                Ok(c) => c,
                Err(e) => break Err(TcError::from_rpc(0, e)),
            };
            if chunk.is_empty() {
                break Ok(()); // EOF
            }
            let n = match self.nfs.write(&dfh, &dsid, doff, &chunk) {
                Ok((n, _)) => n as u64,
                Err(e) => break Err(TcError::from_rpc(0, e)),
            };
            so += n;
            doff += n;
            copied += n;
        };
        let _ = self.nfs.close(&sfh, &ssid);
        let _ = self.nfs.close(&dfh, &dsid);
        result
    }

    // -- application data blocks (tc_write_adb) -----------------------------

    /// Write Application Data Blocks, `tc_write_adb()`. Each ADB block at
    /// `adb_offset + n * adb_block_size` gets the ADBN written at
    /// `adb_reloff_blocknum` and/or the pattern written at
    /// `adb_reloff_pattern`.
    pub fn write_adb(&mut self, patterns: &mut [Adb], _is_transaction: bool) -> TcRes {
        for (i, p) in patterns.iter_mut().enumerate() {
            let full = if p.path.starts_with('/') {
                p.path.trim_start_matches('/').to_string()
            } else {
                self.cwd.join(&p.path).to_string_lossy().to_string()
            };
            let (dir, name) = match Self::split_path(&full) {
                Ok(x) => x,
                Err(e) => return Err(TcError::failure(i, e.err_no)),
            };
            let (fh, sid) = match self.open_impl(dir, name, OPEN4_SHARE_ACCESS_WRITE, true, false) {
                Ok(x) => x,
                Err(e) => return Err(TcError::failure(i, e.err_no)),
            };
            let mut written = 0usize;
            let mut failed: Option<TcError> = None;
            for b in 0..p.adb_block_count {
                let base = p.adb_offset.saturating_add(b as u64 * p.adb_block_size);
                if p.adb_reloff_blocknum != u64::MAX {
                    let adbn = (p.adb_block_num + b as u64).to_be_bytes();
                    if let Err(e) = self
                        .nfs
                        .write(&fh, &sid, base + p.adb_reloff_blocknum, &adbn)
                    {
                        failed = Some(TcError::from_rpc(i, e));
                        break;
                    }
                }
                if p.adb_reloff_pattern != u64::MAX && !p.adb_pattern_data.is_empty() {
                    if let Err(e) =
                        self.nfs
                            .write(&fh, &sid, base + p.adb_reloff_pattern, &p.adb_pattern_data)
                    {
                        failed = Some(TcError::from_rpc(i, e));
                        break;
                    }
                }
                written += 1;
            }
            p.adb_block_count = written;
            let _ = self.nfs.close(&fh, &sid);
            if let Some(e) = failed {
                return Err(e);
            }
        }
        Ok(())
    }

    // -- callback listdir (tc_listdirv) -------------------------------------

    /// List directories with a callback, `tc_listdirv()`. `cb` is invoked for
    /// each entry with the entry and its parent directory; returning `false`
    /// stops the listing early.
    pub fn listdirv<F>(
        &mut self,
        dirs: &[&str],
        masks: AttrMask,
        max_entries: usize,
        recursive: bool,
        cb: &mut F,
    ) -> TcRes
    where
        F: FnMut(&TcAttrs, &str) -> bool,
    {
        for (i, d) in dirs.iter().enumerate() {
            let entries = self
                .listdir(d, masks, max_entries, recursive)
                .map_err(|mut e| {
                    e.index = i;
                    e
                })?;
            for e in &entries {
                if !cb(e, d) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    // -- recursive copy (tc_cp_recursive) -----------------------------------

    /// Recursively copy a directory tree, `tc_cp_recursive()`. When
    /// `symlinks` is true, symlinks are recreated; otherwise their targets are
    /// copied as regular files. Server-side copy is not available on NFSv4.1,
    /// so `use_server_side_copy` is ignored.
    pub fn cp_recursive(
        &mut self,
        src_dir: &str,
        dst: &str,
        symlinks: bool,
        _use_server_side_copy: bool,
    ) -> TcRes {
        if !self.exists(dst) {
            self.ensure_dir(dst, 0o755).map_err(|mut e| {
                e.index = 0;
                e
            })?;
        }
        let masks = AttrMask {
            has_mode: true,
            has_size: true,
            has_fileid: true,
            ..AttrMask::default()
        };
        let entries = self.listdir(src_dir, masks, 0, false)?;
        for e in entries {
            let name = e
                .file
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .ok_or_else(|| TcError::failure(0, nfsstat4_NFS4ERR_INVAL))?;
            let src_child = format!("{}/{}", src_dir.trim_end_matches('/'), name);
            let dst_child = format!("{}/{}", dst.trim_end_matches('/'), name);
            if e.ftype == nfs_ftype4_NF4DIR {
                self.cp_recursive(&src_child, &dst_child, symlinks, false)?;
            } else if e.ftype == nfs_ftype4_NF4LNK && symlinks {
                let target = self.readlink(&src_child).map_err(|mut e| {
                    e.index = 0;
                    e
                })?;
                self.symlink(&String::from_utf8_lossy(&target), &dst_child)
                    .map_err(|mut e| {
                        e.index = 0;
                        e
                    })?;
            } else {
                let pair = ExtentPair::new(&src_child, 0, &dst_child, 0, u64::MAX);
                self.copy_extent(&pair).map_err(|mut e| {
                    e.index = 0;
                    e
                })?;
            }
        }
        Ok(())
    }
}

impl Drop for TxnClient {
    /// `tc_deinit()`: close every open file so the client has no state left,
    /// allowing the session teardown to destroy the clientid on the server.
    fn drop(&mut self) {
        let fds: Vec<i32> = self.open_files.keys().copied().collect();
        for fd in fds {
            let open = self.open_files.remove(&fd);
            if let Some(o) = open {
                let _ = self.nfs.close(&o.fh, &o.stateid);
            }
        }
    }
}

/// `tc_rm_recursive()`.
pub fn rm_recursive(client: &mut TxnClient, dir: &str) -> TcRes {
    client.rm(&[dir], true)
}

fn read_u32(buf: &[u8], off: &mut usize) -> TcResult<u32> {
    if *off + 4 > buf.len() {
        return Err(TcError::failure(0, TC_ERR_RPC));
    }
    let v = u32::from_be_bytes(buf[*off..*off + 4].try_into().unwrap());
    *off += 4;
    Ok(v)
}

fn read_u64(buf: &[u8], off: &mut usize) -> TcResult<u64> {
    if *off + 8 > buf.len() {
        return Err(TcError::failure(0, TC_ERR_RPC));
    }
    let v = u64::from_be_bytes(buf[*off..*off + 8].try_into().unwrap());
    *off += 8;
    Ok(v)
}

/// `tc_okay()` helper: true when a TcRes succeeded.
pub fn okay(res: &TcRes) -> bool {
    res.is_ok()
}

/// Convenience: `tc_file_from_path()`.
pub fn file_from_path(path: &str) -> TcFile {
    TcFile::from_path(path)
}

// Keep `Path` referenced for API stability (getcwd / chdir paths).
#[allow(unused)]
fn _path_ref(_p: &Path) {}
