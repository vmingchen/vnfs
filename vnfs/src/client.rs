//! High-level NFSv4.1 operations on top of the session.

// bindgen emits lowercase constants (e.g. nfs_opnum4_NFS4_OP_WRITE) matched
// here in patterns; silence the style lint for those.
#![allow(non_upper_case_globals)]

use std::os::raw::c_char;
use std::time::Duration;

use nfsv41_sys::*;

use crate::compound::{Compound, CompoundRes};
use crate::error::{RpcError, RpcResult};
use crate::path::{components_bytes, split_path_bytes};
use crate::session::Session;

/// An NFS file handle owned by the client.
#[derive(Clone, Debug)]
pub struct FileHandle {
    bytes: Vec<u8>,
}

/// Which open owner a client operation uses: the user-visible descriptor
/// owner, or the internal path-op owner whose stateids never collide with
/// caller-held descriptors.
#[derive(Clone, Copy)]
enum OwnerSlot {
    User,
    Path,
}

impl FileHandle {
    fn as_nfs_fh(&self) -> nfs_fh4 {
        nfs_fh4 {
            nfs_fh4_len: self.bytes.len() as u32,
            nfs_fh4_val: self.bytes.as_ptr() as *mut c_char,
        }
    }

    fn from_nfs_fh(fh: &nfs_fh4) -> FileHandle {
        let slice = unsafe {
            std::slice::from_raw_parts(fh.nfs_fh4_val as *const u8, fh.nfs_fh4_len as usize)
        };
        FileHandle {
            bytes: slice.to_vec(),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

pub struct NfsClient {
    session: Session,
    root: FileHandle,
    /// Maximum estimated encoded bytes per merged compound (0 = unlimited).
    /// Mirrors txn-compound's 1 MiB `CPD_LIMIT` default.
    pub max_compound_bytes: usize,
    /// Maximum reply bytes per compound (bounds total READ data, which
    /// travels in the reply). Separate from `max_compound_bytes` because
    /// servers commonly grant much larger requests than replies.
    pub max_response_bytes: usize,
    /// Server-confirmed maximum operations per compound (merged builders).
    pub max_ops: usize,
    server_max_request_bytes: usize,
    configured_max_request_bytes: Option<usize>,
}

/// Upper bound for the per-compound payload cap for merged path I/O; the
/// actual default comes from the server-confirmed `ca_maxrequestsize`.
pub const DEFAULT_MAX_COMPOUND_BYTES: usize = 4 << 20;
/// Upper bound for a single READ/WRITE op's payload, matching the XDR codec
/// cap `XDR_BYTES_MAXLEN_IO` (64 MiB) in nfsv41-sys. Servers with smaller
/// per-op or per-compound limits still get their payloads split by the
/// compound/op budgets.
pub const MAX_OP_BYTES: usize = 64 << 20;

/// How an OPEN handles a file that does not exist yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenCreate {
    /// Never create the file; fail with NFS4ERR_NOENT if absent.
    NoCreate,
    /// Create with EXCLUSIVE4 semantics: fail with NFS4ERR_EXIST if present.
    Exclusive,
    /// Create with GUARDED4 semantics: create if absent, succeed if present.
    Guarded,
    /// Create with UNCHECKED4 semantics: create if absent, open if present.
    /// Lets a compound skip the existence probe entirely.
    Unchecked,
}

/// The NFSv4.1 special stateid (seqid 1, zero "other") that Ganesha resolves
/// to "the current stateid of the current filehandle" for READ/WRITE/CLOSE,
/// mirroring the txn-compound client's `CURSID`. This is what lets OPEN +
/// WRITE + CLOSE all live in one compound.
const SPECIAL_STATEID: stateid4 = stateid4 {
    seqid: 1,
    other: [0; 12],
};

/// An entry returned by READDIR.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub cookie: u64,
    /// Raw XDR-encoded attribute list, in the order requested.
    pub attrs: Vec<u8>,
}

/// A directory's listing (entries so far and the cookie to continue).
pub struct ChildListing {
    pub fh: FileHandle,
    pub entries: Vec<DirEntry>,
    pub cookie: u64,
}

/// One READ of a batched compound, `[PUTFH, READ]`.
pub struct ReadOp {
    pub fh: FileHandle,
    pub stateid: stateid4,
    pub offset: u64,
    pub count: u32,
}

/// One WRITE of a batched compound, `[PUTFH, WRITE]`.
pub struct WriteOp {
    pub fh: FileHandle,
    pub stateid: stateid4,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// One GETATTR of a batched compound, `[PUTFH, GETATTR]`.
pub struct GetattrOp {
    pub fh: FileHandle,
    pub attrs: Vec<u32>,
}

/// One SETATTR of a batched compound, `[PUTFH, SETATTR]`.
pub struct SetattrOp {
    pub fh: FileHandle,
    pub mode: Option<u32>,
    pub size: Option<u64>,
}

/// One READLINK of a batched compound, `[PUTFH, READLINK]`.
pub struct ReadlinkOp {
    pub fh: FileHandle,
}

/// One RENAME of a batched compound: `[PUTFH src, SAVEFH, PUTFH dst, RENAME]`.
pub struct RenameOp {
    pub srcdir: FileHandle,
    pub oldname: Vec<u8>,
    pub dstdir: FileHandle,
    pub newname: Vec<u8>,
}

/// One CREATE of a batched compound, `[PUTFH dir, CREATE]` (mkdir / symlink).
pub struct CreateOp {
    pub dir: FileHandle,
    pub name: Vec<u8>,
    pub ftype: nfs_ftype4,
    pub linkdata: Option<Vec<u8>>,
}

/// One LINK of a batched compound: `[PUTFH src, SAVEFH, PUTFH dst, LINK]`.
pub struct LinkOp {
    pub dstdir: FileHandle,
    pub src: FileHandle,
    pub newname: Vec<u8>,
}

/// One OPEN of a batched compound, `[PUTFH dir, OPEN, GETFH]`.
pub struct OpenOp {
    pub dir: FileHandle,
    pub name: Vec<u8>,
    pub access: u32,
    pub create: OpenCreate,
}

/// One CLOSE of a batched compound, `[PUTFH fh, CLOSE]`.
pub struct CloseOp {
    pub fh: FileHandle,
    pub stateid: stateid4,
}

/// One NFSv4.2 COPY: `[PUTFH src, SAVEFH, PUTFH dst, COPY]`.
pub struct CopyOp {
    pub src_fh: FileHandle,
    pub src_stateid: stateid4,
    pub dst_fh: FileHandle,
    pub dst_stateid: stateid4,
    pub src_offset: u64,
    pub dst_offset: u64,
    /// Zero means copy from `src_offset` through EOF.
    pub count: u64,
}

/// The server confirmed `ca_maxoperations` from CREATE_SESSION; keep every
/// compound (plus the implicit SEQUENCE) under it.
const MAX_COMPOUND_OPS: usize = 256;

/// One source of truth for the negotiated operation budget. NFS counts the
/// mandatory SEQUENCE operation in `ca_maxoperations`, while Compound builders
/// only hold the operations that follow it.
#[derive(Clone, Copy, Debug)]
struct CompoundBudget {
    max_ops: usize,
}

impl CompoundBudget {
    fn new(max_ops: usize) -> Self {
        Self { max_ops }
    }

    fn batch_capacity(self, ops_per_item: usize) -> RpcResult<usize> {
        let capacity = self.max_ops.saturating_sub(1) / ops_per_item;
        if capacity == 0 {
            Err(RpcError::op(0, nfsstat4_NFS4ERR_TOO_MANY_OPS))
        } else {
            Ok(capacity)
        }
    }

    /// Limit used by variable-shape merged builders. `reserve` is soft
    /// headroom for path walking; when the server advertises a small limit,
    /// retain enough room for one minimum-shape item without ever exceeding
    /// the negotiated maximum.
    fn merged_limit(self, reserve: usize, min_ops_per_item: usize) -> usize {
        let minimum = 1usize.saturating_add(min_ops_per_item);
        if self.max_ops < minimum {
            0
        } else {
            self.max_ops
                .saturating_sub(reserve)
                .max(minimum)
                .min(self.max_ops)
        }
    }

    fn ensure(self, compound: &Compound) -> RpcResult<()> {
        if compound.op_count().saturating_add(1) > self.max_ops {
            Err(RpcError::op(0, nfsstat4_NFS4ERR_TOO_MANY_OPS))
        } else {
            Ok(())
        }
    }
}

fn effective_request_limit(server: usize, configured: Option<usize>) -> usize {
    configured.map_or(server, |limit| limit.min(server))
}

fn response_payload_budget(negotiated: usize) -> usize {
    negotiated.saturating_mul(3).saturating_div(4).max(1)
}

fn checked_offset(base: u64, delta: usize, op_index: usize) -> RpcResult<u64> {
    let delta = u64::try_from(delta).map_err(|_| RpcError::op(op_index, nfsstat4_NFS4ERR_INVAL))?;
    base.checked_add(delta)
        .ok_or_else(|| RpcError::op(op_index, nfsstat4_NFS4ERR_INVAL))
}

/// FATTR4 attribute ids requested for every READDIR entry, in wire order.
/// Keep in sync with the parse order in `nfs.rs::parse_attrs`. Note:
/// FATTR4_TIME_CREATE is intentionally absent (ganesha omits it, and it maps
/// to creation time, not stat's ctime). FATTR4_NAMED_ATTR is the per-object
/// "has a non-empty named attribute directory" boolean (RFC 5661 s5.8.1.8).
pub const READDIR_ATTRS: [u32; 14] = [
    FATTR4_TYPE,
    FATTR4_CHANGE,
    FATTR4_SIZE,
    FATTR4_NAMED_ATTR,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
    FATTR4_OWNER,
    FATTR4_OWNER_GROUP,
    FATTR4_RAWDEV,
    FATTR4_SPACE_USED,
    FATTR4_TIME_ACCESS,
    FATTR4_TIME_METADATA,
    FATTR4_TIME_MODIFY,
];

// ---------------------------------------------------------------------------
// Merged (single-compound) path I/O
// ---------------------------------------------------------------------------

/// A file reference for a merged compound: a path resolved in-compound, or a
/// pre-resolved open-file handle (descriptor ops, mixed into the same
/// compound with PUTFH).
pub enum FileRef {
    Path(Vec<u8>),
    Handle(FileHandle),
}

/// One path-based WRITE for a merged compound. The path is root-relative
/// (no leading slash); the offset is already resolved to an absolute value.
pub struct PathWriteOp {
    pub file: FileRef,
    pub offset: u64,
    pub data: Vec<u8>,
    pub create: bool,
    /// Truncate the file to zero before writing (emitted as an in-compound
    /// SETATTR size=0 right after the OPEN).
    pub truncate: bool,
    /// Real stateid for descriptor ops (None for path ops, which open and
    /// use the special stateid in-compound).
    pub stateid: Option<stateid4>,
}

/// One path-based READ for a merged compound.
pub struct PathReadOp {
    pub file: FileRef,
    pub offset: u64,
    pub count: usize,
    pub stateid: Option<stateid4>,
}

/// Per-op results of a merged path compound. `counts`/`committed` are `None`
/// for ops that never executed (the compound aborted at `failed`).
pub struct PathWriteOutcome {
    pub counts: Vec<Option<u32>>,
    pub committed: Vec<Option<u32>>,
    /// Open stateids (with their filehandles) created by the compound; used
    /// by the separate-close form and for best-effort cleanup on failure.
    pub opened: Vec<(FileHandle, stateid4)>,
    /// First failing caller-relative op index + NFS status, if any.
    pub failed: Option<(usize, u32)>,
    /// The trailing in-compound CLOSE failed (special stateid unsupported).
    pub close_failed: Option<u32>,
}

/// Per-op results of a merged path read compound.
pub struct PathReadOutcome {
    pub data: Vec<Option<Vec<u8>>>,
    pub eof: Vec<Option<bool>>,
    pub opened: Vec<(FileHandle, stateid4)>,
    pub failed: Option<(usize, u32)>,
    pub close_failed: Option<u32>,
}

/// One path-based GETATTR for a merged compound.
pub struct PathGetattrOp {
    pub file: FileRef,
    pub attrs: Vec<u32>,
}

pub struct PathGetattrOutcome {
    /// Raw XDR attribute lists per op (in the requested order).
    pub lists: Vec<Option<Vec<u8>>>,
    pub failed: Option<(usize, u32)>,
}

/// One path-based SETATTR for a merged compound.
pub struct PathSetattrOp {
    pub file: FileRef,
    pub mode: Option<u32>,
    pub size: Option<u64>,
    /// Request the object's own type (needed for symlink handling).
    pub check_type: bool,
}

pub struct PathSetattrOutcome {
    /// The object's own NFS type per op (when `check_type`), else None.
    pub types: Vec<Option<u32>>,
    pub failed: Option<(usize, u32)>,
}

/// One path-based OPEN for a merged compound.
pub struct PathOpenOp {
    pub path: Vec<u8>,
    pub access: u32,
    pub create: OpenCreate,
    /// Mode to apply on creation (UNCHECKED createattrs / post-open for
    /// EXCLUSIVE creates).
    pub mode: Option<u32>,
    pub truncate: bool,
}

pub struct PathOpenOutcome {
    /// (filehandle, stateid) per op; None for ops that did not complete.
    pub opened: Vec<Option<(FileHandle, stateid4)>>,
    pub failed: Option<(usize, u32)>,
}

pub struct PathRemoveOutcome {
    pub removed: Vec<Option<()>>,
    pub failed: Option<(usize, u32)>,
}

/// One path-based RENAME pair for a merged compound.
pub struct PathRenamePair {
    pub src: Vec<u8>,
    pub dst: Vec<u8>,
}

pub struct PathRenameOutcome {
    pub renamed: Vec<Option<()>>,
    pub failed: Option<(usize, u32)>,
}

/// Compound-local "current filehandle" tracking, mirroring the txn-compound
/// client: the parent directory of the current batch is resolved once
/// (PUTROOTFH + LOOKUPs) and SAVEFH'd; child operations climb back to it with
/// RESTOREFH instead of re-resolving from the root.
#[derive(Default)]
struct CfhCursor {
    /// Root-relative path of the directory currently in the saved-fh slot.
    saved_dir: Option<Vec<u8>>,
    /// Whether the compound's current fh currently equals the saved fh.
    at_saved: bool,
}

impl CfhCursor {
    /// Make the current fh the parent of `path` and leave it in the saved-fh
    /// slot. When a directory is already saved, the walk is relative to it
    /// (LOOKUPP for "..", LOOKUP for shared-prefix descendants) whenever that
    /// is cheaper than re-resolving from PUTROOTFH, so shared prefixes are
    /// never re-walked. Returns the leaf component and the number of ops
    /// appended, or None if the path is malformed.
    fn set_parent(&mut self, c: &mut Compound, path: &[u8]) -> Option<(Vec<u8>, usize)> {
        let (dir, leaf) = split_path_bytes(path).ok()?;
        if self.saved_dir.as_deref() == Some(&dir) {
            let mut ops = 0;
            if !self.at_saved {
                // We are below the saved parent; climb back with RESTOREFH.
                c.restorefh();
                ops += 1;
                self.at_saved = true;
            }
            return Some((leaf, ops));
        }
        let mut ops = 0;
        if let Some(saved) = self.saved_dir.clone() {
            if !self.at_saved {
                c.restorefh();
                ops += 1;
                self.at_saved = true;
            }
            let saved_comps = components_bytes(&saved);
            let target_comps = components_bytes(&dir);
            let common = common_prefix_len(&saved_comps, &target_comps);
            let ups = saved_comps.len() - common;
            let downs = target_comps.len() - common;
            if ups + downs < 1 + target_comps.len() {
                for _ in 0..ups {
                    c.lookupp();
                    ops += 1;
                }
                for comp in &target_comps[common..] {
                    c.lookup(comp);
                    ops += 1;
                }
                c.savefh();
                ops += 1;
                self.saved_dir = Some(dir.clone());
                self.at_saved = true;
                return Some((leaf, ops));
            }
        }
        // Resolve the parent directory from the export root.
        let mut ops = 1; // PUTROOTFH
        c.putrootfh();
        for comp in components_bytes(&dir) {
            c.lookup(&comp);
            ops += 1;
        }
        c.savefh();
        ops += 1;
        self.saved_dir = Some(dir);
        self.at_saved = true;
        Some((leaf, ops))
    }

    /// Make the current fh the parent of `path` WITHOUT saving it (used by
    /// RENAME, which needs the saved-fh slot to keep the source directory).
    fn set_current_parent(&mut self, c: &mut Compound, path: &[u8]) -> Option<(Vec<u8>, usize)> {
        let (dir, leaf) = split_path_bytes(path).ok()?;
        if self.saved_dir.as_deref() == Some(&dir) {
            let mut ops = 0;
            if !self.at_saved {
                c.restorefh();
                ops += 1;
                self.at_saved = true;
            }
            return Some((leaf, ops));
        }
        let mut ops = 0;
        if let Some(saved) = self.saved_dir.clone() {
            if !self.at_saved {
                c.restorefh();
                ops += 1;
                self.at_saved = true;
            }
            let saved_comps = components_bytes(&saved);
            let target_comps = components_bytes(&dir);
            let common = common_prefix_len(&saved_comps, &target_comps);
            let ups = saved_comps.len() - common;
            let downs = target_comps.len() - common;
            if ups + downs < 1 + target_comps.len() {
                for _ in 0..ups {
                    c.lookupp();
                    ops += 1;
                }
                for comp in &target_comps[common..] {
                    c.lookup(comp);
                    ops += 1;
                }
                self.at_saved = false;
                return Some((leaf, ops));
            }
        }
        let mut ops = 1; // PUTROOTFH
        c.putrootfh();
        for comp in components_bytes(&dir) {
            c.lookup(&comp);
            ops += 1;
        }
        self.at_saved = false; // current fh differs from the saved one
        Some((leaf, ops))
    }

    /// Make the current fh a known open-file handle (descriptor ops). The
    /// saved fh is untouched, so a later path op can still RESTOREFH back.
    fn set_handle(&mut self, c: &mut Compound, fh: &FileHandle) {
        c.putfh(&fh.as_nfs_fh());
        self.at_saved = false;
    }

    /// Note that an operation (OPEN/LOOKUP/...) moved the current fh away
    /// from the saved fh.
    fn descend(&mut self) {
        self.at_saved = false;
    }
}

fn common_prefix_len<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Lengths of the per-op chunks covering `[start, end)` when each op carries
/// at most `per` bytes.
fn chunk_lens(start: usize, end: usize, per: usize) -> Vec<usize> {
    (start..end)
        .step_by(per)
        .map(|off| (end - off).min(per))
        .collect()
}

/// Maps compound op positions (resarray indices; 0 = SEQUENCE) to the
/// caller-relative op whose range contains them, so a mid-compound failure
/// can be attributed to the right caller index.
#[derive(Default)]
struct OpMap {
    /// (caller_index, first_op, end_op_exclusive) per caller op.
    ranges: Vec<(usize, usize, usize)>,
    next: usize,
}

impl OpMap {
    fn new() -> OpMap {
        // resarray[0] is the implicit SEQUENCE.
        OpMap {
            ranges: Vec::new(),
            next: 1,
        }
    }

    fn begin(&mut self, caller: usize) {
        self.ranges.push((caller, self.next, self.next));
    }

    fn end(&mut self) {
        if let Some(last) = self.ranges.last_mut() {
            last.2 = self.next;
        }
    }

    fn note_ops(&mut self, n: usize) {
        self.next += n;
    }
}

/// The first op range that is incomplete or contains a failing op, as
/// `(caller_index, nfs_status)`.
fn first_failed_range(res: &CompoundRes, ranges: &[(usize, usize, usize)]) -> Option<(usize, u32)> {
    for (caller, s, e) in ranges {
        let mut bad: Option<u32> = None;
        let upto = (*e).min(res.nops());
        for j in *s..upto {
            let st = res.op_status(j);
            if st != nfsstat4_NFS4_OK {
                bad = Some(st);
                break;
            }
        }
        if bad.is_none() && *e > res.nops() {
            bad = Some(res.status());
        }
        if let Some(st) = bad {
            return Some((*caller, st));
        }
    }
    None
}

impl NfsClient {
    /// Connect, run the session handshake, and resolve the export root.
    pub fn connect(host: &str) -> RpcResult<NfsClient> {
        match Self::connect_minor(host, 2) {
            Ok(client) => Ok(client),
            Err(error) if error.status == nfsstat4_NFS4ERR_MINOR_VERS_MISMATCH => {
                Self::connect_minor(host, 1)
            }
            Err(error) => Err(error),
        }
    }

    pub fn connect_minor(host: &str, minorversion: u32) -> RpcResult<NfsClient> {
        Self::connect_minor_with_timeouts(
            host,
            minorversion,
            Duration::from_secs(10),
            Duration::from_secs(5),
        )
    }

    pub fn connect_with_timeouts(
        host: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> RpcResult<NfsClient> {
        match Self::connect_minor_with_timeouts(host, 2, connect_timeout, request_timeout) {
            Ok(client) => Ok(client),
            Err(error) if error.status == nfsstat4_NFS4ERR_MINOR_VERS_MISMATCH => {
                Self::connect_minor_with_timeouts(host, 1, connect_timeout, request_timeout)
            }
            Err(error) => Err(error),
        }
    }

    pub fn connect_minor_with_timeouts(
        host: &str,
        minorversion: u32,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> RpcResult<NfsClient> {
        let mut session = Session::connect_minor_with_timeouts(
            host,
            minorversion,
            connect_timeout,
            request_timeout,
        )?;
        let root = session_mount_root(&mut session)?;
        let configured_max_request_bytes = Some(DEFAULT_MAX_COMPOUND_BYTES);
        let max_compound_bytes =
            effective_request_limit(session.max_requestsize, configured_max_request_bytes);
        let max_response_bytes = session.max_responsesize.min(DEFAULT_MAX_COMPOUND_BYTES);
        let max_ops = session.max_operations.min(MAX_COMPOUND_OPS);
        let server_max_request_bytes = session.max_requestsize;
        Ok(NfsClient {
            session,
            root,
            max_compound_bytes,
            max_response_bytes,
            max_ops,
            server_max_request_bytes,
            configured_max_request_bytes,
        })
    }

    /// Minor version negotiated for this session.
    pub fn minorversion(&self) -> u32 {
        self.session.minorversion
    }

    /// Set the configured per-compound payload cap. Zero restores the
    /// negotiated server maximum; callers can never exceed that maximum.
    pub fn set_max_compound_bytes(&mut self, bytes: usize) {
        self.configured_max_request_bytes = (bytes != 0).then_some(bytes);
        self.max_compound_bytes = effective_request_limit(
            self.server_max_request_bytes,
            self.configured_max_request_bytes,
        );
    }

    /// Per-op data cap: no single READ/WRITE op may carry more than the
    /// server's per-op limit (bounded by the compound cap as well).
    pub fn per_op_bytes(&self) -> usize {
        // Leave room for the compound header, SEQUENCE, filehandle, and op
        // metadata. The final op-count guard cannot protect a byte-size
        // limit, so a single payload must not fill the entire request.
        self.max_compound_bytes
            .min(self.server_max_request_bytes.saturating_sub(1024))
            .clamp(1, MAX_OP_BYTES)
    }

    /// Total READ data budget per compound. Servers validate the summed READ
    /// counts (plus resarray overhead) against `ca_maxresponsesize` and
    /// commonly reject a compound that fills it exactly, so leave a quarter
    /// of the reply budget as headroom.
    pub fn read_compound_bytes(&self) -> usize {
        response_payload_budget(self.max_response_bytes)
    }

    /// Per-op data cap for READs: a single READ's data travels in the reply,
    /// so it must never exceed the reply budget even alone in a compound.
    pub fn read_per_op_bytes(&self) -> usize {
        self.per_op_bytes().min(self.read_compound_bytes())
    }

    fn readdir_limits(&self, operations: usize) -> (u32, u32) {
        let per_operation = (self.read_compound_bytes() / operations.max(1)).max(1);
        let maxcount = per_operation.min(u32::MAX as usize) as u32;
        let dircount = (maxcount / 4).max(1);
        (dircount, maxcount.max(1))
    }

    pub fn root(&self) -> &FileHandle {
        &self.root
    }

    fn op_budget(&self) -> CompoundBudget {
        CompoundBudget::new(self.max_ops)
    }

    /// Send a compound only when its final size, including SEQUENCE, honors
    /// the server-confirmed `ca_maxoperations` value.
    fn call_compound(&mut self, compound: &mut Compound) -> RpcResult<CompoundRes> {
        self.op_budget().ensure(compound)?;
        self.session.compound(compound)
    }

    /// Look up a single component below `dir`.
    pub fn lookup(&mut self, dir: &FileHandle, name: &[u8]) -> RpcResult<FileHandle> {
        let mut c = Compound::new();
        c.tag(b"lookup");
        c.putfh(&dir.as_nfs_fh());
        c.lookup(name);
        c.getfh();
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(FileHandle::from_nfs_fh(res.getfh(3)))
    }

    /// Look up `name` below `dir`, returning the child handle and its
    /// FATTR4_TYPE in one compound (`[PUTFH, LOOKUP, GETFH, GETATTR]`).
    /// A symlink is returned as-is (type `NF4LNK`), so callers can follow it.
    pub fn lookup_getattr(
        &mut self,
        dir: &FileHandle,
        name: &[u8],
    ) -> RpcResult<(FileHandle, u32)> {
        let mut c = Compound::new();
        c.tag(b"lookup_getattr");
        c.putfh(&dir.as_nfs_fh());
        c.lookup(name);
        c.getfh();
        c.getattr(&[FATTR4_TYPE]);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let fh = FileHandle::from_nfs_fh(res.getfh(3));
        let t = res.getattr_bytes(4);
        let ftype = if t.len() >= 4 {
            u32::from_be_bytes(t[0..4].try_into().unwrap())
        } else {
            0
        };
        Ok((fh, ftype))
    }

    /// Tolerantly LOOKUP each `(parent, name)` and return the child handle and
    /// FATTR4_TYPE (`[PUTFH, LOOKUP, GETFH, GETATTR]` per element). Failed
    /// LOOKUPs are reported per path; only transport / compound-level
    /// failures abort the whole call.
    pub fn lookup_getattr_many(
        &mut self,
        ops: &[(FileHandle, Vec<u8>)],
    ) -> RpcResult<Vec<Result<(FileHandle, u32), u32>>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let per_chunk = self.op_budget().batch_capacity(4)?;
        let mut out = Vec::with_capacity(ops.len());
        for chunk in ops.chunks(per_chunk) {
            let mut c = Compound::new();
            c.tag(b"lookup_typev");
            for (dir, name) in chunk {
                c.putfh(&dir.as_nfs_fh());
                c.lookup(name);
                c.getfh();
                c.getattr(&[FATTR4_TYPE]);
            }
            let res = self.call_compound(&mut c)?;
            for (i, _) in chunk.iter().enumerate() {
                let st_idx = 2 + 4 * i;
                if st_idx >= res.nops() {
                    out.push(Err(res.status()));
                    continue;
                }
                if res.op_status(st_idx) == nfsstat4_NFS4_OK
                    && 3 + 4 * i < res.nops()
                    && 4 + 4 * i < res.nops()
                {
                    let fh = FileHandle::from_nfs_fh(res.getfh(3 + 4 * i));
                    let t = res.getattr_bytes(4 + 4 * i);
                    let ftype = if t.len() >= 4 {
                        u32::from_be_bytes(t[0..4].try_into().unwrap())
                    } else {
                        0
                    };
                    out.push(Ok((fh, ftype)));
                } else {
                    out.push(Err(res.op_status(st_idx)));
                }
            }
        }
        Ok(out)
    }

    /// Tolerantly LOOKUP each `(parent, name)` in as few compounds as
    /// possible (`[PUTFH, LOOKUP, GETFH]` per element), returning the per-path
    /// result. A failed LOOKUP (e.g. `NFS4ERR_NOENT`) is reported per path;
    /// only transport / compound-level failures abort the whole call. The
    /// returned error index, when one is produced, is the element's position
    /// in `ops`.
    pub fn lookup_many(
        &mut self,
        ops: &[(FileHandle, Vec<u8>)],
    ) -> RpcResult<Vec<Result<FileHandle, u32>>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let per_chunk = self.op_budget().batch_capacity(3)?;
        let mut out = Vec::with_capacity(ops.len());
        for chunk in ops.chunks(per_chunk) {
            let mut c = Compound::new();
            c.tag(b"lookupv");
            for (dir, name) in chunk {
                c.putfh(&dir.as_nfs_fh());
                c.lookup(name);
                c.getfh();
            }
            let res = self.call_compound(&mut c)?;
            for (i, _) in chunk.iter().enumerate() {
                let st_idx = 2 + 3 * i;
                if st_idx >= res.nops() {
                    // The server aborted the compound at an earlier failing
                    // op and omitted the remaining resops; report the
                    // compound status for everything from here on.
                    out.push(Err(res.status()));
                    continue;
                }
                if res.op_status(st_idx) == nfsstat4_NFS4_OK && 3 + 3 * i < res.nops() {
                    out.push(Ok(FileHandle::from_nfs_fh(res.getfh(3 + 3 * i))));
                } else {
                    out.push(Err(res.op_status(st_idx)));
                }
            }
        }
        Ok(out)
    }

    /// Resolve a slash-separated path from the export root in a single
    /// compound: `[PUTFH root, LOOKUP a, LOOKUP b, ..., GETFH]`. After each
    /// LOOKUP the current filehandle is the looked-up object, so consecutive
    /// LOOKUPs chain without intermediate round trips.
    pub fn resolve(&mut self, path: &[u8]) -> RpcResult<FileHandle> {
        let mut c = Compound::new();
        c.tag(b"resolve");
        c.putfh(&self.root.as_nfs_fh());
        let mut ncomps = 0usize;
        for comp in crate::path::components_bytes(path) {
            c.lookup(&comp);
            ncomps += 1;
        }
        if ncomps == 0 {
            return Ok(self.root.clone());
        }
        c.getfh();
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(FileHandle::from_nfs_fh(res.getfh(2 + ncomps)))
    }

    /// WRITE several `[PUTFH, WRITE]` pairs in as few compounds as possible.
    /// Returns `(bytes written, committed)` per op.
    ///
    /// Note: there is no batched READ counterpart. The kernel nfsd does not
    /// handle multiple READ ops per compound correctly (its reply-page offset
    /// computation collides sub-page reads), so reads are issued one per
    /// compound.
    /// READ several `[PUTFH, READ]` pairs in as few compounds as possible.
    /// Each i-th read in a compound is at resop `2 + 2*i` (SEQUENCE, PUTFH,
    /// READ, PUTFH, READ, ...). The kernel nfsd does not serve multiple READ
    /// ops per compound correctly; use an nfs-ganesha server for this.
    /// Returns the data and the server's EOF flag per request, in request
    /// order.
    pub fn readv(&mut self, ops: &[ReadOp]) -> RpcResult<Vec<(Vec<u8>, bool)>> {
        self.batch_ops(
            b"readv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.read(&op.stateid, op.offset, op.count);
            },
            |res, i| {
                let ok = res.read(2 + 2 * i);
                let len = ok.data.data_len as usize;
                let data = if len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(ok.data.data_val as *const u8, len) }
                        .to_vec()
                };
                (data, ok.eof != 0)
            },
        )
    }

    /// WRITE several `[PUTFH, WRITE]` pairs in as few compounds as possible;
    /// returns (bytes written, commit mode) per request.
    pub fn writev(&mut self, ops: &[WriteOp]) -> RpcResult<Vec<(u32, u32)>> {
        self.batch_ops(
            b"writev",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.write(&op.stateid, op.offset, stable_how4_FILE_SYNC4, &op.data);
            },
            |res, i| {
                let ok = res.write(2 + 2 * i);
                (ok.count, ok.committed)
            },
        )
    }

    /// REMOVE several names from `dir` in one compound. REMOVE leaves the
    /// current filehandle on `dir`, so consecutive REMOVEs chain.
    pub fn remove_many(&mut self, dir: &FileHandle, names: &[Vec<u8>]) -> RpcResult<()> {
        let map = |op_index: usize| {
            op_index
                .saturating_sub(1)
                .min(names.len().saturating_sub(1))
        };
        let mut c = Compound::new();
        c.tag(b"removev");
        c.putfh(&dir.as_nfs_fh());
        for n in names {
            c.remove(n);
        }
        let res = self.call_compound(&mut c).map_err(|e| {
            let idx = map(e.op_index);
            e.with_op_index(idx)
        })?;
        self.session.expect_all_ok(&res).map_err(|e| {
            let idx = map(e.op_index);
            e.with_op_index(idx)
        })?;
        Ok(())
    }

    /// OPEN a file below `dir`. `create` controls creation semantics. Returns
    /// the file handle and the open stateid.
    pub fn open(
        &mut self,
        dir: &FileHandle,
        name: &[u8],
        access: u32,
        create: OpenCreate,
    ) -> RpcResult<(FileHandle, stateid4)> {
        self.open_slot(dir, name, access, create, OwnerSlot::User)
    }

    /// Like [`open`](Self::open) but uses the path-op open owner, whose
    /// stateids never collide with caller-held descriptors.
    pub fn open_path(
        &mut self,
        dir: &FileHandle,
        name: &[u8],
        access: u32,
        create: OpenCreate,
    ) -> RpcResult<(FileHandle, stateid4)> {
        self.open_slot(dir, name, access, create, OwnerSlot::Path)
    }

    fn open_slot(
        &mut self,
        dir: &FileHandle,
        name: &[u8],
        access: u32,
        create: OpenCreate,
        slot: OwnerSlot,
    ) -> RpcResult<(FileHandle, stateid4)> {
        let (seqid, verifier, owner_name) = match slot {
            OwnerSlot::User => (
                self.session.open_owner.seqid,
                self.session.open_owner.verifier,
                self.session.open_owner.name.clone(),
            ),
            OwnerSlot::Path => (
                self.session.path_owner.seqid,
                self.session.path_owner.verifier,
                self.session.path_owner.name.clone(),
            ),
        };
        let mut c = Compound::new();
        c.tag(b"open");
        c.putfh(&dir.as_nfs_fh());
        let openhow = make_open_how(create, verifier);
        c.open_claim_null(
            seqid,
            access,
            OPEN4_SHARE_DENY_NONE,
            self.session.clientid,
            &owner_name,
            openhow,
            name,
        );
        c.getfh();
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let stateid = res.open(2).stateid;
        let fh = res.getfh(3);
        match slot {
            OwnerSlot::User => self.session.open_owner.seqid += 1,
            OwnerSlot::Path => self.session.path_owner.seqid += 1,
        }
        Ok((FileHandle::from_nfs_fh(fh), stateid))
    }

    /// Run a batch of same-shaped operations across as few compounds as
    /// possible. `add` appends `per_op` ops for each element to the current
    /// compound (with the element's global index, for seqid-based ops);
    /// `extract` reads the result of the i-th element from a compound reply.
    fn batch_ops<T, R>(
        &mut self,
        tag: &[u8],
        per_op: usize,
        ops: &[T],
        mut add: impl FnMut(&mut Compound, &T, usize),
        extract: impl Fn(&CompoundRes, usize) -> R,
    ) -> RpcResult<Vec<R>> {
        /// Translate a compound-internal resop index (0 = SEQUENCE) to the
        /// caller's request index: element `i` occupies resops
        /// `1 + per_op*i .. 1 + per_op*(i+1)`.
        fn caller_index(
            op_index: usize,
            per_op: usize,
            chunk_start: usize,
            chunk_len: usize,
        ) -> usize {
            let local = op_index.saturating_sub(1) / per_op;
            chunk_start + local.min(chunk_len.saturating_sub(1))
        }
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let chunk_size = self.op_budget().batch_capacity(per_op)?;
        let mut out = Vec::with_capacity(ops.len());
        let mut global = 0usize;
        for chunk in ops.chunks(chunk_size) {
            let chunk_start = global;
            let mut c = Compound::new();
            c.tag(tag);
            for op in chunk {
                add(&mut c, op, global);
                global += 1;
            }
            let res = self.call_compound(&mut c).map_err(|e| {
                let idx = caller_index(e.op_index, per_op, chunk_start, chunk.len());
                e.with_op_index(idx)
            })?;
            // Attribute a failing op to the right caller index. When the
            // server reports the failed op in the reply we find it by
            // scanning; when it aborts mid-compound and only sets the
            // compound-level status, the failing op is the first one whose
            // result is missing.
            let bad = (0..res.nops()).find(|&i| res.op_status(i) != nfsstat4_NFS4_OK);
            match bad {
                Some(i) => {
                    let idx = caller_index(i, per_op, chunk_start, chunk.len());
                    return Err(RpcError::op(idx, res.op_status(i)));
                }
                None if res.status() != nfsstat4_NFS4_OK => {
                    let present = res.nops().saturating_sub(1);
                    let local = present / per_op;
                    let idx = chunk_start + local.min(chunk.len() - 1);
                    return Err(RpcError::op(idx, res.status()));
                }
                None => {}
            }
            for (i, _) in chunk.iter().enumerate() {
                out.push(extract(&res, i));
            }
        }
        Ok(out)
    }

    /// GETATTR several files in as few compounds as possible; returns the raw
    /// attribute list per file, in request order.
    pub fn getattr_many(&mut self, ops: &[GetattrOp]) -> RpcResult<Vec<Vec<u8>>> {
        self.batch_ops(
            b"getattrv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.getattr(&op.attrs);
            },
            |res, i| res.getattr_bytes(2 + 2 * i),
        )
    }

    /// SETATTR mode and/or size on several files in one compound.
    pub fn setattr_many(&mut self, ops: &[SetattrOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<SetattrOp, ()>(
            b"setattrv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.setattr(op.mode, op.size);
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// READLINK several files in as few compounds as possible.
    pub fn readlink_many(&mut self, ops: &[ReadlinkOp]) -> RpcResult<Vec<Vec<u8>>> {
        self.batch_ops(
            b"readlinkv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.readlink();
            },
            |res, i| res.readlink(2 + 2 * i).to_vec(),
        )
    }

    /// RENAME several pairs in as few compounds as possible. Each pair is
    /// `[PUTFH src, SAVEFH, PUTFH dst, RENAME]`.
    pub fn rename_many(&mut self, ops: &[RenameOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<RenameOp, ()>(
            b"renamev",
            4,
            ops,
            |c, op, _| {
                c.putfh(&op.srcdir.as_nfs_fh());
                c.savefh();
                c.putfh(&op.dstdir.as_nfs_fh());
                c.rename(&op.oldname, &op.newname);
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// CREATE several objects (mkdir / symlink) in as few compounds as
    /// possible. CREATE changes the current filehandle, so each gets its own
    /// `[PUTFH dir, CREATE]`.
    pub fn create_many(&mut self, ops: &[CreateOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<CreateOp, ()>(
            b"createv",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.dir.as_nfs_fh());
                c.create(&op.name, op.ftype, op.linkdata.as_deref());
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// LINK several sources into their destinations in as few compounds as
    /// possible. Each is `[PUTFH src, SAVEFH, PUTFH dst, LINK]`.
    pub fn link_many(&mut self, ops: &[LinkOp]) -> RpcResult<()> {
        let _ = self.batch_ops::<LinkOp, ()>(
            b"linkv",
            4,
            ops,
            |c, op, _| {
                c.putfh(&op.src.as_nfs_fh());
                c.savefh();
                c.putfh(&op.dstdir.as_nfs_fh());
                c.link(&op.newname);
            },
            |_, _| (),
        )?;
        Ok(())
    }

    /// OPEN several files in as few compounds as possible; each is
    /// `[PUTFH dir, OPEN, GETFH]`. Open-owner seqids are assigned
    /// consecutively across the batch.
    pub fn open_many(&mut self, ops: &[OpenOp]) -> RpcResult<Vec<(FileHandle, stateid4)>> {
        self.open_many_slot(ops, OwnerSlot::User)
    }

    /// Like [`open_many`](Self::open_many) but using the path-op open owner.
    pub fn open_many_path(&mut self, ops: &[OpenOp]) -> RpcResult<Vec<(FileHandle, stateid4)>> {
        self.open_many_slot(ops, OwnerSlot::Path)
    }

    fn open_many_slot(
        &mut self,
        ops: &[OpenOp],
        slot: OwnerSlot,
    ) -> RpcResult<Vec<(FileHandle, stateid4)>> {
        let (base, verifier, owner_name) = match slot {
            OwnerSlot::User => (
                self.session.open_owner.seqid,
                self.session.open_owner.verifier,
                self.session.open_owner.name.clone(),
            ),
            OwnerSlot::Path => (
                self.session.path_owner.seqid,
                self.session.path_owner.verifier,
                self.session.path_owner.name.clone(),
            ),
        };
        let clientid = self.session.clientid;
        let n = ops.len();
        let out = self.batch_ops(
            b"openv",
            3,
            ops,
            |c, op, gi| {
                c.putfh(&op.dir.as_nfs_fh());
                let openhow = make_open_how(op.create, verifier);
                c.open_claim_null(
                    base + gi as u32,
                    op.access,
                    OPEN4_SHARE_DENY_NONE,
                    clientid,
                    &owner_name,
                    openhow,
                    &op.name,
                );
                c.getfh();
            },
            |res, i| {
                let stateid = res.open(2 + 3 * i).stateid;
                let fh = res.getfh(3 + 3 * i);
                (FileHandle::from_nfs_fh(fh), stateid)
            },
        )?;
        match slot {
            OwnerSlot::User => self.session.open_owner.seqid = base + n as u32,
            OwnerSlot::Path => self.session.path_owner.seqid = base + n as u32,
        }
        Ok(out)
    }

    /// CLOSE several files in as few compounds as possible. Close seqids are
    /// assigned consecutively across the batch.
    pub fn close_many(&mut self, ops: &[CloseOp]) -> RpcResult<()> {
        self.close_many_slot(ops, OwnerSlot::User)
    }

    /// Like [`close_many`](Self::close_many) but using the path-op open owner.
    pub fn close_many_path(&mut self, ops: &[CloseOp]) -> RpcResult<()> {
        self.close_many_slot(ops, OwnerSlot::Path)
    }

    /// Batched path-based WRITEs in one compound per chunk:
    ///
    /// `[SEQUENCE, PUTROOTFH, LOOKUP <parent>, SAVEFH, OPEN, WRITE,
    /// RESTOREFH, OPEN, WRITE, ..., CLOSE]`
    ///
    /// The parent directory is resolved once and SAVEFH'd; each file is
    /// OPENed (UNCHECKED create, so no existence probe), WRITten with the
    /// special stateid, and the compound climbs back with RESTOREFH. When
    /// `close_in_compound` is set the final CLOSE uses the special stateid
    /// (Ganesha resolves it to the current open); otherwise the open
    /// stateids/filehandles are returned so the caller can CLOSE in a
    /// follow-up compound (the portable fallback).
    ///
    /// On a mid-compound failure, ops before the failing op are reported in
    /// `counts`/`committed` and `failed` carries the caller-relative index
    /// and NFS status.
    pub fn writev_path_compound(
        &mut self,
        ops: &[PathWriteOp],
        close_in_compound: bool,
    ) -> RpcResult<PathWriteOutcome> {
        let n = ops.len();
        let mut counts: Vec<Option<u32>> = vec![None; n];
        let mut committed: Vec<Option<u32>> = vec![None; n];
        let mut opened: Vec<(FileHandle, stateid4)> = Vec::new();
        let mut failed: Option<(usize, u32)> = None;
        let mut close_failed: Option<u32> = None;
        // Worst case per file: RESTOREFH + CLOSE + OPEN (+GETFH) + WRITE.
        let per_file = 4;
        // Headroom for a new directory's path resolution inside a compound.
        let reserve = 8;
        let budget = self.op_budget().merged_limit(reserve, per_file);
        let per_op = self.per_op_bytes();

        let mut global = 0usize;
        // Bytes of ops[global] already emitted across earlier compounds
        // (>0 means ops[global] is being continued mid-file).
        let mut part_off = 0usize;
        while global < n {
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = OpMap::new();
            let mut c = Compound::new();
            c.tag(if close_in_compound {
                b"writev1"
            } else {
                b"writev2"
            });
            let mut opened_path: Option<Vec<u8>> = None;
            let mut fh_at_opened = false;
            let mut opens_in_chunk = 0usize;
            let base_seq = self.session.path_owner.seqid;
            let mut payload = 0usize;

            while global < n && map.next + per_file <= budget {
                let op = &ops[global];
                let start = part_off;
                let remaining = op.data.len() - start;
                // A single WRITE op is capped by the server's per-op limit
                // (and the generated XDR's 1 MiB opaque bound), so large
                // payloads become consecutive WRITE ops. Only as many chunks
                // as fit this compound's byte and op budgets are emitted; the
                // rest resume in the next compound.
                let chunks_total = remaining.div_ceil(per_op).max(1);
                let room = if self.max_compound_bytes > 0 {
                    if payload > 0 {
                        self.max_compound_bytes.saturating_sub(payload + 128)
                    } else {
                        self.max_compound_bytes
                    }
                } else {
                    usize::MAX
                };
                // Cap by the byte budget using the next chunk's actual size
                // (not the 1 MiB op cap): small per-file windows must pack
                // densely into the compound.
                let per_chunk = per_op.min(remaining).max(1);
                let mut take = chunks_total.min(budget.saturating_sub(map.next + 3));
                let by_bytes = if room >= per_chunk {
                    (room / per_chunk).max(1)
                } else {
                    0
                };
                take = take.min(by_bytes);
                if take == 0 {
                    if map.next == 0 && payload == 0 {
                        // Never stall on the first file: a single chunk is at
                        // most per_op bytes, which fits by construction.
                        take = 1;
                    } else {
                        break;
                    }
                }
                let end = (start + take * per_op).min(op.data.len());
                if end == start && remaining > 0 {
                    break;
                }
                map.begin(global);
                let mut newly_opened = false;
                match &op.file {
                    FileRef::Path(p) => {
                        if opened_path.as_deref() == Some(p.as_slice()) && fh_at_opened {
                            // Same file: the current fh is still the opened file.
                        } else {
                            if close_in_compound && opened_path.is_some() && fh_at_opened {
                                // Close the previous file while its fh is current.
                                c.close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                                map.note_ops(1);
                                opened_path = None;
                            }
                            let (leaf, nops) = match cursor.set_parent(&mut c, p) {
                                Some(x) => x,
                                None => {
                                    failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                                    global = n;
                                    break;
                                }
                            };
                            map.note_ops(nops);
                            if op.create && op.truncate {
                                c.open_claim_null_create_mode(
                                    base_seq + opens_in_chunk as u32,
                                    OPEN4_SHARE_ACCESS_BOTH,
                                    OPEN4_SHARE_DENY_NONE,
                                    self.session.clientid,
                                    &self.session.path_owner.name,
                                    &leaf,
                                    None,
                                    true,
                                );
                            } else {
                                let create = if op.create {
                                    OpenCreate::Unchecked
                                } else {
                                    OpenCreate::NoCreate
                                };
                                c.open_claim_null(
                                    base_seq + opens_in_chunk as u32,
                                    OPEN4_SHARE_ACCESS_BOTH,
                                    OPEN4_SHARE_DENY_NONE,
                                    self.session.clientid,
                                    &self.session.path_owner.name,
                                    make_open_how(create, self.session.path_owner.verifier),
                                    &leaf,
                                );
                            }
                            opens_in_chunk += 1;
                            map.note_ops(1);
                            if !close_in_compound {
                                c.getfh();
                                map.note_ops(1);
                            }
                            opened_path = Some(p.clone());
                            fh_at_opened = true;
                            newly_opened = true;
                            if op.truncate && !op.create {
                                // Truncate in-compound right after OPEN so a
                                // no-create open still has O_TRUNC semantics.
                                // Creation opens carry size=0 in OPEN's
                                // createattrs instead.
                                c.setattr_with_stateid(None, Some(0), &SPECIAL_STATEID);
                                map.note_ops(1);
                            }
                        }
                        if end > start {
                            let mut off = 0usize;
                            for chunk in op.data[start..end].chunks(per_op) {
                                c.write(
                                    &SPECIAL_STATEID,
                                    checked_offset(op.offset, start + off, global)?,
                                    stable_how4_FILE_SYNC4,
                                    chunk,
                                );
                                map.note_ops(1);
                                off += chunk.len();
                            }
                        } else {
                            // Zero-length write: still open/create the file
                            // and emit an empty WRITE for the result.
                            c.write(
                                &SPECIAL_STATEID,
                                checked_offset(op.offset, start, global)?,
                                stable_how4_FILE_SYNC4,
                                &[],
                            );
                            map.note_ops(1);
                        }
                        if newly_opened {
                            cursor.descend();
                        }
                    }
                    FileRef::Handle(fh) => {
                        if close_in_compound && opened_path.is_some() && fh_at_opened {
                            c.close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                            map.note_ops(1);
                            opened_path = None;
                        }
                        cursor.set_handle(&mut c, fh);
                        map.note_ops(1);
                        let sid = op.stateid.as_ref().unwrap_or(&SPECIAL_STATEID);
                        if end > start {
                            let mut off = 0usize;
                            for chunk in op.data[start..end].chunks(per_op) {
                                c.write(
                                    sid,
                                    checked_offset(op.offset, start + off, global)?,
                                    stable_how4_FILE_SYNC4,
                                    chunk,
                                );
                                map.note_ops(1);
                                off += chunk.len();
                            }
                        } else {
                            c.write(
                                sid,
                                checked_offset(op.offset, start, global)?,
                                stable_how4_FILE_SYNC4,
                                &[],
                            );
                            map.note_ops(1);
                        }
                        fh_at_opened = false;
                    }
                }
                map.end();
                payload += 128 + (end - start);
                if end == op.data.len() {
                    global += 1;
                    part_off = 0;
                } else {
                    // The compound is full; resume this file next time.
                    part_off = end;
                    break;
                }
                // A new directory's resolution could exceed the reserve.
                if map.next + per_file > budget && global < n {
                    break;
                }
            }

            if map.ranges.is_empty() {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
            if close_in_compound && opened_path.is_some() {
                c.close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                map.note_ops(1);
            }
            self.op_budget().ensure(&c)?;
            self.session.path_owner.seqid = base_seq + opens_in_chunk as u32;

            let res = self.call_compound(&mut c)?;
            // Find the first incomplete/failed range.
            let mut range_failed = None;
            let mut done = 0usize;
            for (caller, s, e) in &map.ranges {
                let mut bad: Option<(usize, u32)> = None;
                let upto = (*e).min(res.nops());
                for j in *s..upto {
                    let st = res.op_status(j);
                    if st != nfsstat4_NFS4_OK {
                        bad = Some((j, st));
                        break;
                    }
                }
                if bad.is_none() && *e > res.nops() {
                    bad = Some((res.nops(), res.status()));
                }
                if let Some((_, st)) = bad {
                    range_failed = Some((*caller, st));
                    done = *caller;
                    break;
                }
                done = *caller + 1;
            }
            if let Some((caller, st)) = range_failed {
                failed = Some((caller, st));
                for i in caller + 1..n {
                    counts[i] = None;
                    committed[i] = None;
                }
            }
            // Extract results and opened stateids from the resarray.
            for (caller, s, e) in &map.ranges {
                if *caller >= done {
                    continue;
                }
                for j in *s..(*e).min(res.nops()) {
                    let ro = res.op(j);
                    unsafe {
                        match ro.resop {
                            nfs_opnum4_NFS4_OP_WRITE => {
                                let ok = ro.nfs_resop4_u.opwrite.WRITE4res_u.resok4;
                                let c = counts[*caller].get_or_insert(0);
                                *c = c.saturating_add(ok.count);
                                committed[*caller] = Some(ok.committed);
                            }
                            nfs_opnum4_NFS4_OP_OPEN if !close_in_compound => {
                                // Paired with the GETFH right after it.
                                let stateid = res.open(j).stateid;
                                let fh = res.getfh(j + 1);
                                opened.push((FileHandle::from_nfs_fh(fh), stateid));
                            }
                            _ => {}
                        }
                    }
                }
            }
            // The trailing CLOSE (close_in_compound form) sits outside any
            // file range; report a failure there so the caller can fall back
            // to the separate-close form.
            if close_in_compound && range_failed.is_none() && opened_path.is_some() {
                let last = map.ranges.last().map(|(_, _, e)| *e).unwrap_or(1);
                if last < res.nops() && res.op_status(last) != nfsstat4_NFS4_OK {
                    close_failed = Some(res.op_status(last));
                }
            }
            if failed.is_some() {
                break;
            }
            if chunk_start == global && part_off == 0 {
                // No progress (capacity check failed even for one op): bail.
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
        }
        Ok(PathWriteOutcome {
            counts,
            committed,
            opened,
            failed,
            close_failed,
        })
    }

    /// Batched path-based READs in one compound per chunk, same shape as
    /// [`writev_path_compound`](Self::writev_path_compound) but with READ and
    /// no creation.
    pub fn readv_path_compound(
        &mut self,
        ops: &[PathReadOp],
        close_in_compound: bool,
    ) -> RpcResult<PathReadOutcome> {
        let n = ops.len();
        let mut data: Vec<Option<Vec<u8>>> = vec![None; n];
        let mut eof: Vec<Option<bool>> = vec![None; n];
        let mut opened: Vec<(FileHandle, stateid4)> = Vec::new();
        let mut failed: Option<(usize, u32)> = None;
        let mut close_failed: Option<u32> = None;
        let per_file = 4;
        let reserve = 8;
        let budget = self.op_budget().merged_limit(reserve, per_file);
        let per_op = self.read_per_op_bytes();

        let mut global = 0usize;
        // Bytes of ops[global] already fetched across earlier compounds
        // (>0 means ops[global] is being continued mid-file).
        let mut part_off = 0usize;
        while global < n {
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = OpMap::new();
            let mut c = Compound::new();
            c.tag(if close_in_compound {
                b"readv1"
            } else {
                b"readv2"
            });
            let mut opened_path: Option<Vec<u8>> = None;
            let mut fh_at_opened = false;
            let mut opens_in_chunk = 0usize;
            let base_seq = self.session.path_owner.seqid;
            let mut payload = 0usize;

            while global < n && map.next + per_file <= budget {
                let op = &ops[global];
                let start = part_off;
                let remaining = op.count.saturating_sub(start);
                // A single READ op is capped by the server's per-op limit
                // (and the generated XDR's 1 MiB opaque reply bound), so
                // large reads become consecutive READ ops. Only as many
                // chunks as fit this compound's byte and op budgets are
                // emitted; the rest resume in the next compound.
                let chunks_total = remaining.div_ceil(per_op).max(1);
                let room = if self.max_response_bytes > 0 {
                    // Reserve a little overhead even for the first file: the
                    // server validates the summed READ counts (plus resarray
                    // overhead) against ca_maxresponsesize before serving.
                    if payload > 0 {
                        self.read_compound_bytes().saturating_sub(payload + 128)
                    } else {
                        // The first op is capped at read_per_op_bytes(), so a
                        // full-budget room never lets it overflow the reply.
                        self.read_compound_bytes()
                    }
                } else {
                    usize::MAX
                };
                // Cap by the byte budget using the next chunk's actual size
                // (not the 1 MiB op cap): small per-file windows must pack
                // densely into the compound.
                let per_chunk = per_op.min(remaining).max(1);
                let mut take = chunks_total.min(budget.saturating_sub(map.next + 3));
                let by_bytes = if room >= per_chunk {
                    (room / per_chunk).max(1)
                } else {
                    0
                };
                take = take.min(by_bytes);
                if take == 0 {
                    if map.next == 0 && payload == 0 {
                        take = 1;
                    } else {
                        break;
                    }
                }
                let end = (start + take * per_op).min(op.count);
                if end == start && remaining > 0 {
                    break;
                }
                map.begin(global);
                let mut newly_opened = false;
                match &op.file {
                    FileRef::Path(p) => {
                        if opened_path.as_deref() == Some(p.as_slice()) && fh_at_opened {
                            // Same file: the current fh is still the opened file.
                        } else {
                            if close_in_compound && opened_path.is_some() && fh_at_opened {
                                c.close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                                map.note_ops(1);
                                opened_path = None;
                            }
                            let (leaf, nops) = match cursor.set_parent(&mut c, p) {
                                Some(x) => x,
                                None => {
                                    failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                                    global = n;
                                    break;
                                }
                            };
                            map.note_ops(nops);
                            c.open_claim_null(
                                base_seq + opens_in_chunk as u32,
                                OPEN4_SHARE_ACCESS_READ,
                                OPEN4_SHARE_DENY_NONE,
                                self.session.clientid,
                                &self.session.path_owner.name,
                                make_open_how(
                                    OpenCreate::NoCreate,
                                    self.session.path_owner.verifier,
                                ),
                                &leaf,
                            );
                            opens_in_chunk += 1;
                            map.note_ops(1);
                            if !close_in_compound {
                                c.getfh();
                                map.note_ops(1);
                            }
                            opened_path = Some(p.clone());
                            fh_at_opened = true;
                            newly_opened = true;
                        }
                        if end > start {
                            let mut off = 0usize;
                            for chunk_len in chunk_lens(start, end, per_op) {
                                c.read(
                                    &SPECIAL_STATEID,
                                    checked_offset(op.offset, start + off, global)?,
                                    chunk_len as u32,
                                );
                                map.note_ops(1);
                                off += chunk_len;
                            }
                        } else {
                            // Zero-length read: still OPEN and emit an empty
                            // READ so the result array stays aligned.
                            c.read(
                                &SPECIAL_STATEID,
                                checked_offset(op.offset, start, global)?,
                                0,
                            );
                            map.note_ops(1);
                        }
                        if newly_opened {
                            cursor.descend();
                        }
                    }
                    FileRef::Handle(fh) => {
                        if close_in_compound && opened_path.is_some() && fh_at_opened {
                            c.close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                            map.note_ops(1);
                            opened_path = None;
                        }
                        cursor.set_handle(&mut c, fh);
                        map.note_ops(1);
                        let sid = op.stateid.as_ref().unwrap_or(&SPECIAL_STATEID);
                        if end > start {
                            let mut off = 0usize;
                            for chunk_len in chunk_lens(start, end, per_op) {
                                c.read(
                                    sid,
                                    checked_offset(op.offset, start + off, global)?,
                                    chunk_len as u32,
                                );
                                map.note_ops(1);
                                off += chunk_len;
                            }
                        } else {
                            c.read(sid, checked_offset(op.offset, start, global)?, 0);
                            map.note_ops(1);
                        }
                        fh_at_opened = false;
                    }
                }
                map.end();
                payload += 128 + (end - start);
                if end == op.count {
                    global += 1;
                    part_off = 0;
                } else {
                    part_off = end;
                    break;
                }
                if map.next + per_file > budget && global < n {
                    break;
                }
            }

            if map.ranges.is_empty() {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
            if close_in_compound && opened_path.is_some() {
                c.close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                map.note_ops(1);
            }
            self.op_budget().ensure(&c)?;
            self.session.path_owner.seqid = base_seq + opens_in_chunk as u32;

            let res = self.call_compound(&mut c)?;
            let mut range_failed = None;
            let mut done = 0usize;
            for (caller, s, e) in &map.ranges {
                let mut bad: Option<(usize, u32)> = None;
                let upto = (*e).min(res.nops());
                for j in *s..upto {
                    let st = res.op_status(j);
                    if st != nfsstat4_NFS4_OK {
                        bad = Some((j, st));
                        break;
                    }
                }
                if bad.is_none() && *e > res.nops() {
                    bad = Some((res.nops(), res.status()));
                }
                if let Some((_, st)) = bad {
                    range_failed = Some((*caller, st));
                    done = *caller;
                    break;
                }
                done = *caller + 1;
            }
            if let Some((caller, st)) = range_failed {
                failed = Some((caller, st));
                for i in caller + 1..n {
                    data[i] = None;
                    eof[i] = None;
                }
            }
            for (caller, s, e) in &map.ranges {
                if *caller >= done {
                    continue;
                }
                for j in *s..(*e).min(res.nops()) {
                    let ro = res.op(j);
                    unsafe {
                        match ro.resop {
                            nfs_opnum4_NFS4_OP_READ => {
                                let ok = ro.nfs_resop4_u.opread.READ4res_u.resok4;
                                let len = ok.data.data_len as usize;
                                let bytes = if len == 0 {
                                    Vec::new()
                                } else {
                                    std::slice::from_raw_parts(ok.data.data_val as *const u8, len)
                                        .to_vec()
                                };
                                data[*caller].get_or_insert_with(Vec::new).extend(bytes);
                                eof[*caller] = Some(ok.eof != 0);
                            }
                            nfs_opnum4_NFS4_OP_OPEN if !close_in_compound => {
                                let stateid = res.open(j).stateid;
                                let fh = res.getfh(j + 1);
                                opened.push((FileHandle::from_nfs_fh(fh), stateid));
                            }
                            _ => {}
                        }
                    }
                }
            }
            if close_in_compound && range_failed.is_none() && opened_path.is_some() {
                let last = map.ranges.last().map(|(_, _, e)| *e).unwrap_or(1);
                if last < res.nops() && res.op_status(last) != nfsstat4_NFS4_OK {
                    close_failed = Some(res.op_status(last));
                }
            }
            if failed.is_some() {
                break;
            }
            if chunk_start == global && part_off == 0 {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
        }
        Ok(PathReadOutcome {
            data,
            eof,
            opened,
            failed,
            close_failed,
        })
    }

    /// Batched path-based GETATTRs in one compound per chunk:
    /// `[SEQUENCE, PUTROOTFH, LOOKUP <parent>, SAVEFH, LOOKUP, GETATTR,
    /// RESTOREFH, LOOKUP, GETATTR, ...]`.
    pub fn getattr_path_compound(
        &mut self,
        ops: &[PathGetattrOp],
    ) -> RpcResult<PathGetattrOutcome> {
        let n = ops.len();
        let mut lists: Vec<Option<Vec<u8>>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 4; // RESTOREFH + LOOKUP + GETATTR + margin
        let reserve = 16;
        let budget = self.op_budget().merged_limit(reserve, per_file);
        let mut global = 0usize;
        while global < n {
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = OpMap::new();
            let mut c = Compound::new();
            c.tag(b"getattrv1");
            let mut payload = 0usize;
            while global < n && map.next + per_file <= budget {
                let op = &ops[global];
                let est = 256;
                if payload > 0
                    && self.max_compound_bytes > 0
                    && payload + est > self.max_compound_bytes
                {
                    break;
                }
                map.begin(global);
                match &op.file {
                    FileRef::Path(p) => {
                        let (leaf, nops) = match cursor.set_parent(&mut c, p) {
                            Some(x) => x,
                            None => {
                                failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                                global = n;
                                break;
                            }
                        };
                        map.note_ops(nops);
                        c.lookup(&leaf);
                        map.note_ops(1);
                    }
                    FileRef::Handle(fh) => {
                        cursor.set_handle(&mut c, fh);
                        map.note_ops(1);
                    }
                }
                c.getattr(&op.attrs);
                map.note_ops(1);
                map.end();
                cursor.descend();
                payload += est;
                global += 1;
                if map.next + per_file > budget && global < n {
                    break;
                }
            }
            if map.ranges.is_empty() {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
            let res = self.call_compound(&mut c)?;
            if let Some((caller, st)) = first_failed_range(&res, &map.ranges) {
                failed = Some((caller, st));
                // Keep the prefix results (the caller resumes from here).
                for (c, s, e) in &map.ranges {
                    if *c >= caller {
                        continue;
                    }
                    for j in *s..*e {
                        if res.op(j).resop == nfs_opnum4_NFS4_OP_GETATTR {
                            lists[*c] = Some(res.getattr_bytes(j));
                        }
                    }
                }
                break;
            }
            for (caller, s, e) in &map.ranges {
                for j in *s..*e {
                    if res.op(j).resop == nfs_opnum4_NFS4_OP_GETATTR {
                        lists[*caller] = Some(res.getattr_bytes(j));
                    }
                }
            }
            if chunk_start == global {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
        }
        Ok(PathGetattrOutcome { lists, failed })
    }

    /// Batched path-based SETATTRs in one compound per chunk. When
    /// `check_type` is set, each file's own type is fetched (for symlink
    /// handling by the caller).
    pub fn setattr_path_compound(
        &mut self,
        ops: &[PathSetattrOp],
    ) -> RpcResult<PathSetattrOutcome> {
        let n = ops.len();
        let mut types: Vec<Option<u32>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 5; // RESTOREFH + LOOKUP + [GETATTR] + SETATTR + margin
        let reserve = 16;
        let budget = self.op_budget().merged_limit(reserve, per_file);
        let mut global = 0usize;
        while global < n {
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = OpMap::new();
            let mut c = Compound::new();
            c.tag(b"setattrv1");
            let mut payload = 0usize;
            while global < n && map.next + per_file <= budget {
                let op = &ops[global];
                let est = 256;
                if payload > 0
                    && self.max_compound_bytes > 0
                    && payload + est > self.max_compound_bytes
                {
                    break;
                }
                map.begin(global);
                match &op.file {
                    FileRef::Path(p) => {
                        let (leaf, nops) = match cursor.set_parent(&mut c, p) {
                            Some(x) => x,
                            None => {
                                failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                                global = n;
                                break;
                            }
                        };
                        map.note_ops(nops);
                        c.lookup(&leaf);
                        map.note_ops(1);
                    }
                    FileRef::Handle(fh) => {
                        cursor.set_handle(&mut c, fh);
                        map.note_ops(1);
                    }
                }
                if op.check_type {
                    c.getattr(&[FATTR4_TYPE]);
                    map.note_ops(1);
                }
                c.setattr(op.mode, op.size);
                map.note_ops(1);
                map.end();
                cursor.descend();
                payload += est;
                global += 1;
                if map.next + per_file > budget && global < n {
                    break;
                }
            }
            if map.ranges.is_empty() {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
            let res = self.call_compound(&mut c)?;
            if let Some((caller, st)) = first_failed_range(&res, &map.ranges) {
                failed = Some((caller, st));
                // Keep the prefix types (the caller resumes from here).
                for (c, s, e) in &map.ranges {
                    if *c >= caller {
                        continue;
                    }
                    for j in *s..*e {
                        if res.op(j).resop == nfs_opnum4_NFS4_OP_GETATTR {
                            let b = res.getattr_bytes(j);
                            types[*c] = (b.len() >= 4)
                                .then(|| u32::from_be_bytes(b[0..4].try_into().unwrap()));
                        }
                    }
                }
                break;
            }
            for (caller, s, e) in &map.ranges {
                for j in *s..*e {
                    if res.op(j).resop == nfs_opnum4_NFS4_OP_GETATTR {
                        let b = res.getattr_bytes(j);
                        types[*caller] =
                            (b.len() >= 4).then(|| u32::from_be_bytes(b[0..4].try_into().unwrap()));
                    }
                }
            }
            if chunk_start == global {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
        }
        Ok(PathSetattrOutcome { types, failed })
    }

    /// Batched path-based OPENs in one compound per chunk, returning the
    /// opened (filehandle, stateid) pairs (the caller keeps them open).
    pub fn openv_path_compound(&mut self, ops: &[PathOpenOp]) -> RpcResult<PathOpenOutcome> {
        let n = ops.len();
        let mut opened: Vec<Option<(FileHandle, stateid4)>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 6; // RESTOREFH + OPEN + GETFH + [SETATTR x2] + margin
        let reserve = 16;
        let budget = self.op_budget().merged_limit(reserve, per_file);
        let mut global = 0usize;
        while global < n {
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = OpMap::new();
            let mut c = Compound::new();
            c.tag(b"openv1");
            let mut opens_in_chunk = 0usize;
            let base_seq = self.session.path_owner.seqid;
            let mut payload = 0usize;
            while global < n && map.next + per_file <= budget {
                let op = &ops[global];
                let est = 256;
                if payload > 0
                    && self.max_compound_bytes > 0
                    && payload + est > self.max_compound_bytes
                {
                    break;
                }
                map.begin(global);
                let (leaf, nops) = match cursor.set_parent(&mut c, &op.path) {
                    Some(x) => x,
                    None => {
                        failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                        global = n;
                        break;
                    }
                };
                map.note_ops(nops);
                match op.create {
                    OpenCreate::Unchecked => {
                        let mode = op.mode.unwrap_or(0o644);
                        c.open_claim_null_create_mode(
                            base_seq + opens_in_chunk as u32,
                            op.access,
                            OPEN4_SHARE_DENY_NONE,
                            self.session.clientid,
                            &self.session.path_owner.name,
                            &leaf,
                            Some(mode),
                            op.truncate,
                        );
                    }
                    create => c.open_claim_null(
                        base_seq + opens_in_chunk as u32,
                        op.access,
                        OPEN4_SHARE_DENY_NONE,
                        self.session.clientid,
                        &self.session.path_owner.name,
                        make_open_how(create, self.session.path_owner.verifier),
                        &leaf,
                    ),
                }
                opens_in_chunk += 1;
                map.note_ops(1);
                c.getfh();
                map.note_ops(1);
                if op.create == OpenCreate::Exclusive {
                    // EXCLUSIVE create always created the file; apply mode.
                    c.setattr(Some(op.mode.unwrap_or(0o644) & 0o7777), None);
                    map.note_ops(1);
                }
                map.end();
                cursor.descend();
                payload += est;
                global += 1;
                if map.next + per_file > budget && global < n {
                    break;
                }
            }
            if map.ranges.is_empty() {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
            self.op_budget().ensure(&c)?;
            self.session.path_owner.seqid = base_seq + opens_in_chunk as u32;
            let res = self.call_compound(&mut c)?;
            if let Some((caller, st)) = first_failed_range(&res, &map.ranges) {
                failed = Some((caller, st));
                // Keep the prefix opens (the caller resumes from here).
                for (c, s, e) in &map.ranges {
                    if *c >= caller {
                        continue;
                    }
                    for j in *s..*e {
                        if res.op(j).resop == nfs_opnum4_NFS4_OP_OPEN {
                            let stateid = res.open(j).stateid;
                            let fh = res.getfh(j + 1);
                            opened[*c] = Some((FileHandle::from_nfs_fh(fh), stateid));
                        }
                    }
                }
                break;
            }
            for (caller, s, e) in &map.ranges {
                for j in *s..*e {
                    if res.op(j).resop == nfs_opnum4_NFS4_OP_OPEN {
                        let stateid = res.open(j).stateid;
                        let fh = res.getfh(j + 1);
                        opened[*caller] = Some((FileHandle::from_nfs_fh(fh), stateid));
                    }
                }
            }
            if chunk_start == global {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
        }
        Ok(PathOpenOutcome { opened, failed })
    }

    /// Batched path-based REMOVEs in one compound per chunk. REMOVE keeps
    /// the current fh on the parent, so same-directory removals chain.
    pub fn removev_path_compound(&mut self, paths: &[Vec<u8>]) -> RpcResult<PathRemoveOutcome> {
        let n = paths.len();
        let mut removed: Vec<Option<()>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 3; // RESTOREFH + REMOVE + margin
        let reserve = 16;
        let budget = self.op_budget().merged_limit(reserve, per_file);
        let mut global = 0usize;
        while global < n {
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = OpMap::new();
            let mut c = Compound::new();
            c.tag(b"removev1");
            let mut payload = 0usize;
            while global < n && map.next + per_file <= budget {
                let est = 128;
                if payload > 0
                    && self.max_compound_bytes > 0
                    && payload + est > self.max_compound_bytes
                {
                    break;
                }
                map.begin(global);
                let (leaf, nops) = match cursor.set_parent(&mut c, &paths[global]) {
                    Some(x) => x,
                    None => {
                        failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                        global = n;
                        break;
                    }
                };
                map.note_ops(nops);
                c.remove(&leaf);
                map.note_ops(1);
                map.end();
                // REMOVE leaves the current fh on the parent: cursor state
                // is unchanged.
                payload += est;
                global += 1;
                if map.next + per_file > budget && global < n {
                    break;
                }
            }
            if map.ranges.is_empty() {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
            let res = self.call_compound(&mut c)?;
            let done = first_failed_range(&res, &map.ranges);
            match done {
                Some((caller, st)) => {
                    failed = Some((caller, st));
                    for (c, _, _) in &map.ranges {
                        if *c < caller {
                            removed[*c] = Some(());
                        }
                    }
                    break;
                }
                None => {
                    for (c, _, _) in &map.ranges {
                        removed[*c] = Some(());
                    }
                }
            }
            if chunk_start == global {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
        }
        Ok(PathRemoveOutcome { removed, failed })
    }

    /// Batched path-based RENAMEs in one compound per chunk. Each pair is
    /// `[.. SAVEFH src-dir, .., RENAME]`; RENAME uses the saved fh as the
    /// source directory and the current fh as the destination directory.
    pub fn renamev_path_compound(
        &mut self,
        pairs: &[PathRenamePair],
    ) -> RpcResult<PathRenameOutcome> {
        let n = pairs.len();
        let mut renamed: Vec<Option<()>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 8; // two dir resolutions + RENAME + margin
        let reserve = 16;
        let budget = self.op_budget().merged_limit(reserve, per_file);
        let mut global = 0usize;
        while global < n {
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = OpMap::new();
            let mut c = Compound::new();
            c.tag(b"renamev1");
            let mut payload = 0usize;
            while global < n && map.next + per_file <= budget {
                let pair = &pairs[global];
                let est = 256;
                if payload > 0
                    && self.max_compound_bytes > 0
                    && payload + est > self.max_compound_bytes
                {
                    break;
                }
                map.begin(global);
                let (sname, snops) = match cursor.set_parent(&mut c, &pair.src) {
                    Some(x) => x,
                    None => {
                        failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                        global = n;
                        break;
                    }
                };
                map.note_ops(snops);
                let (dname, dnops) = match cursor.set_current_parent(&mut c, &pair.dst) {
                    Some(x) => x,
                    None => {
                        failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                        global = n;
                        break;
                    }
                };
                map.note_ops(dnops);
                c.rename(&sname, &dname);
                map.note_ops(1);
                map.end();
                cursor.descend(); // RENAME moved the current fh
                payload += est;
                global += 1;
                if map.next + per_file > budget && global < n {
                    break;
                }
            }
            if map.ranges.is_empty() {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
            let res = self.call_compound(&mut c)?;
            let done = first_failed_range(&res, &map.ranges);
            match done {
                Some((caller, st)) => {
                    failed = Some((caller, st));
                    for (c, _, _) in &map.ranges {
                        if *c < caller {
                            renamed[*c] = Some(());
                        }
                    }
                    break;
                }
                None => {
                    for (c, _, _) in &map.ranges {
                        renamed[*c] = Some(());
                    }
                }
            }
            if chunk_start == global {
                failed.get_or_insert((chunk_start, nfsstat4_NFS4ERR_TOO_MANY_OPS));
                break;
            }
        }
        Ok(PathRenameOutcome { renamed, failed })
    }

    fn close_many_slot(&mut self, ops: &[CloseOp], slot: OwnerSlot) -> RpcResult<()> {
        let base = match slot {
            OwnerSlot::User => self.session.open_owner.seqid,
            OwnerSlot::Path => self.session.path_owner.seqid,
        };
        let n = ops.len();
        let _ = self.batch_ops::<CloseOp, ()>(
            b"closev",
            2,
            ops,
            |c, op, gi| {
                c.putfh(&op.fh.as_nfs_fh());
                c.close(base + gi as u32, &op.stateid);
            },
            |_, _| (),
        )?;
        match slot {
            OwnerSlot::User => self.session.open_owner.seqid = base + n as u32,
            OwnerSlot::Path => self.session.path_owner.seqid = base + n as u32,
        }
        Ok(())
    }

    /// READ `count` bytes at `offset`; returns the data read and the server's
    /// EOF flag.
    pub fn read(
        &mut self,
        fh: &FileHandle,
        stateid: &stateid4,
        offset: u64,
        count: u32,
    ) -> RpcResult<(Vec<u8>, bool)> {
        let mut c = Compound::new();
        c.tag(b"read");
        c.putfh(&fh.as_nfs_fh());
        c.read(stateid, offset, count);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let ok = res.read(2);
        let len = ok.data.data_len as usize;
        let data = if len == 0 {
            Vec::new()
        } else {
            let data = unsafe { std::slice::from_raw_parts(ok.data.data_val as *const u8, len) };
            data.to_vec()
        };
        Ok((data, ok.eof != 0))
    }

    /// WRITE `data` at `offset` with FILE_SYNC stability; returns bytes
    /// written and committed.
    pub fn write(
        &mut self,
        fh: &FileHandle,
        stateid: &stateid4,
        offset: u64,
        data: &[u8],
    ) -> RpcResult<(u32, u32)> {
        let mut c = Compound::new();
        c.tag(b"write");
        c.putfh(&fh.as_nfs_fh());
        c.write(stateid, offset, stable_how4_FILE_SYNC4, data);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        let ok = res.write(2);
        Ok((ok.count, ok.committed))
    }

    /// Copy bytes entirely on an NFSv4.2 server. The source filehandle is
    /// saved and the destination is current as required by RFC 7862.
    pub fn copy(&mut self, op: &CopyOp) -> RpcResult<u64> {
        Ok(self.copy_many(std::slice::from_ref(op))?[0])
    }

    /// Issue multiple COPY operations in as few compounds as the negotiated
    /// operation limit permits.
    pub fn copy_many(&mut self, ops: &[CopyOp]) -> RpcResult<Vec<u64>> {
        self.batch_ops(
            b"copyv",
            4,
            ops,
            |c, op, _| {
                c.putfh(&op.src_fh.as_nfs_fh());
                c.savefh();
                c.putfh(&op.dst_fh.as_nfs_fh());
                c.copy(
                    &op.src_stateid,
                    &op.dst_stateid,
                    op.src_offset,
                    op.dst_offset,
                    op.count,
                );
            },
            |res, i| res.copy(4 + 4 * i).cr_response.wr_count,
        )
    }

    /// CLOSE the open file.
    pub fn close(&mut self, fh: &FileHandle, stateid: &stateid4) -> RpcResult<()> {
        self.close_slot(fh, stateid, OwnerSlot::User)
    }

    /// Like [`close`](Self::close) but using the path-op open owner.
    pub fn close_path(&mut self, fh: &FileHandle, stateid: &stateid4) -> RpcResult<()> {
        self.close_slot(fh, stateid, OwnerSlot::Path)
    }

    fn close_slot(
        &mut self,
        fh: &FileHandle,
        stateid: &stateid4,
        slot: OwnerSlot,
    ) -> RpcResult<()> {
        let seqid = match slot {
            OwnerSlot::User => self.session.open_owner.seqid,
            OwnerSlot::Path => self.session.path_owner.seqid,
        };
        let mut c = Compound::new();
        c.tag(b"close");
        c.putfh(&fh.as_nfs_fh());
        c.close(seqid, stateid);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        match slot {
            OwnerSlot::User => self.session.open_owner.seqid += 1,
            OwnerSlot::Path => self.session.path_owner.seqid += 1,
        }
        Ok(())
    }

    /// CREATE a new object below `dir`. `ftype` is the object type (NF4DIR
    /// for mkdir, NF4LNK for a symlink with `linkdata`). Returns the handle
    /// of the new object.
    fn create(
        &mut self,
        dir: &FileHandle,
        name: &str,
        ftype: nfs_ftype4,
        linkdata: Option<&str>,
    ) -> RpcResult<FileHandle> {
        let mut c = Compound::new();
        c.tag(b"create");
        c.putfh(&dir.as_nfs_fh());
        c.create(name.as_bytes(), ftype, linkdata.map(|s| s.as_bytes()));
        c.getfh();
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(FileHandle::from_nfs_fh(res.getfh(3)))
    }

    /// Create a directory `name` below `dir`.
    pub fn mkdir(&mut self, dir: &FileHandle, name: &str) -> RpcResult<FileHandle> {
        self.create(dir, name, nfs_ftype4_NF4DIR, None)
    }

    /// Create a symbolic link `name` below `dir` pointing at `target`.
    pub fn symlink(&mut self, dir: &FileHandle, name: &str, target: &str) -> RpcResult<FileHandle> {
        self.create(dir, name, nfs_ftype4_NF4LNK, Some(target))
    }

    /// Read the target of the symlink at `fh`.
    pub fn readlink(&mut self, fh: &FileHandle) -> RpcResult<Vec<u8>> {
        let mut c = Compound::new();
        c.tag(b"readlink");
        c.putfh(&fh.as_nfs_fh());
        c.readlink();
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(res.readlink(2).to_vec())
    }

    /// GETATTR the requested FATTR4 attributes of `fh`; returns the raw XDR
    /// attribute list in request order.
    pub fn getattr(&mut self, fh: &FileHandle, attrs: &[u32]) -> RpcResult<Vec<u8>> {
        let mut c = Compound::new();
        c.tag(b"getattr");
        c.putfh(&fh.as_nfs_fh());
        c.getattr(attrs);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(res.getattr_bytes(2))
    }

    /// SETATTR mode and/or size on `fh`.
    pub fn setattr(
        &mut self,
        fh: &FileHandle,
        mode: Option<u32>,
        size: Option<u64>,
    ) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"setattr");
        c.putfh(&fh.as_nfs_fh());
        c.setattr(mode, size);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// READDIR `dir` starting at `cookie`; returns entries with names, next
    /// cookies, and requested attributes. Skips "." and "..".
    /// READDIR `dir` from `cookie`, requesting the given FATTR4 attributes
    /// for each entry.
    pub fn readdir(
        &mut self,
        dir: &FileHandle,
        cookie: u64,
        attrs: &[u32],
    ) -> RpcResult<Vec<DirEntry>> {
        let mut c = Compound::new();
        c.tag(b"readdir");
        c.putfh(&dir.as_nfs_fh());
        let zeroverf: verifier4 = [0; 8];
        let (dircount, maxcount) = self.readdir_limits(1);
        c.readdir(cookie, &zeroverf, dircount, maxcount, attrs);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(Self::collect_readdir(res.readdir(2)).0)
    }

    /// Extract the entries and the next cookie from a decoded READDIR reply.
    fn collect_readdir(ok: &READDIR4resok) -> (Vec<DirEntry>, u64) {
        let mut out = Vec::new();
        let mut cookie = 0u64;
        let mut e = ok.reply.entries;
        while !e.is_null() {
            let ent = unsafe { &*e };
            let name_len = ent.name.utf8string_len as usize;
            let name = if name_len == 0 {
                Vec::new()
            } else {
                unsafe {
                    std::slice::from_raw_parts(ent.name.utf8string_val as *const u8, name_len)
                }
                .to_vec()
            };
            if name != b"." && name != b".." {
                let attrs_len = ent.attrs.attr_vals.attrlist4_len as usize;
                let attrs = if attrs_len == 0 {
                    Vec::new()
                } else {
                    unsafe {
                        std::slice::from_raw_parts(
                            ent.attrs.attr_vals.attrlist4_val as *const u8,
                            attrs_len,
                        )
                    }
                    .to_vec()
                };
                out.push(DirEntry {
                    name,
                    cookie: ent.cookie,
                    attrs,
                });
            }
            cookie = ent.cookie;
            e = ent.nextentry;
        }
        (out, cookie)
    }

    /// For each `(parent, child_name)` pair, LOOKUP the child, GETFH its
    /// handle, and READDIR its first page -- all in as few compounds as
    /// possible (`[PUTFH parent, LOOKUP, GETFH, READDIR]` per child), each
    /// READDIR requesting `attrs` per entry.
    pub fn readdir_children(
        &mut self,
        ops: &[(FileHandle, Vec<u8>)],
        attrs: &[u32],
    ) -> RpcResult<Vec<ChildListing>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let per_chunk = self.op_budget().batch_capacity(4)?;
        let mut out = Vec::with_capacity(ops.len());
        let zeroverf: verifier4 = [0; 8];
        for chunk in ops.chunks(per_chunk) {
            let (dircount, maxcount) = self.readdir_limits(chunk.len());
            let map = |op_index: usize| {
                let local = op_index.saturating_sub(1) / 4;
                chunk.len().saturating_sub(1).min(local)
            };
            let mut c = Compound::new();
            c.tag(b"readdir_children");
            for (pfh, name) in chunk {
                c.putfh(&pfh.as_nfs_fh());
                c.lookup(name);
                c.getfh();
                c.readdir(0, &zeroverf, dircount, maxcount, attrs);
            }
            let res = self.call_compound(&mut c).map_err(|e| {
                let idx = map(e.op_index);
                e.with_op_index(idx)
            })?;
            self.session.expect_all_ok(&res).map_err(|e| {
                let idx = map(e.op_index);
                e.with_op_index(idx)
            })?;
            for (i, _) in chunk.iter().enumerate() {
                let fh = res.getfh(3 + 4 * i);
                let (entries, cookie) = Self::collect_readdir(res.readdir(4 + 4 * i));
                out.push(ChildListing {
                    fh: FileHandle::from_nfs_fh(fh),
                    entries,
                    cookie,
                });
            }
        }
        Ok(out)
    }

    /// For each `(fh, cookie)`, continue READDIR with the next page in as few
    /// compounds as possible (`[PUTFH fh, READDIR(cookie)]` per dir), each
    /// READDIR requesting `attrs` per entry.
    pub fn readdir_pages(
        &mut self,
        ops: &[(FileHandle, u64)],
        attrs: &[u32],
    ) -> RpcResult<Vec<(Vec<DirEntry>, u64)>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let per_chunk = self.op_budget().batch_capacity(2)?;
        let mut out = Vec::with_capacity(ops.len());
        let zeroverf: verifier4 = [0; 8];
        for chunk in ops.chunks(per_chunk) {
            let (dircount, maxcount) = self.readdir_limits(chunk.len());
            let map = |op_index: usize| {
                let local = op_index.saturating_sub(1) / 2;
                chunk.len().saturating_sub(1).min(local)
            };
            let mut c = Compound::new();
            c.tag(b"readdir_pages");
            for (fh, cookie) in chunk {
                c.putfh(&fh.as_nfs_fh());
                c.readdir(*cookie, &zeroverf, dircount, maxcount, attrs);
            }
            let res = self.call_compound(&mut c).map_err(|e| {
                let idx = map(e.op_index);
                e.with_op_index(idx)
            })?;
            self.session.expect_all_ok(&res).map_err(|e| {
                let idx = map(e.op_index);
                e.with_op_index(idx)
            })?;
            for (i, _) in chunk.iter().enumerate() {
                let (entries, cookie) = Self::collect_readdir(res.readdir(2 + 2 * i));
                out.push((entries, cookie));
            }
        }
        Ok(out)
    }

    /// REMOVE `name` from directory `dir`.
    pub fn remove(&mut self, dir: &FileHandle, name: &str) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"remove");
        c.putfh(&dir.as_nfs_fh());
        c.remove(name.as_bytes());
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// RENAME `oldname` out of `srcdir` to `newname` in `dstdir`. The kernel
    /// reads the source directory from the saved filehandle, so we PUTFH the
    /// source dir, SAVEFH it, then PUTFH the target dir before the RENAME op.
    pub fn rename(
        &mut self,
        srcdir: &FileHandle,
        oldname: &str,
        dstdir: &FileHandle,
        newname: &str,
    ) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"rename");
        c.putfh(&srcdir.as_nfs_fh());
        c.savefh();
        c.putfh(&dstdir.as_nfs_fh());
        c.rename(oldname.as_bytes(), newname.as_bytes());
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// Create a hard link named `newname` in directory `dir` to `src`.
    /// Requires the saved-fh trick: SAVEFH the source, then LINK into `dir`.
    pub fn link(&mut self, dir: &FileHandle, src: &FileHandle, newname: &str) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"link");
        c.putfh(&src.as_nfs_fh());
        c.savefh();
        c.putfh(&dir.as_nfs_fh());
        c.link(newname.as_bytes());
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }
}

fn session_mount_root(session: &mut Session) -> RpcResult<FileHandle> {
    let mut c = Compound::new();
    c.tag(b"mount");
    c.putrootfh();
    c.getfh();
    let res = session.compound(&mut c)?;
    session.expect_all_ok(&res)?;
    Ok(FileHandle::from_nfs_fh(res.getfh(2)))
}

/// Build the `openflag4` for an OPEN based on the create mode.
fn make_open_how(create: OpenCreate, verifier: verifier4) -> openflag4 {
    match create {
        OpenCreate::NoCreate => openflag4 {
            opentype: opentype4_OPEN4_NOCREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: unsafe { std::mem::zeroed() },
            },
        },
        OpenCreate::Exclusive => openflag4 {
            opentype: opentype4_OPEN4_CREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: createhow4 {
                    mode: createmode4_EXCLUSIVE4,
                    createhow4_u: createhow4__bindgen_ty_1 {
                        createverf: verifier,
                    },
                },
            },
        },
        OpenCreate::Guarded => openflag4 {
            opentype: opentype4_OPEN4_CREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: createhow4 {
                    mode: createmode4_GUARDED4,
                    createhow4_u: createhow4__bindgen_ty_1 {
                        createattrs: fattr4 {
                            attrmask: bitmap4 {
                                bitmap4_len: 0,
                                map: [0; 3],
                            },
                            attr_vals: attrlist4 {
                                attrlist4_len: 0,
                                attrlist4_val: std::ptr::null_mut(),
                            },
                        },
                    },
                },
            },
        },
        OpenCreate::Unchecked => openflag4 {
            opentype: opentype4_OPEN4_CREATE,
            openflag4_u: openflag4__bindgen_ty_1 {
                how: createhow4 {
                    mode: createmode4_UNCHECKED4,
                    createhow4_u: createhow4__bindgen_ty_1 {
                        createattrs: fattr4 {
                            attrmask: bitmap4 {
                                bitmap4_len: 0,
                                map: [0; 3],
                            },
                            attr_vals: attrlist4 {
                                attrlist4_len: 0,
                                attrlist4_val: std::ptr::null_mut(),
                            },
                        },
                    },
                },
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiated_op_budgets_include_sequence() {
        assert_eq!(CompoundBudget::new(4).batch_capacity(3).unwrap(), 1);
        assert!(CompoundBudget::new(4).batch_capacity(4).is_err());
        assert_eq!(CompoundBudget::new(8).batch_capacity(4).unwrap(), 1);
        assert_eq!(CompoundBudget::new(32).batch_capacity(4).unwrap(), 7);
    }

    #[test]
    fn merged_budgets_never_exceed_server_limit() {
        for max_ops in [4, 8, 32] {
            let budget = CompoundBudget::new(max_ops);
            let limit = budget.merged_limit(16, 3);
            assert!(limit <= max_ops);
            if max_ops >= 4 {
                assert!(limit >= 4);
            }
        }
        assert_eq!(CompoundBudget::new(4).merged_limit(16, 4), 0);
    }

    #[test]
    fn negotiated_byte_limits_have_no_sixty_four_kib_floor() {
        let below_floor = 32 * 1024;
        assert_eq!(effective_request_limit(below_floor, None), below_floor);
        assert_eq!(
            effective_request_limit(below_floor, Some(4 * 1024 * 1024)),
            below_floor
        );
        assert_eq!(response_payload_budget(below_floor), 24 * 1024);
    }

    #[test]
    fn zero_configuration_cannot_bypass_negotiated_limit() {
        let negotiated = 48 * 1024;
        assert_eq!(effective_request_limit(negotiated, None), negotiated);
        assert_eq!(
            effective_request_limit(negotiated, Some(96 * 1024)),
            negotiated
        );
        assert_eq!(
            effective_request_limit(negotiated, Some(16 * 1024)),
            16 * 1024
        );
    }
}
