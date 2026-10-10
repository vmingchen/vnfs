//! High-level NFSv4.1 operations on top of the session.

// bindgen emits lowercase constants (e.g. nfs_opnum4_NFS4_OP_WRITE) matched
// here in patterns; silence the style lint for those.
#![allow(non_upper_case_globals)]

use std::os::raw::c_char;
#[cfg(feature = "test-faults")]
use std::sync::Arc;
use std::time::Duration;

use nfsv41_sys::*;

use crate::compound::{Compound, CompoundRes};
use crate::error::{RpcError, RpcResult};
use crate::path::{components_bytes, split_path_bytes};
use crate::planner::{
    AdaptiveCompoundLimits, ExecutionMap, FailureCause, RecoveryAction, RequestSafety,
    recovery_action, resource_rejected_before_mutation,
};
use crate::session::Session;
#[cfg(feature = "test-faults")]
use vfsi_core::internal::faults::{FaultInjector, OpenFaultPoint};

/// An NFS file handle owned by the client.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
    /// Caller cap for an encoded request, including a reserved RPC/auth
    /// envelope. The effective cap never exceeds CREATE_SESSION's confirmed
    /// `ca_maxrequestsize`.
    max_compound_bytes: usize,
    /// Maximum reply bytes per compound (bounds total READ data, which
    /// travels in the reply). Separate from `max_compound_bytes` because
    /// servers commonly grant much larger requests than replies.
    pub max_response_bytes: usize,
    /// Server-confirmed maximum operations per compound (merged builders).
    pub max_ops: usize,
    compound_limits: AdaptiveCompoundLimits,
    server_max_request_bytes: usize,
    configured_max_request_bytes: Option<std::num::NonZeroUsize>,
    rpc_envelope_reserve: usize,
    deferred_path_closes: Vec<CloseOp>,
    next_public_open_owner_id: u64,
    #[cfg(feature = "test-faults")]
    fault_injector: Option<Arc<dyn FaultInjector>>,
    #[cfg(feature = "test-faults")]
    confirmed_path_closes: usize,
    #[cfg(feature = "test-faults")]
    reject_next_compound_tag: Option<Vec<u8>>,
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
pub struct WriteOp<'a> {
    pub fh: FileHandle,
    pub stateid: stateid4,
    pub offset: u64,
    pub data: &'a [u8],
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
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<(i64, u32)>,
    pub mtime: Option<(i64, u32)>,
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

fn remove_result_index(op_index: usize, item_count: usize) -> usize {
    // The response starts with SEQUENCE and PUTFH before the first REMOVE.
    op_index.saturating_sub(2).min(item_count.saturating_sub(1))
}

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

fn effective_request_limit(server: usize, configured: Option<std::num::NonZeroUsize>) -> usize {
    configured.map_or(server, |limit| limit.get().min(server))
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
pub struct PathWriteOp<'a> {
    pub file: FileRef,
    pub offset: u64,
    pub data: &'a [u8],
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

#[derive(Clone, Copy)]
struct PathIoOpen {
    access: u32,
    create: OpenCreate,
    truncate_create: bool,
    truncate_existing: bool,
}

enum PathIoChunkResult {
    Repack { max_items: usize },
    Retry,
    Response(CompoundRes),
}

struct PathIoTarget<'a> {
    compound: &'a mut Compound,
    cursor: &'a mut CfhCursor,
    map: &'a mut ExecutionMap,
    close_in_compound: bool,
    opened_path: &'a mut Option<Vec<u8>>,
    fh_at_opened: &'a mut bool,
    base_seq: u32,
    opens_in_chunk: &'a mut usize,
}

struct PathIoChunk<'a> {
    tag: &'a [u8],
    one_item_ops: usize,
    reserve: usize,
    per_file: usize,
    old_budget: usize,
    allow_repack: bool,
    compound: &'a mut Compound,
    map: &'a ExecutionMap,
    base_seq: u32,
    opens_in_chunk: usize,
    safety: RequestSafety,
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
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<(i64, u32)>,
    pub mtime: Option<(i64, u32)>,
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

// Keep compound safety classification shared by mutation and resource retries.
fn batch_is_read_only(tag: &[u8]) -> bool {
    matches!(tag, b"getattrv" | b"vstatfs_impl" | b"readlinkv" | b"readv")
}

fn batch_resource_progress(
    tag: &[u8],
    per_op: usize,
    status: u32,
    failed_op: Option<usize>,
) -> Option<usize> {
    if batch_is_read_only(tag)
        && matches!(
            status,
            nfsstat4_NFS4ERR_RESOURCE | nfsstat4_NFS4ERR_TOO_MANY_OPS
        )
    {
        // SEQUENCE occupies slot zero; only complete caller items are retained.
        Some(failed_op.unwrap_or(0).saturating_sub(1) / per_op)
    } else {
        None
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

fn path_io_chunk_end(
    start: usize,
    total: usize,
    per_op: usize,
    room: usize,
    available_ops: usize,
    allow_first: bool,
) -> Option<usize> {
    let remaining = total.saturating_sub(start);
    let chunks_total = remaining.div_ceil(per_op).max(1);
    let per_chunk = per_op.min(remaining).max(1);
    let by_bytes = if room >= per_chunk {
        (room / per_chunk).max(1)
    } else {
        0
    };
    let mut take = chunks_total.min(available_ops).min(by_bytes);
    if take == 0 {
        if allow_first {
            take = 1;
        } else {
            return None;
        }
    }
    let end = (start + take * per_op).min(total);
    if end == start && remaining > 0 {
        None
    } else {
        Some(end)
    }
}

fn first_failed_range(res: &CompoundRes, map: &ExecutionMap) -> RpcResult<Option<(usize, u32)>> {
    let report = map
        .analyze(res)
        .map_err(|error| RpcError::transport(format!("malformed NFS COMPOUND reply: {error}")))?;
    if let Some(status) = report.compound_failure {
        return Err(RpcError::op(0, status));
    }
    Ok(report
        .failure
        .map(|failure| (failure.caller, failure.status)))
}

impl NfsClient {
    /// Inject one pre-dispatch NFS4ERR_RESOURCE response for this compound
    /// shape. Test-only: no request is sent and no mutation can have run.
    #[cfg(feature = "test-faults")]
    pub fn inject_resource_rejection_once(&mut self, tag: &[u8]) {
        self.reject_next_compound_tag = Some(tag.to_vec());
    }

    #[cfg(feature = "test-faults")]
    pub fn resource_rejection_pending(&self) -> bool {
        self.reject_next_compound_tag.is_some()
    }

    #[cfg(feature = "test-faults")]
    pub fn set_fault_injector(&mut self, injector: Arc<dyn FaultInjector>) {
        self.fault_injector = Some(injector);
    }

    #[cfg(feature = "test-faults")]
    pub fn confirmed_path_closes(&self) -> usize {
        self.confirmed_path_closes
    }

    #[cfg(feature = "test-faults")]
    pub fn deferred_path_close_count(&self) -> usize {
        self.deferred_path_closes.len()
    }

    fn close_path_or_defer(&mut self, closes: Vec<CloseOp>) {
        if closes.is_empty() {
            return;
        }
        if self.close_many(&closes).is_err() {
            self.deferred_path_closes.extend(closes);
        }
    }

    fn drain_deferred_path_closes(&mut self) -> RpcResult<()> {
        let mut remaining = std::mem::take(&mut self.deferred_path_closes);
        while !remaining.is_empty() {
            let close = remaining.remove(0);
            match self.close(&close.fh, &close.stateid) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.status,
                        nfsstat4_NFS4ERR_BAD_STATEID | nfsstat4_NFS4ERR_OLD_STATEID
                    ) => {}
                Err(error) => {
                    self.deferred_path_closes.push(close);
                    self.deferred_path_closes.extend(remaining);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn next_public_open_owner_name(&mut self) -> Vec<u8> {
        let id = self.next_public_open_owner_id;
        self.next_public_open_owner_id = id.wrapping_add(1);
        let mut name = self.session.open_owner.name.clone();
        name.push(b'-');
        name.extend_from_slice(id.to_string().as_bytes());
        name
    }

    pub(crate) fn abandon(&mut self) {
        self.session.abandon();
    }

    pub(crate) fn shutdown(&mut self) -> RpcResult<()> {
        self.session.shutdown()
    }

    /// Connect, run the session handshake, and resolve the export root.
    pub fn connect(host: &str) -> RpcResult<NfsClient> {
        crate::session::negotiate_minor(|minorversion| Self::connect_minor(host, minorversion))
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
        Self::connect_with_authentication(
            host,
            connect_timeout,
            request_timeout,
            &crate::rpc::NfsAuthentication::AuthSys,
        )
    }

    /// Connect and negotiate a minor version using explicit authentication.
    pub fn connect_with_authentication(
        host: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
        authentication: &crate::rpc::NfsAuthentication,
    ) -> RpcResult<NfsClient> {
        Self::connect_with_identity(
            host,
            connect_timeout,
            request_timeout,
            authentication,
            None,
            None,
        )
    }

    pub(crate) fn connect_with_identity(
        host: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
        authentication: &crate::rpc::NfsAuthentication,
        client_owner: Option<&[u8]>,
        client_verifier: Option<verifier4>,
    ) -> RpcResult<NfsClient> {
        crate::session::negotiate_minor(|minorversion| {
            Self::connect_minor_with_identity(
                host,
                minorversion,
                connect_timeout,
                request_timeout,
                authentication,
                client_owner,
                client_verifier,
            )
        })
    }

    pub fn connect_minor_with_timeouts(
        host: &str,
        minorversion: u32,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> RpcResult<NfsClient> {
        Self::connect_minor_with_authentication(
            host,
            minorversion,
            connect_timeout,
            request_timeout,
            &crate::rpc::NfsAuthentication::AuthSys,
        )
    }

    /// Connect with an explicit minor version, timeouts, and authentication.
    pub fn connect_minor_with_authentication(
        host: &str,
        minorversion: u32,
        connect_timeout: Duration,
        request_timeout: Duration,
        authentication: &crate::rpc::NfsAuthentication,
    ) -> RpcResult<NfsClient> {
        Self::connect_minor_with_identity(
            host,
            minorversion,
            connect_timeout,
            request_timeout,
            authentication,
            None,
            None,
        )
    }

    pub(crate) fn connect_minor_with_identity(
        host: &str,
        minorversion: u32,
        connect_timeout: Duration,
        request_timeout: Duration,
        authentication: &crate::rpc::NfsAuthentication,
        client_owner: Option<&[u8]>,
        client_verifier: Option<verifier4>,
    ) -> RpcResult<NfsClient> {
        let mut session = Session::connect_minor_with_identity(
            host,
            minorversion,
            connect_timeout,
            request_timeout,
            authentication,
            client_owner,
            client_verifier,
        )?;
        let root = session_mount_root(&mut session)?;
        let configured_max_request_bytes = std::num::NonZeroUsize::new(DEFAULT_MAX_COMPOUND_BYTES);
        let max_compound_bytes =
            effective_request_limit(session.max_requestsize, configured_max_request_bytes);
        let max_response_bytes = session.max_responsesize.min(DEFAULT_MAX_COMPOUND_BYTES);
        let max_ops = session.max_operations.min(MAX_COMPOUND_OPS);
        let server_max_request_bytes = session.max_requestsize;
        let rpc_envelope_reserve = match authentication {
            crate::rpc::NfsAuthentication::AuthSys => 1024,
            #[cfg(feature = "rpcsec-gss")]
            crate::rpc::NfsAuthentication::RpcsecGss { .. } => 8192,
        };
        if max_compound_bytes <= rpc_envelope_reserve + 128 {
            return Err(RpcError::transport(format!(
                "negotiated NFS request limit {max_compound_bytes} is too small for the RPC/authentication envelope"
            )));
        }
        Ok(NfsClient {
            session,
            root,
            max_compound_bytes,
            max_response_bytes,
            max_ops,
            compound_limits: AdaptiveCompoundLimits::new(max_ops),
            server_max_request_bytes,
            configured_max_request_bytes,
            rpc_envelope_reserve,
            deferred_path_closes: Vec::new(),
            next_public_open_owner_id: 0,
            #[cfg(feature = "test-faults")]
            fault_injector: None,
            #[cfg(feature = "test-faults")]
            confirmed_path_closes: 0,
            #[cfg(feature = "test-faults")]
            reject_next_compound_tag: None,
        })
    }

    /// Minor version negotiated for this session.
    pub fn minorversion(&self) -> u32 {
        self.session.minorversion
    }

    /// Set the configured per-compound payload cap. Zero restores the
    /// negotiated server maximum; callers can never exceed that maximum.
    pub fn set_max_compound_bytes(&mut self, bytes: usize) {
        self.configured_max_request_bytes = std::num::NonZeroUsize::new(bytes);
        self.max_compound_bytes = effective_request_limit(
            self.server_max_request_bytes,
            self.configured_max_request_bytes,
        );
    }

    /// Lower the operation ceiling for deterministic compound-splitting tests.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn test_limit_compound_operations(&mut self, limit: usize) -> usize {
        assert!(
            limit >= 8,
            "test ceiling must allow individual path operations"
        );
        self.max_ops = self.max_ops.min(limit);
        self.compound_limits = AdaptiveCompoundLimits::new(self.max_ops);
        self.max_ops
    }

    /// Per-op data cap: no single READ/WRITE op may carry more than the
    /// server's per-op limit (bounded by the compound cap as well).
    pub fn per_op_bytes(&self) -> usize {
        // Leave room for the compound header, SEQUENCE, filehandle, and op
        // metadata. The final op-count guard cannot protect a byte-size
        // limit, so a single payload must not fill the entire request.
        self.max_request_arg_bytes()
            .saturating_sub(1024)
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
        self.per_op_bytes()
            .min(self.read_compound_bytes().saturating_sub(256).max(1))
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

    fn op_budget_for(&self, tag: &[u8], one_item_ops: usize) -> CompoundBudget {
        CompoundBudget::new(
            self.compound_limits
                .limit(tag)
                .min(self.max_ops)
                .max(one_item_ops.min(self.max_ops)),
        )
    }

    /// The negotiated request size includes the RPC header and authentication
    /// wrapper, not just COMPOUND XDR. Supported AUTH_SYS credentials fit in
    /// 1 KiB; Kerberos integrity wrapping gets a larger 8 KiB allowance.
    pub(crate) fn max_request_arg_bytes(&self) -> usize {
        self.max_compound_bytes
            .min(self.server_max_request_bytes)
            .saturating_sub(self.rpc_envelope_reserve)
    }

    fn compound_request_fits(&self, compound: &Compound) -> RpcResult<bool> {
        let arg_limit = self
            .max_request_arg_bytes()
            .saturating_sub(crate::compound::SEQUENCE_XDR_BYTES);
        Ok(compound.encoded_len_up_to(arg_limit)?.is_some())
    }

    /// Path resolution has a variable number of LOOKUPs. The packing
    /// estimate is only a hint; check the finished compound before sending.
    /// A lone item may exceed the learned limit when its path itself is
    /// deeper than that limit, but still must fit the negotiated hard cap.
    fn merged_paths_need_repack(
        &self,
        tag: &[u8],
        one_item_ops: usize,
        compound: &Compound,
        items: usize,
    ) -> RpcResult<bool> {
        Ok(items > 1
            && (compound.op_count().saturating_add(1)
                > self.op_budget_for(tag, one_item_ops).max_ops
                || !self.compound_request_fits(compound)?))
    }

    /// The adaptive learner has already observed this reply in
    /// `call_compound`. Only a rejection before the first mutating operation
    /// permits rebuilding this chunk with its newly smaller budget.
    fn can_retry_merged_resource(
        &self,
        tag: &[u8],
        reply: &CompoundRes,
        old_budget: usize,
        reserve: usize,
        per_item: usize,
    ) -> bool {
        resource_rejected_before_mutation(reply)
            && self
                .op_budget_for(tag, 1 + per_item)
                .merged_limit(reserve, per_item)
                < old_budget
    }

    fn prepare_path_io_target(
        &mut self,
        target: &mut PathIoTarget<'_>,
        file: &FileRef,
        open: PathIoOpen,
    ) -> Option<bool> {
        match file {
            FileRef::Path(path) => {
                if target.opened_path.as_deref() == Some(path.as_slice()) && *target.fh_at_opened {
                    return Some(false);
                }
                if target.close_in_compound && target.opened_path.is_some() && *target.fh_at_opened
                {
                    target
                        .compound
                        .close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                    target.map.note_ops(1);
                    *target.opened_path = None;
                }
                let (leaf, nops) = target.cursor.set_parent(&mut *target.compound, path)?;
                target.map.note_ops(nops);
                let seqid = target.base_seq + *target.opens_in_chunk as u32;
                if open.truncate_create {
                    target.compound.open_claim_null_create_mode(
                        seqid,
                        open.access,
                        OPEN4_SHARE_DENY_NONE,
                        self.session.clientid,
                        &self.session.path_owner.name,
                        &leaf,
                        None,
                        true,
                    );
                } else {
                    target.compound.open_claim_null(
                        seqid,
                        open.access,
                        OPEN4_SHARE_DENY_NONE,
                        self.session.clientid,
                        &self.session.path_owner.name,
                        make_open_how(open.create, self.session.path_owner.verifier),
                        &leaf,
                    );
                }
                *target.opens_in_chunk += 1;
                target.map.note_ops(1);
                if !target.close_in_compound {
                    target.compound.getfh();
                    target.map.note_ops(1);
                }
                *target.opened_path = Some(path.clone());
                *target.fh_at_opened = true;
                if open.truncate_existing {
                    target
                        .compound
                        .setattr_with_stateid(None, Some(0), &SPECIAL_STATEID);
                    target.map.note_ops(1);
                }
                Some(true)
            }
            FileRef::Handle(handle) => {
                if target.close_in_compound && target.opened_path.is_some() && *target.fh_at_opened
                {
                    target
                        .compound
                        .close(SPECIAL_STATEID.seqid, &SPECIAL_STATEID);
                    target.map.note_ops(1);
                    *target.opened_path = None;
                }
                target.cursor.set_handle(&mut *target.compound, handle);
                target.map.note_ops(1);
                *target.fh_at_opened = false;
                Some(false)
            }
        }
    }

    fn send_path_io_chunk(&mut self, chunk: PathIoChunk<'_>) -> RpcResult<PathIoChunkResult> {
        let PathIoChunk {
            tag,
            one_item_ops,
            reserve,
            per_file,
            old_budget,
            allow_repack,
            compound,
            map,
            base_seq,
            opens_in_chunk,
            safety,
        } = chunk;
        if allow_repack
            && self.merged_paths_need_repack(tag, one_item_ops, compound, map.ranges.len())?
        {
            return Ok(PathIoChunkResult::Repack {
                max_items: map.ranges.len() - 1,
            });
        }
        CompoundBudget::new(self.max_ops).ensure(compound)?;
        self.session.path_owner.seqid = base_seq + opens_in_chunk as u32;
        let response = self.call_compound_with_safety(compound, safety)?;
        if resource_rejected_before_mutation(&response) {
            self.session.path_owner.seqid = base_seq;
            if self.can_retry_merged_resource(tag, &response, old_budget, reserve, per_file) {
                return Ok(PathIoChunkResult::Retry);
            }
        }
        Ok(PathIoChunkResult::Response(response))
    }

    fn can_retry_read_only_merged_resource(
        &self,
        tag: &[u8],
        reply: &CompoundRes,
        old_budget: usize,
        reserve: usize,
        per_item: usize,
    ) -> bool {
        (reply.status() == nfsstat4_NFS4ERR_RESOURCE
            || reply.status() == nfsstat4_NFS4ERR_TOO_MANY_OPS)
            && self
                .op_budget_for(tag, 1 + per_item)
                .merged_limit(reserve, per_item)
                < old_budget
    }

    /// REMOVE uses SEQUENCE, PUTFH, then one operation per item.
    pub(crate) fn remove_batch_capacity(&self) -> usize {
        self.op_budget_for(b"removev", 3)
            .max_ops
            .saturating_sub(2)
            .max(1)
    }

    /// LOOKUP uses SEQUENCE, then PUTFH/LOOKUP/GETFH per item.
    pub(crate) fn lookup_batch_capacity(&self) -> usize {
        (self.op_budget_for(b"lookupv", 4).max_ops.saturating_sub(1) / 3).max(1)
    }

    /// Send a compound only when its final size, including SEQUENCE, honors
    /// the server-confirmed `ca_maxoperations` value.
    fn call_compound(&mut self, compound: &mut Compound) -> RpcResult<CompoundRes> {
        CompoundBudget::new(self.max_ops).ensure(compound)?;
        if !self.compound_request_fits(compound)? {
            return Err(RpcError::op(0, nfsv41_sys::nfsstat4_NFS4ERR_REQ_TOO_BIG));
        }
        let sent_ops = compound.op_count().saturating_add(1);
        #[cfg(feature = "test-faults")]
        if self.reject_next_compound_tag.as_deref() == Some(compound.tag_bytes()) {
            self.reject_next_compound_tag = None;
            let reply = CompoundRes::injected_resource_rejection(nfsstat4_NFS4ERR_RESOURCE);
            self.compound_limits.observe(
                compound.tag_bytes(),
                sent_ops,
                reply.status(),
                reply.nops(),
            );
            return Ok(reply);
        }
        let reply = self.session.compound(compound)?;
        self.compound_limits
            .observe(compound.tag_bytes(), sent_ops, reply.status(), reply.nops());
        Ok(reply)
    }

    /// Send a compound while preserving whether a lost response makes the
    /// operation's outcome ambiguous. Session replay is not yet available,
    /// so mutating requests are never silently reissued after transport loss.
    fn call_compound_with_safety(
        &mut self,
        compound: &mut Compound,
        safety: RequestSafety,
    ) -> RpcResult<CompoundRes> {
        self.call_compound(compound).map_err(|mut error| {
            if error.is_transport()
                && recovery_action(
                    safety,
                    FailureCause::Transport {
                        exact_replay: false,
                    },
                    false,
                ) == RecoveryAction::Ambiguous
            {
                error.message = format!(
                    "ambiguous outcome for mutating compound; request was not retried: {}",
                    error.message
                );
            }
            error
        })
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
        let mut out: Vec<Option<Result<(FileHandle, u32), u32>>> =
            (0..ops.len()).map(|_| None).collect();
        let mut cursor = 0usize;
        let mut max_items = usize::MAX;
        while cursor < ops.len() {
            let batch_capacity = self
                .op_budget_for(b"lookup_typev", 5)
                .batch_capacity(4)?
                .min(max_items);
            let end = (cursor + batch_capacity).min(ops.len());
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"lookup_typev");
            for (caller, (dir, name)) in ops[cursor..end].iter().enumerate() {
                map.begin(cursor + caller);
                c.putfh(&dir.as_nfs_fh());
                c.lookup(name);
                c.getfh();
                c.getattr(&[FATTR4_TYPE]);
                map.note_ops(4);
                map.end();
            }
            if !self.compound_request_fits(&c)? && end - cursor > 1 {
                max_items = (end - cursor) / 2;
                continue;
            }
            max_items = usize::MAX;
            let res = self.call_compound_with_safety(&mut c, RequestSafety::ReadOnly)?;
            let report = map.analyze(&res).map_err(|error| {
                RpcError::transport(format!("malformed lookup_typev COMPOUND reply: {error}"))
            })?;

            if let Some(status) = report.compound_failure {
                if matches!(
                    status,
                    nfsstat4_NFS4ERR_TOO_MANY_OPS | nfsstat4_NFS4ERR_RESOURCE
                ) && end - cursor > 1
                {
                    debug_assert_eq!(
                        recovery_action(RequestSafety::ReadOnly, FailureCause::ResourceLimit, true,),
                        RecoveryAction::SplitAndRetry
                    );
                    continue;
                }
                return Err(RpcError::op(cursor, status));
            }

            for caller in report.completed {
                let (_, start, _) = map.range(caller).expect("reported range must exist");
                let fh = FileHandle::from_nfs_fh(res.getfh(start + 2));
                let bytes = res.getattr_bytes(start + 3);
                let ftype = if bytes.len() >= 4 {
                    u32::from_be_bytes(bytes[0..4].try_into().unwrap())
                } else {
                    0
                };
                out[caller] = Some(Ok((fh, ftype)));
            }
            if let Some(failure) = report.failure {
                if matches!(
                    failure.status,
                    nfsstat4_NFS4ERR_TOO_MANY_OPS | nfsstat4_NFS4ERR_RESOURCE
                ) && end - cursor > 1
                {
                    cursor = failure.caller;
                    continue;
                }
                out[failure.caller] = Some(Err(failure.status));
                debug_assert_eq!(
                    recovery_action(RequestSafety::ReadOnly, FailureCause::ItemStatus, true,),
                    RecoveryAction::ContinueSuffix
                );
                cursor = failure.caller + 1;
            } else {
                cursor = end;
            }
        }
        out.into_iter()
            .enumerate()
            .map(|(caller, value)| {
                value.ok_or_else(|| {
                    RpcError::transport(format!(
                        "lookup_typev planner omitted caller item {caller}"
                    ))
                })
            })
            .collect()
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
        let mut out: Vec<Option<Result<FileHandle, u32>>> = (0..ops.len()).map(|_| None).collect();
        let mut cursor = 0usize;
        let mut max_items = usize::MAX;
        while cursor < ops.len() {
            let batch_capacity = self
                .op_budget_for(b"lookupv", 4)
                .batch_capacity(3)?
                .min(max_items);
            let end = (cursor + batch_capacity).min(ops.len());
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"lookupv");
            for (caller, (dir, name)) in ops[cursor..end].iter().enumerate() {
                map.begin(cursor + caller);
                c.putfh(&dir.as_nfs_fh());
                c.lookup(name);
                c.getfh();
                map.note_ops(3);
                map.end();
            }
            if !self.compound_request_fits(&c)? && end - cursor > 1 {
                max_items = (end - cursor) / 2;
                continue;
            }
            max_items = usize::MAX;
            let res = self.call_compound_with_safety(&mut c, RequestSafety::ReadOnly)?;
            let report = map.analyze(&res).map_err(|error| {
                RpcError::transport(format!("malformed lookupv COMPOUND reply: {error}"))
            })?;

            if let Some(status) = report.compound_failure {
                if matches!(
                    status,
                    nfsstat4_NFS4ERR_TOO_MANY_OPS | nfsstat4_NFS4ERR_RESOURCE
                ) && end - cursor > 1
                {
                    continue;
                }
                return Err(RpcError::op(cursor, status));
            }
            for caller in report.completed {
                let (_, start, _) = map.range(caller).expect("reported range must exist");
                out[caller] = Some(Ok(FileHandle::from_nfs_fh(res.getfh(start + 2))));
            }
            if let Some(failure) = report.failure {
                if matches!(
                    failure.status,
                    nfsstat4_NFS4ERR_TOO_MANY_OPS | nfsstat4_NFS4ERR_RESOURCE
                ) && end - cursor > 1
                {
                    cursor = failure.caller;
                    continue;
                }
                out[failure.caller] = Some(Err(failure.status));
                cursor = failure.caller + 1;
            } else {
                cursor = end;
            }
        }
        out.into_iter()
            .enumerate()
            .map(|(caller, value)| {
                value.ok_or_else(|| {
                    RpcError::transport(format!("lookupv planner omitted caller item {caller}"))
                })
            })
            .collect()
    }

    /// Resolve a slash-separated path from the export root. LOOKUPs chain
    /// within each compound, with long paths split at the learned operation
    /// limit; a server-side resource rejection rebuilds the current chunk.
    pub fn resolve(&mut self, path: &[u8]) -> RpcResult<FileHandle> {
        let components = crate::path::components_bytes(path);
        if components.is_empty() {
            return Ok(self.root.clone());
        }
        let mut current = self.root.clone();
        let mut cursor = 0;
        let mut max_components = usize::MAX;
        while cursor < components.len() {
            // SEQUENCE, PUTFH, and GETFH leave room for this many LOOKUPs.
            let capacity = self.op_budget_for(b"resolve", 4).max_ops.saturating_sub(3);
            if capacity == 0 {
                return Err(RpcError::transport(
                    "NFS compound operation budget cannot resolve one path component",
                ));
            }
            let take = capacity.min(max_components).min(components.len() - cursor);
            let mut c = Compound::new();
            c.tag(b"resolve");
            c.putfh(&current.as_nfs_fh());
            for component in &components[cursor..cursor + take] {
                c.lookup(component);
            }
            c.getfh();
            if !self.compound_request_fits(&c)? && take > 1 {
                max_components = take / 2;
                continue;
            }
            max_components = usize::MAX;
            let res = self.call_compound(&mut c)?;
            if take > 1
                && matches!(
                    res.status(),
                    nfsstat4_NFS4ERR_RESOURCE | nfsstat4_NFS4ERR_TOO_MANY_OPS
                )
                && self.op_budget_for(b"resolve", 4).max_ops.saturating_sub(3) < take
            {
                continue;
            }
            self.session.expect_all_ok(&res)?;
            current = FileHandle::from_nfs_fh(res.getfh(2 + take));
            cursor += take;
        }
        Ok(current)
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
        self.readv_decode(ops, |_, data, eof| Ok((data.to_vec(), eof)))
    }

    /// Decode READ replies into caller storage while the compound reply is
    /// still alive. `on_data` sees each wire operation in request order.
    pub fn readv_into(
        &mut self,
        ops: &[ReadOp],
        mut on_data: impl FnMut(usize, &[u8]) -> RpcResult<()>,
    ) -> RpcResult<Vec<(usize, bool)>> {
        self.readv_decode(ops, |index, data, eof| {
            on_data(index, data)?;
            Ok((data.len(), eof))
        })
    }

    // One packing/validation engine; destinations are statically dispatched
    // while the reply storage is alive. Into reads never allocate owned data.
    fn readv_decode<R>(
        &mut self,
        ops: &[ReadOp],
        mut decode: impl FnMut(usize, &[u8], bool) -> RpcResult<R>,
    ) -> RpcResult<Vec<R>> {
        let byte_budget = self.read_compound_bytes().saturating_sub(128);
        let mut results = Vec::with_capacity(ops.len());
        let mut start = 0usize;
        while start < ops.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < ops.len() {
                let next = (ops[end].count as usize).saturating_add(128);
                if bytes.saturating_add(next) > byte_budget {
                    break;
                }
                bytes += next;
                end += 1;
            }
            if end == start {
                return Err(RpcError::op(start, nfsstat4_NFS4ERR_REP_TOO_BIG));
            }
            let mut extracted = 0usize;
            let chunk = self
                .batch_ops_fallible(
                    b"readv",
                    2,
                    &ops[start..end],
                    |c, op, _| {
                        c.putfh(&op.fh.as_nfs_fh());
                        c.read(&op.stateid, op.offset, op.count);
                    },
                    |res, i| {
                        let local_index = extracted;
                        let wire_index = start + local_index;
                        extracted += 1;
                        let ok = res.read(2 + 2 * i);
                        let len = ok.data.data_len as usize;
                        if len > ops[wire_index].count as usize {
                            return Err(RpcError::transport(
                                "NFS READ reply exceeded requested count",
                            )
                            .with_op_index(local_index));
                        }
                        let data = if len == 0 {
                            &[][..]
                        } else {
                            unsafe {
                                std::slice::from_raw_parts(ok.data.data_val as *const u8, len)
                            }
                        };
                        decode(wire_index, data, ok.eof != 0)
                            .map_err(|error| error.with_op_index(local_index))
                    },
                )
                .map_err(|error| {
                    let index = start + error.op_index;
                    error.with_op_index(index)
                })?;
            results.extend(chunk);
            start = end;
        }
        Ok(results)
    }

    /// WRITE several `[PUTFH, WRITE]` pairs in as few compounds as possible;
    /// returns (bytes written, commit mode) per request.
    pub fn writev(&mut self, ops: &[WriteOp<'_>]) -> RpcResult<Vec<(u32, u32)>> {
        self.batch_ops(
            b"writev",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.write(&op.stateid, op.offset, stable_how4_FILE_SYNC4, op.data);
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
        let mut start = 0usize;
        let mut max_items = usize::MAX;
        while start < names.len() {
            // SEQUENCE and PUTFH precede the first REMOVE. Keep each request
            // within the shape's learned operation budget.
            let take = self
                .remove_batch_capacity()
                .min(max_items)
                .min(names.len() - start);
            // Earlier chunks are confirmed; this chunk has not been sent.
            #[cfg(feature = "test-faults")]
            if let Some(injector) = &self.fault_injector {
                injector
                    .check(&OpenFaultPoint::BeforeRemoveChunk {
                        first_name: names[start].clone(),
                    })
                    .map_err(|error| {
                        if error.is_transport() {
                            RpcError::transport(error.to_string())
                        } else {
                            RpcError::op(start, error.err_no())
                        }
                    })?;
            }
            let mut c = Compound::new();
            c.tag(b"removev");
            c.putfh(&dir.as_nfs_fh());
            for name in &names[start..start + take] {
                c.remove(name);
            }
            if !self.compound_request_fits(&c)? && take > 1 {
                max_items = take / 2;
                continue;
            }
            max_items = usize::MAX;
            let map = |op_index: usize| start + remove_result_index(op_index, take);
            let res = self
                .call_compound_with_safety(&mut c, RequestSafety::NonIdempotentMutation)
                .map_err(|error| {
                    let index = map(error.op_index);
                    error.with_op_index(index)
                })?;
            if take > 1
                && resource_rejected_before_mutation(&res)
                && self.remove_batch_capacity() < take
            {
                continue;
            }
            self.session.expect_all_ok(&res).map_err(|error| {
                let index = map(error.op_index);
                error.with_op_index(index)
            })?;
            start += take;
        }
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
            OwnerSlot::User => {
                let verifier = self.session.open_owner.verifier;
                (0, verifier, self.next_public_open_owner_name())
            }
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
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid += 1;
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
        add: impl FnMut(&mut Compound, &T, usize),
        mut extract: impl FnMut(&CompoundRes, usize) -> R,
    ) -> RpcResult<Vec<R>> {
        self.batch_ops_fallible(tag, per_op, ops, add, |res, index| Ok(extract(res, index)))
    }

    fn batch_ops_fallible<T, R>(
        &mut self,
        tag: &[u8],
        per_op: usize,
        ops: &[T],
        mut add: impl FnMut(&mut Compound, &T, usize),
        mut extract: impl FnMut(&CompoundRes, usize) -> RpcResult<R>,
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
        let read_only = batch_is_read_only(tag);
        let mut out = Vec::with_capacity(ops.len());
        let mut global = 0usize;
        let mut max_items = usize::MAX;
        while global < ops.len() {
            let chunk_size = self.op_budget_for(tag, 1 + per_op).batch_capacity(per_op)?;
            let chunk = &ops[global..(global + chunk_size.min(max_items)).min(ops.len())];
            let chunk_start = global;
            let mut c = Compound::new();
            c.tag(tag);
            for op in chunk {
                add(&mut c, op, global);
                global += 1;
            }
            if !self.compound_request_fits(&c)? && chunk.len() > 1 {
                max_items = chunk.len() / 2;
                global = chunk_start;
                continue;
            }
            max_items = usize::MAX;
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
            let status = bad.map_or(res.status(), |i| res.op_status(i));
            if !read_only && chunk.len() > 1 && resource_rejected_before_mutation(&res) {
                // The server rejected this compound before its first PUTFH
                // completed, so no mutation can have run. The adaptive
                // budget learned from the rejection; resend only if it now
                // produces a smaller chunk. Never replay an uncertain OPEN,
                // WRITE, or completed prefix.
                let smaller = self.op_budget_for(tag, 1 + per_op).batch_capacity(per_op)?;
                if smaller < chunk.len() {
                    global = chunk_start;
                    continue;
                }
            }
            if let Some(completed) = batch_resource_progress(tag, per_op, status, bad)
                && chunk.len() > 1
            {
                let next_capacity = self.op_budget_for(tag, 1 + per_op).batch_capacity(per_op)?;
                if completed > 0 || next_capacity < chunk.len() {
                    for i in 0..completed {
                        out.push(extract(&res, i)?);
                    }
                    global = chunk_start + completed;
                    continue;
                }
            }
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
                out.push(extract(&res, i)?);
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

    pub fn getattr_many_with_bitmap(
        &mut self,
        ops: &[GetattrOp],
    ) -> RpcResult<Vec<(bitmap4, Vec<u8>)>> {
        self.batch_ops(
            b"vstatfs_impl",
            2,
            ops,
            |c, op, _| {
                c.putfh(&op.fh.as_nfs_fh());
                c.getattr(&op.attrs);
            },
            |res, i| res.getattr_with_bitmap(2 + 2 * i),
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
                c.setattr_ownership(
                    op.mode,
                    op.size,
                    (op.uid, op.gid),
                    op.atime,
                    op.mtime,
                    &stateid4 {
                        seqid: 0,
                        other: [0; 12],
                    },
                );
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
            OwnerSlot::User => (0, self.session.open_owner.verifier, Vec::new()),
            OwnerSlot::Path => (
                self.session.path_owner.seqid,
                self.session.path_owner.verifier,
                self.session.path_owner.name.clone(),
            ),
        };
        let n = ops.len();
        let public_owner_names: Vec<Vec<u8>> = if matches!(slot, OwnerSlot::User) {
            (0..n).map(|_| self.next_public_open_owner_name()).collect()
        } else {
            Vec::new()
        };
        let clientid = self.session.clientid;
        let out = self.batch_ops(
            b"openv",
            3,
            ops,
            |c, op, gi| {
                c.putfh(&op.dir.as_nfs_fh());
                let openhow = make_open_how(op.create, verifier);
                let (seqid, owner) = if matches!(slot, OwnerSlot::User) {
                    (0, &public_owner_names[gi])
                } else {
                    (base + gi as u32, &owner_name)
                };
                c.open_claim_null(
                    seqid,
                    op.access,
                    OPEN4_SHARE_DENY_NONE,
                    clientid,
                    owner,
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
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid = base + n as u32;
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
        #[cfg(feature = "test-faults")]
        if let Some(injector) = &self.fault_injector
            && let Err(error) = injector.check(&OpenFaultPoint::BeforePathCloseBatch)
        {
            return Err(RpcError::transport(error.to_string()));
        }
        self.close_many_slot(ops, OwnerSlot::Path)?;
        #[cfg(feature = "test-faults")]
        {
            self.confirmed_path_closes += ops.len();
        }
        Ok(())
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
        ops: &[PathWriteOp<'_>],
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
        let per_op = self.per_op_bytes();

        let mut global = 0usize;
        // Bytes of ops[global] already emitted across earlier compounds
        // (>0 means ops[global] is being continued mid-file).
        let mut part_off = 0usize;
        let mut max_items = usize::MAX;
        while global < n {
            let tag = if close_in_compound {
                b"writev1".as_slice()
            } else {
                b"writev2".as_slice()
            };
            let budget = self
                .op_budget_for(tag, 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let chunk_part_off = part_off;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(tag);
            let mut opened_path: Option<Vec<u8>> = None;
            let mut fh_at_opened = false;
            let mut opens_in_chunk = 0usize;
            let base_seq = self.session.path_owner.seqid;
            let mut payload = 0usize;

            while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                let op = &ops[global];
                let start = part_off;
                // Bound the next write window by both wire-op and request
                // payload budgets. A first operation may still make progress
                // when its byte estimate reaches the soft boundary.
                let room = if self.max_request_arg_bytes() > 0 {
                    if payload > 0 {
                        self.max_request_arg_bytes().saturating_sub(payload + 128)
                    } else {
                        self.max_request_arg_bytes()
                    }
                } else {
                    usize::MAX
                };
                let end = match path_io_chunk_end(
                    start,
                    op.data.len(),
                    per_op,
                    room,
                    budget.saturating_sub(map.next + 3),
                    map.next == 0 && payload == 0,
                ) {
                    Some(end) => end,
                    None => break,
                };
                map.begin(global);
                let newly_opened = match self.prepare_path_io_target(
                    &mut PathIoTarget {
                        compound: &mut c,
                        cursor: &mut cursor,
                        map: &mut map,
                        close_in_compound,
                        opened_path: &mut opened_path,
                        fh_at_opened: &mut fh_at_opened,
                        base_seq,
                        opens_in_chunk: &mut opens_in_chunk,
                    },
                    &op.file,
                    PathIoOpen {
                        access: OPEN4_SHARE_ACCESS_BOTH,
                        create: if op.create {
                            OpenCreate::Unchecked
                        } else {
                            OpenCreate::NoCreate
                        },
                        truncate_create: op.create && op.truncate && start == 0,
                        truncate_existing: op.truncate && !op.create && start == 0,
                    },
                ) {
                    Some(opened) => opened,
                    None => {
                        failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                        global = n;
                        break;
                    }
                };
                let stateid = match &op.file {
                    FileRef::Path(_) => &SPECIAL_STATEID,
                    FileRef::Handle(_) => op.stateid.as_ref().unwrap_or(&SPECIAL_STATEID),
                };
                if end > start {
                    let mut off = 0usize;
                    for chunk in op.data[start..end].chunks(per_op) {
                        c.write(
                            stateid,
                            checked_offset(op.offset, start + off, global)?,
                            stable_how4_FILE_SYNC4,
                            chunk,
                        );
                        map.note_ops(1);
                        off += chunk.len();
                    }
                } else {
                    // A zero-length write still emits a WRITE for this item.
                    c.write(
                        stateid,
                        checked_offset(op.offset, start, global)?,
                        stable_how4_FILE_SYNC4,
                        &[],
                    );
                    map.note_ops(1);
                }
                if newly_opened {
                    cursor.descend();
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
            let res = match self.send_path_io_chunk(PathIoChunk {
                tag,
                one_item_ops: 1 + per_file,
                reserve,
                per_file,
                old_budget: budget,
                allow_repack: failed.is_none(),
                compound: &mut c,
                map: &map,
                base_seq,
                opens_in_chunk,
                safety: RequestSafety::NonIdempotentMutation,
            })? {
                PathIoChunkResult::Repack { max_items: cap } => {
                    max_items = cap;
                    global = chunk_start;
                    part_off = chunk_part_off;
                    continue;
                }
                PathIoChunkResult::Retry => {
                    max_items = usize::MAX;
                    global = chunk_start;
                    part_off = chunk_part_off;
                    continue;
                }
                PathIoChunkResult::Response(response) => {
                    max_items = usize::MAX;
                    response
                }
            };
            let report = map.analyze(&res).map_err(|error| {
                RpcError::transport(format!("malformed writev COMPOUND reply: {error}"))
            })?;
            let range_failed = report
                .failure
                .map(|failure| (failure.caller, failure.status));
            if let Some((caller, st)) = range_failed {
                failed = Some((caller, st));
                for i in caller + 1..n {
                    counts[i] = None;
                    committed[i] = None;
                }
            }
            // Extract results and opened stateids from the resarray.
            for (caller, s, e) in &map.ranges {
                if !report.completed.contains(caller) {
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
            if let Some(status) = report.compound_failure {
                let close_op = map.ranges.last().map(|(_, _, end)| *end);
                if close_in_compound
                    && opened_path.is_some()
                    && report.compound_failure_op == close_op
                {
                    close_failed = Some(status);
                } else {
                    return Err(RpcError::op(0, status));
                }
            }
            if failed.is_some() || close_failed.is_some() {
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
        let per_op = self.read_per_op_bytes();

        let mut global = 0usize;
        // Bytes of ops[global] already fetched across earlier compounds
        // (>0 means ops[global] is being continued mid-file).
        let mut part_off = 0usize;
        let mut max_items = usize::MAX;
        while global < n {
            let tag = if close_in_compound {
                b"readv1".as_slice()
            } else {
                b"readv2".as_slice()
            };
            let budget = self
                .op_budget_for(tag, 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let chunk_part_off = part_off;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(tag);
            let mut opened_path: Option<Vec<u8>> = None;
            let mut fh_at_opened = false;
            let mut opens_in_chunk = 0usize;
            let base_seq = self.session.path_owner.seqid;
            let mut payload = 0usize;

            while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                let op = &ops[global];
                let start = part_off;
                // Leave reply-array overhead in the response byte budget.
                let room = if self.max_response_bytes > 0 {
                    if payload > 0 {
                        self.read_compound_bytes().saturating_sub(payload + 128)
                    } else {
                        self.read_compound_bytes()
                    }
                } else {
                    usize::MAX
                };
                let end = match path_io_chunk_end(
                    start,
                    op.count,
                    per_op,
                    room,
                    budget.saturating_sub(map.next + 3),
                    map.next == 0 && payload == 0,
                ) {
                    Some(end) => end,
                    None => break,
                };
                map.begin(global);
                let newly_opened = match self.prepare_path_io_target(
                    &mut PathIoTarget {
                        compound: &mut c,
                        cursor: &mut cursor,
                        map: &mut map,
                        close_in_compound,
                        opened_path: &mut opened_path,
                        fh_at_opened: &mut fh_at_opened,
                        base_seq,
                        opens_in_chunk: &mut opens_in_chunk,
                    },
                    &op.file,
                    PathIoOpen {
                        access: OPEN4_SHARE_ACCESS_READ,
                        create: OpenCreate::NoCreate,
                        truncate_create: false,
                        truncate_existing: false,
                    },
                ) {
                    Some(opened) => opened,
                    None => {
                        failed = Some((global, nfsstat4_NFS4ERR_INVAL));
                        global = n;
                        break;
                    }
                };
                let stateid = match &op.file {
                    FileRef::Path(_) => &SPECIAL_STATEID,
                    FileRef::Handle(_) => op.stateid.as_ref().unwrap_or(&SPECIAL_STATEID),
                };
                if end > start {
                    let mut off = 0usize;
                    for chunk_len in chunk_lens(start, end, per_op) {
                        c.read(
                            stateid,
                            checked_offset(op.offset, start + off, global)?,
                            chunk_len as u32,
                        );
                        map.note_ops(1);
                        off += chunk_len;
                    }
                } else {
                    // Empty reads still emit a READ to preserve result order.
                    c.read(stateid, checked_offset(op.offset, start, global)?, 0);
                    map.note_ops(1);
                }
                if newly_opened {
                    cursor.descend();
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
            let res = match self.send_path_io_chunk(PathIoChunk {
                tag,
                one_item_ops: 1 + per_file,
                reserve,
                per_file,
                old_budget: budget,
                allow_repack: failed.is_none(),
                compound: &mut c,
                map: &map,
                base_seq,
                opens_in_chunk,
                safety: RequestSafety::ReadOnly,
            })? {
                PathIoChunkResult::Repack { max_items: cap } => {
                    max_items = cap;
                    global = chunk_start;
                    part_off = chunk_part_off;
                    continue;
                }
                PathIoChunkResult::Retry => {
                    max_items = usize::MAX;
                    global = chunk_start;
                    part_off = chunk_part_off;
                    continue;
                }
                PathIoChunkResult::Response(response) => {
                    max_items = usize::MAX;
                    response
                }
            };
            let report = map.analyze(&res).map_err(|error| {
                RpcError::transport(format!("malformed readv COMPOUND reply: {error}"))
            })?;
            let range_failed = report
                .failure
                .map(|failure| (failure.caller, failure.status));
            if let Some((caller, st)) = range_failed {
                failed = Some((caller, st));
                for i in caller + 1..n {
                    data[i] = None;
                    eof[i] = None;
                }
            }
            for (caller, s, e) in &map.ranges {
                if !report.completed.contains(caller) {
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
            if let Some(status) = report.compound_failure {
                let close_op = map.ranges.last().map(|(_, _, end)| *end);
                if close_in_compound
                    && opened_path.is_some()
                    && report.compound_failure_op == close_op
                {
                    close_failed = Some(status);
                } else {
                    return Err(RpcError::op(0, status));
                }
            }
            if failed.is_some() || close_failed.is_some() {
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
        let mut global = 0usize;
        let mut max_items = usize::MAX;
        while global < n {
            let budget = self
                .op_budget_for(b"getattrv1", 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"getattrv1");
            let mut payload = 0usize;
            while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                let op = &ops[global];
                let est = 256;
                if payload > 0
                    && self.max_request_arg_bytes() > 0
                    && payload + est > self.max_request_arg_bytes()
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
            if failed.is_none()
                && self.merged_paths_need_repack(
                    b"getattrv1",
                    1 + per_file,
                    &c,
                    map.ranges.len(),
                )?
            {
                max_items = map.ranges.len() - 1;
                global = chunk_start;
                continue;
            }
            max_items = usize::MAX;
            let res = self.call_compound_with_safety(&mut c, RequestSafety::IdempotentMutation)?;
            if self.can_retry_read_only_merged_resource(
                b"getattrv1",
                &res,
                budget,
                reserve,
                per_file,
            ) {
                global = chunk_start;
                continue;
            }
            if let Some((caller, st)) = first_failed_range(&res, &map)? {
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
        let mut global = 0usize;
        let mut max_items = usize::MAX;
        while global < n {
            let budget = self
                .op_budget_for(b"setattrv1", 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"setattrv1");
            let mut payload = 0usize;
            while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                let op = &ops[global];
                let est = 256;
                if payload > 0
                    && self.max_request_arg_bytes() > 0
                    && payload + est > self.max_request_arg_bytes()
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
                c.setattr_ownership(
                    op.mode,
                    op.size,
                    (op.uid, op.gid),
                    op.atime,
                    op.mtime,
                    &stateid4 {
                        seqid: 0,
                        other: [0; 12],
                    },
                );
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
            if failed.is_none()
                && self.merged_paths_need_repack(
                    b"setattrv1",
                    1 + per_file,
                    &c,
                    map.ranges.len(),
                )?
            {
                max_items = map.ranges.len() - 1;
                global = chunk_start;
                continue;
            }
            max_items = usize::MAX;
            let res = self.call_compound(&mut c)?;
            if self.can_retry_merged_resource(b"setattrv1", &res, budget, reserve, per_file) {
                global = chunk_start;
                continue;
            }
            if let Some((caller, st)) = first_failed_range(&res, &map)? {
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
        self.drain_deferred_path_closes()?;
        let public_owner_names: Vec<Vec<u8>> =
            (0..n).map(|_| self.next_public_open_owner_name()).collect();
        let mut opened: Vec<Option<(FileHandle, stateid4)>> = vec![None; n];
        let mut failed: Option<(usize, u32)> = None;
        let per_file = 6; // RESTOREFH + OPEN + GETFH + [SETATTR x2] + margin
        let reserve = 16;
        let mut global = 0usize;
        let mut max_items = usize::MAX;
        #[cfg(feature = "test-faults")]
        let mut chunk_index = 0usize;
        while global < n {
            let budget = self
                .op_budget_for(b"openv1", 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"openv1");
            let mut payload = 0usize;
            while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                let op = &ops[global];
                let est = 256;
                if payload > 0
                    && self.max_request_arg_bytes() > 0
                    && payload + est > self.max_request_arg_bytes()
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
                            0,
                            op.access,
                            OPEN4_SHARE_DENY_NONE,
                            self.session.clientid,
                            &public_owner_names[global],
                            &leaf,
                            Some(mode),
                            op.truncate,
                        );
                    }
                    create => c.open_claim_null(
                        0,
                        op.access,
                        OPEN4_SHARE_DENY_NONE,
                        self.session.clientid,
                        &public_owner_names[global],
                        make_open_how(create, self.session.path_owner.verifier),
                        &leaf,
                    ),
                }
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
            if failed.is_none()
                && self.merged_paths_need_repack(b"openv1", 1 + per_file, &c, map.ranges.len())?
            {
                max_items = map.ranges.len() - 1;
                global = chunk_start;
                continue;
            }
            CompoundBudget::new(self.max_ops).ensure(&c)?;
            max_items = usize::MAX;
            #[cfg(feature = "test-faults")]
            if let Some(injector) = self.fault_injector.clone()
                && let Err(error) =
                    injector.check(&OpenFaultPoint::BeforeOpenChunk { chunk: chunk_index })
            {
                let closes: Vec<CloseOp> = opened
                    .iter_mut()
                    .filter_map(Option::take)
                    .map(|(fh, stateid)| CloseOp { fh, stateid })
                    .collect();
                self.close_path_or_defer(closes);
                return Err(RpcError::transport(error.to_string()));
            }
            let res = match self
                .call_compound_with_safety(&mut c, RequestSafety::NonIdempotentMutation)
            {
                Ok(response) => response,
                Err(error) => {
                    let closes: Vec<CloseOp> = opened
                        .iter_mut()
                        .filter_map(Option::take)
                        .map(|(fh, stateid)| CloseOp { fh, stateid })
                        .collect();
                    self.close_path_or_defer(closes);
                    return Err(error);
                }
            };
            if resource_rejected_before_mutation(&res)
                && self.can_retry_merged_resource(b"openv1", &res, budget, reserve, per_file)
            {
                global = chunk_start;
                continue;
            }
            if let Some((caller, st)) = first_failed_range(&res, &map)? {
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
            #[cfg(feature = "test-faults")]
            {
                chunk_index += 1;
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
        let mut global = 0usize;
        let mut max_items = usize::MAX;
        while global < n {
            let budget = self
                .op_budget_for(b"removev1", 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"removev1");
            let mut payload = 0usize;
            while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                let est = 128;
                if payload > 0
                    && self.max_request_arg_bytes() > 0
                    && payload + est > self.max_request_arg_bytes()
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
            if failed.is_none()
                && self.merged_paths_need_repack(b"removev1", 1 + per_file, &c, map.ranges.len())?
            {
                max_items = map.ranges.len() - 1;
                global = chunk_start;
                continue;
            }
            max_items = usize::MAX;
            let res =
                self.call_compound_with_safety(&mut c, RequestSafety::NonIdempotentMutation)?;
            if self.can_retry_merged_resource(b"removev1", &res, budget, reserve, per_file) {
                global = chunk_start;
                continue;
            }
            let done = first_failed_range(&res, &map)?;
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
        let mut global = 0usize;
        let mut max_items = usize::MAX;
        while global < n {
            let budget = self
                .op_budget_for(b"renamev1", 1 + per_file)
                .merged_limit(reserve, per_file);
            let chunk_start = global;
            let mut cursor = CfhCursor::default();
            let mut map = ExecutionMap::new();
            let mut c = Compound::new();
            c.tag(b"renamev1");
            let mut payload = 0usize;
            while global < n && map.ranges.len() < max_items && map.next + per_file <= budget {
                let pair = &pairs[global];
                let est = 256;
                if payload > 0
                    && self.max_request_arg_bytes() > 0
                    && payload + est > self.max_request_arg_bytes()
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
            if failed.is_none()
                && self.merged_paths_need_repack(b"renamev1", 1 + per_file, &c, map.ranges.len())?
            {
                max_items = map.ranges.len() - 1;
                global = chunk_start;
                continue;
            }
            max_items = usize::MAX;
            let res =
                self.call_compound_with_safety(&mut c, RequestSafety::NonIdempotentMutation)?;
            if self.can_retry_merged_resource(b"renamev1", &res, budget, reserve, per_file) {
                global = chunk_start;
                continue;
            }
            let done = first_failed_range(&res, &map)?;
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
            OwnerSlot::User => 1,
            OwnerSlot::Path => self.session.path_owner.seqid,
        };
        let n = ops.len();
        let _ = self.batch_ops::<CloseOp, ()>(
            b"closev",
            2,
            ops,
            |c, op, gi| {
                c.putfh(&op.fh.as_nfs_fh());
                let seqid = if matches!(slot, OwnerSlot::User) {
                    base
                } else {
                    base + gi as u32
                };
                c.close(seqid, &op.stateid);
            },
            |_, _| (),
        )?;
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid = base + n as u32;
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
        let count = count.min(self.read_per_op_bytes() as u32);
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
        #[cfg(feature = "test-faults")]
        if let Some(injector) = &self.fault_injector
            && let Err(error) = injector.check(&OpenFaultPoint::BeforePathClose)
        {
            return Err(RpcError::transport(error.to_string()));
        }
        self.close_slot(fh, stateid, OwnerSlot::Path)?;
        #[cfg(feature = "test-faults")]
        {
            self.confirmed_path_closes += 1;
        }
        Ok(())
    }

    fn close_slot(
        &mut self,
        fh: &FileHandle,
        stateid: &stateid4,
        slot: OwnerSlot,
    ) -> RpcResult<()> {
        let seqid = match slot {
            OwnerSlot::User => 1,
            OwnerSlot::Path => self.session.path_owner.seqid,
        };
        let mut c = Compound::new();
        c.tag(b"close");
        c.putfh(&fh.as_nfs_fh());
        c.close(seqid, stateid);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        if matches!(slot, OwnerSlot::Path) {
            self.session.path_owner.seqid += 1;
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

    /// SETATTR mode, size, and/or access/modify timestamps on `fh`.
    pub fn setattr_values(
        &mut self,
        fh: &FileHandle,
        mode: Option<u32>,
        size: Option<u64>,
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
    ) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"setattr-values");
        c.putfh(&fh.as_nfs_fh());
        c.setattr_values(
            mode,
            size,
            atime,
            mtime,
            &stateid4 {
                seqid: 0,
                other: [0; 12],
            },
        );
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(())
    }

    /// SETATTR including numeric owner/group IDs.
    pub fn setattr_ownership(
        &mut self,
        fh: &FileHandle,
        mode: Option<u32>,
        size: Option<u64>,
        ownership: (Option<u32>, Option<u32>),
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
    ) -> RpcResult<()> {
        let mut c = Compound::new();
        c.tag(b"setattr-ownership");
        c.putfh(&fh.as_nfs_fh());
        c.setattr_ownership(
            mode,
            size,
            ownership,
            atime,
            mtime,
            &stateid4 {
                seqid: 0,
                other: [0; 12],
            },
        );
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
        self.readdir_bounded(dir, cookie, attrs, usize::MAX)
    }

    /// READDIR with a caller-imposed reply budget. The server-negotiated
    /// response limit remains the upper bound; tiny requested limits are
    /// raised to 4 KiB so a valid reply can hold at least one entry.
    pub fn readdir_bounded(
        &mut self,
        dir: &FileHandle,
        cookie: u64,
        attrs: &[u32],
        max_response_bytes: usize,
    ) -> RpcResult<Vec<DirEntry>> {
        let mut c = Compound::new();
        c.tag(b"readdir");
        c.putfh(&dir.as_nfs_fh());
        let zeroverf: verifier4 = [0; 8];
        let (dircount, maxcount) = self.readdir_limits(1);
        let maxcount = maxcount.min(max_response_bytes.max(4096).min(u32::MAX as usize) as u32);
        let dircount = dircount.min(maxcount / 4).max(1);
        c.readdir(cookie, &zeroverf, dircount, maxcount, attrs);
        let res = self.call_compound(&mut c)?;
        self.session.expect_all_ok(&res)?;
        Ok(Self::collect_readdir(res.readdir(2))?.0)
    }

    /// Extract the entries and the next cookie from a decoded READDIR reply.
    fn collect_readdir(ok: &READDIR4resok) -> RpcResult<(Vec<DirEntry>, u64)> {
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
        // EOF is authoritative even when this final page contains entries.
        // Zero is the internal "finished" marker, not a request to restart.
        if ok.reply.eof == 0 && cookie == 0 {
            return Err(RpcError::transport(
                "READDIR page has no continuation progress",
            ));
        }
        Ok((out, if ok.reply.eof != 0 { 0 } else { cookie }))
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
        self.readdir_children_bounded(ops, attrs, usize::MAX)
    }

    pub(crate) fn readdir_children_bounded(
        &mut self,
        ops: &[(FileHandle, Vec<u8>)],
        attrs: &[u32],
        max_page_bytes: usize,
    ) -> RpcResult<Vec<ChildListing>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(ops.len());
        let zeroverf: verifier4 = [0; 8];
        while out.len() < ops.len() {
            let per_chunk = self
                .op_budget_for(b"readdir_children", 5)
                .batch_capacity(4)?;
            let chunk = &ops[out.len()..(out.len() + per_chunk).min(ops.len())];
            let (dircount, maxcount) = self.readdir_limits(chunk.len());
            let maxcount = maxcount.min(max_page_bytes.max(4096).min(u32::MAX as usize) as u32);
            let dircount = dircount.min(maxcount / 4).max(1);
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
            if chunk.len() > 1
                && matches!(
                    res.status(),
                    nfsstat4_NFS4ERR_RESOURCE | nfsstat4_NFS4ERR_TOO_MANY_OPS
                )
                && self
                    .op_budget_for(b"readdir_children", 5)
                    .batch_capacity(4)?
                    < chunk.len()
            {
                continue;
            }
            self.session.expect_all_ok(&res).map_err(|e| {
                let idx = map(e.op_index);
                e.with_op_index(idx)
            })?;
            for (i, _) in chunk.iter().enumerate() {
                let fh = res.getfh(3 + 4 * i);
                let (entries, cookie) = Self::collect_readdir(res.readdir(4 + 4 * i))?;
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
        self.readdir_pages_bounded(ops, attrs, usize::MAX)
    }

    /// One bounded wire page per directory; negotiated response limits still apply.
    pub(crate) fn readdir_pages_bounded(
        &mut self,
        ops: &[(FileHandle, u64)],
        attrs: &[u32],
        max_page_bytes: usize,
    ) -> RpcResult<Vec<(Vec<DirEntry>, u64)>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(ops.len());
        let zeroverf: verifier4 = [0; 8];
        while out.len() < ops.len() {
            let per_chunk = self.op_budget_for(b"readdir_pages", 3).batch_capacity(2)?;
            let chunk = &ops[out.len()..(out.len() + per_chunk).min(ops.len())];
            let (dircount, maxcount) = self.readdir_limits(chunk.len());
            let maxcount = maxcount.min(max_page_bytes.max(4096).min(u32::MAX as usize) as u32);
            let dircount = dircount.min(maxcount / 4).max(1);
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
            if chunk.len() > 1
                && matches!(
                    res.status(),
                    nfsstat4_NFS4ERR_RESOURCE | nfsstat4_NFS4ERR_TOO_MANY_OPS
                )
                && self.op_budget_for(b"readdir_pages", 3).batch_capacity(2)? < chunk.len()
            {
                continue;
            }
            self.session.expect_all_ok(&res).map_err(|e| {
                let idx = map(e.op_index);
                e.with_op_index(idx)
            })?;
            for (i, _) in chunk.iter().enumerate() {
                let (entries, cookie) = Self::collect_readdir(res.readdir(2 + 2 * i))?;
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
    #[test]
    fn statistics_resource_retries_preserve_complete_items() {
        use nfsv41_sys::{nfsstat4_NFS4ERR_RESOURCE, nfsstat4_NFS4ERR_TOO_MANY_OPS};
        for status in [nfsstat4_NFS4ERR_RESOURCE, nfsstat4_NFS4ERR_TOO_MANY_OPS] {
            // Two PUTFH/GETATTR pairs succeeded. The next PUTFH or GETATTR failed.
            for failed_op in [5, 6] {
                assert_eq!(
                    super::batch_resource_progress(b"vstatfs_impl", 2, status, Some(failed_op)),
                    Some(2)
                );
            }
            // Failure on the first GETATTR retains no incomplete item.
            assert_eq!(
                super::batch_resource_progress(b"vstatfs_impl", 2, status, Some(2)),
                Some(0)
            );
            assert_eq!(
                super::batch_resource_progress(b"vstatfs_impl", 2, status, None),
                Some(0)
            );
            // Never enable this retry path for a mutation with partial progress.
            assert_eq!(
                super::batch_resource_progress(b"setattrv", 2, status, Some(6)),
                None
            );
        }
        assert_eq!(
            super::batch_resource_progress(
                b"vstatfs_impl",
                2,
                nfsv41_sys::nfsstat4_NFS4ERR_ACCESS,
                Some(6)
            ),
            None
        );
    }

    #[test]
    fn directory_page_eof_suppresses_continuation_without_rewriting_entry_cookies() {
        let mut entry: nfsv41_sys::entry4 = unsafe { std::mem::zeroed() };
        entry.cookie = 42;
        let mut reply: nfsv41_sys::READDIR4resok = unsafe { std::mem::zeroed() };
        reply.reply.entries = &mut entry;
        reply.reply.eof = 1;
        let (entries, next) = super::NfsClient::collect_readdir(&reply).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cookie, 42);
        assert_eq!(next, 0);
        reply.reply.eof = 0;
        assert_eq!(super::NfsClient::collect_readdir(&reply).unwrap().1, 42);
        reply.reply.entries = std::ptr::null_mut();
        assert!(
            super::NfsClient::collect_readdir(&reply)
                .unwrap_err()
                .is_transport()
        );
    }

    use proptest::prelude::*;

    use super::*;

    fn offset_boundary() -> impl Strategy<Value = u64> {
        prop_oneof![
            0u64..=2048,
            (i64::MAX as u64 - 1024)..=(i64::MAX as u64 + 1024),
            (u64::MAX - 2048)..=u64::MAX,
        ]
    }

    proptest! {
        #[test]
        fn checked_offsets_match_u64_arithmetic_at_signed_and_unsigned_boundaries(
            base in offset_boundary(),
            delta in 0usize..=4096,
            index in 0usize..32,
        ) {
            match (base.checked_add(delta as u64), checked_offset(base, delta, index)) {
                (Some(expected), Ok(actual)) => prop_assert_eq!(actual, expected),
                (None, Err(error)) => {
                    prop_assert_eq!(error.op_index, index);
                    prop_assert_eq!(error.status, nfsstat4_NFS4ERR_INVAL);
                }
                (expected, actual) => prop_assert!(false, "expected {expected:?}, got {actual:?}"),
            }
        }
    }

    #[test]
    fn negotiated_op_budgets_include_sequence() {
        assert_eq!(CompoundBudget::new(4).batch_capacity(3).unwrap(), 1);
        assert!(CompoundBudget::new(4).batch_capacity(4).is_err());
        assert_eq!(CompoundBudget::new(8).batch_capacity(4).unwrap(), 1);
        assert_eq!(CompoundBudget::new(32).batch_capacity(4).unwrap(), 7);
    }

    #[test]
    fn remove_failure_index_counts_only_remove_items() {
        assert_eq!(remove_result_index(0, 4), 0); // SEQUENCE
        assert_eq!(remove_result_index(1, 4), 0); // PUTFH
        assert_eq!(remove_result_index(2, 4), 0); // first REMOVE
        assert_eq!(remove_result_index(3, 4), 1);
        assert_eq!(remove_result_index(5, 4), 3);
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
            effective_request_limit(below_floor, std::num::NonZeroUsize::new(4 * 1024 * 1024)),
            below_floor
        );
        assert_eq!(response_payload_budget(below_floor), 24 * 1024);
    }

    #[test]
    fn zero_configuration_cannot_bypass_negotiated_limit() {
        let negotiated = 48 * 1024;
        assert_eq!(effective_request_limit(negotiated, None), negotiated);
        assert_eq!(
            effective_request_limit(negotiated, std::num::NonZeroUsize::new(96 * 1024)),
            negotiated
        );
        assert_eq!(
            effective_request_limit(negotiated, std::num::NonZeroUsize::new(16 * 1024)),
            16 * 1024
        );
    }
}
